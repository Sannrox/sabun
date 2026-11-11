//! Z-sets: collections where every row carries a signed count.
//!
//! A plain set can only say "this row is here". A Z-set says "this row is here
//! `n` times", and `n` may be negative. That one extension lets the same type
//! describe both data and a change to data:
//!
//! - a table is a Z-set where every weight is positive;
//! - an insert is a Z-set with weight `+1`;
//! - a delete is a Z-set with weight `-1`;
//! - an update is a delete of the old row plus an insert of the new one.
//!
//! Applying a change is addition: `new_table = old_table + change`.

use std::collections::BTreeMap;
use std::ops::{Add, Neg};

/// Rows of type `T`, each with a non-zero signed weight.
///
/// Invariant: no row is stored with weight zero. A row whose inserts and
/// deletes cancel out disappears, so two Z-sets with the same content are
/// always equal. A `BTreeMap` keeps iteration order deterministic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZSet<T: Ord> {
    weights: BTreeMap<T, i64>,
}

impl<T: Ord> Default for ZSet<T> {
    fn default() -> Self {
        Self {
            weights: BTreeMap::new(),
        }
    }
}

impl<T: Ord> ZSet<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `weight` to the row's count, dropping the row if it reaches zero.
    pub fn add_row(&mut self, row: T, weight: i64) {
        if weight == 0 {
            return;
        }
        match self.weights.entry(row) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(weight);
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                *slot.get_mut() += weight;
                if *slot.get() == 0 {
                    slot.remove();
                }
            }
        }
    }

    /// The row's count, zero if absent.
    pub fn weight(&self, row: &T) -> i64 {
        self.weights.get(row).copied().unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.weights.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&T, i64)> {
        self.weights.iter().map(|(row, weight)| (row, *weight))
    }

    /// Transforms every row, keeping its weight. Two rows that map to the same
    /// output have their weights added, which is why an insert and a delete
    /// that land on the same output cancel.
    pub fn map<U: Ord>(&self, f: impl Fn(&T) -> U) -> ZSet<U> {
        self.iter().map(|(row, weight)| (f(row), weight)).collect()
    }

    /// Keeps the rows the predicate accepts, with their weights.
    pub fn filter(&self, keep: impl Fn(&T) -> bool) -> ZSet<T>
    where
        T: Clone,
    {
        self.iter()
            .filter(|(row, _)| keep(row))
            .map(|(row, weight)| (row.clone(), weight))
            .collect()
    }
}

impl<T: Ord> FromIterator<(T, i64)> for ZSet<T> {
    fn from_iter<I: IntoIterator<Item = (T, i64)>>(rows: I) -> Self {
        let mut set = Self::new();
        for (row, weight) in rows {
            set.add_row(row, weight);
        }
        set
    }
}

impl<T: Ord> Add for ZSet<T> {
    type Output = ZSet<T>;

    fn add(mut self, other: ZSet<T>) -> ZSet<T> {
        for (row, weight) in other.weights {
            self.add_row(row, weight);
        }
        self
    }
}

impl<T: Ord> Neg for ZSet<T> {
    type Output = ZSet<T>;

    /// Turns every insert into a delete and back: `a + (-a)` is empty.
    fn neg(mut self) -> ZSet<T> {
        for weight in self.weights.values_mut() {
            *weight = -*weight;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows<T: Ord>(rows: impl IntoIterator<Item = (T, i64)>) -> ZSet<T> {
        rows.into_iter().collect()
    }

    #[test]
    fn an_insert_and_a_delete_of_the_same_row_cancel() {
        let table = rows([("a", 1), ("b", 1)]);
        let change = rows([("b", -1), ("c", 1)]);
        assert_eq!(table + change, rows([("a", 1), ("c", 1)]));
    }

    #[test]
    fn a_collection_plus_its_negation_is_empty() {
        let table = rows([("a", 2), ("b", 1)]);
        assert!((table.clone() + -table).is_empty());
    }

    /// The property the engine is built on. For `map` and `filter`, running the
    /// query on the changed table gives the same answer as running it on the
    /// old table and on the change separately, then adding the results. So the
    /// old table never has to be read again: the change alone is enough.
    #[test]
    fn map_and_filter_can_be_run_on_the_change_alone() {
        // (order id, amount in cents)
        let table = rows([((1, 500), 1), ((2, 80), 1), ((3, 1200), 1)]);
        // Order 2 is corrected from 80 to 800, order 4 arrives.
        let change = rows([((2, 80), -1), ((2, 800), 1), ((4, 30), 1)]);

        let large_amounts = |set: &ZSet<(u32, u32)>| {
            set.filter(|(_, amount)| *amount >= 500)
                .map(|(_, amount)| *amount)
        };

        let recomputed = large_amounts(&(table.clone() + change.clone()));
        let incremental = large_amounts(&table) + large_amounts(&change);

        assert_eq!(recomputed, incremental);
        assert_eq!(large_amounts(&change), rows([(800, 1)]));
    }

    #[test]
    fn rows_that_map_to_the_same_output_add_their_weights() {
        let cities = rows([
            (("ada", "Wien"), 1),
            (("bo", "Wien"), 1),
            (("cy", "Lyon"), 1),
        ]);
        assert_eq!(
            cities.map(|(_, city)| *city),
            rows([("Wien", 2), ("Lyon", 1)])
        );
    }
}
