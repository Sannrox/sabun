//! Per-key aggregates that report an update as a retraction plus an insert.
//!
//! The output of an aggregate is not "the total is now 800". It is a delete of
//! the old group result and an insert of the new one. Operators further down
//! stay correct because they see that pair, the same way they see any other
//! change.
//!
//! `count` and `sum` keep a small accumulator per group and update it from the
//! change alone. `min` cannot: deleting the current minimum means looking at
//! every value still in the group, so it stores the values with their counts
//! in an ordered map.

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::fmt;
use std::marker::PhantomData;

use crate::circuit::Operator;
use crate::zset::ZSet;

/// An accumulator would wrap, or a group's row count would go negative.
///
/// The operator leaves its state unchanged so the caller can retry or reject
/// the step without double-applying a partial update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Overflow,
    NegativeCount,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => f.write_str("aggregate accumulator overflowed"),
            Self::NegativeCount => f.write_str("aggregate row count went negative"),
        }
    }
}

impl StdError for Error {}

/// How many rows sit in each group.
///
/// The stored number is the aggregate itself. When it reaches zero the group
/// is forgotten: a count of zero is not a group.
#[derive(Debug, Clone)]
pub struct Count<K: Ord, V: Ord> {
    groups: BTreeMap<K, i64>,
    _value: PhantomData<V>,
}

impl<K: Ord, V: Ord> Default for Count<K, V> {
    fn default() -> Self {
        Self {
            groups: BTreeMap::new(),
            _value: PhantomData,
        }
    }
}

impl<K: Ord, V: Ord> Count<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this key still has a group.
    pub fn remembers(&self, key: &K) -> bool {
        self.groups.contains_key(key)
    }
}

impl<K: Clone + Ord, V: Ord> Count<K, V> {
    /// Feeds one change and returns the change to `(key, count)`.
    pub fn step(&mut self, change: ZSet<(K, V)>) -> Result<ZSet<(K, i64)>, Error> {
        let mut delta: BTreeMap<K, i64> = BTreeMap::new();
        for ((key, _), weight) in change.iter() {
            add_i64(delta.entry(key.clone()).or_insert(0), weight)?;
        }

        let mut out = ZSet::new();
        let mut next: BTreeMap<K, Option<i64>> = BTreeMap::new();
        for (key, d) in delta {
            let old = self.groups.get(&key).copied();
            let new_count = checked_add(old.unwrap_or(0), d)?;
            if new_count < 0 {
                return Err(Error::NegativeCount);
            }
            let new = (new_count != 0).then_some(new_count);
            emit(&mut out, &key, old, new);
            next.insert(key, new);
        }
        for (key, new) in next {
            match new {
                Some(count) => {
                    self.groups.insert(key, count);
                }
                None => {
                    self.groups.remove(&key);
                }
            }
        }
        Ok(out)
    }
}

impl<K, V> Operator for Count<K, V>
where
    K: Clone + Ord,
    V: Ord,
{
    type In = (K, V);
    type Out = (K, i64);
    type Error = Error;

    fn step(&mut self, change: ZSet<(K, V)>) -> Result<ZSet<(K, i64)>, Error> {
        Count::step(self, change)
    }
}

/// The total of the values in each group.
///
/// The total alone cannot tell an empty group from a group whose values sum
/// to zero, so the accumulator also holds a row count. Empty groups are
/// forgotten; a stored total of zero still has rows.
#[derive(Debug, Clone)]
pub struct Sum<K: Ord, V: Ord> {
    groups: BTreeMap<K, SumGroup>,
    _value: PhantomData<V>,
}

impl<K: Ord, V: Ord> Default for Sum<K, V> {
    fn default() -> Self {
        Self {
            groups: BTreeMap::new(),
            _value: PhantomData,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SumGroup {
    rows: i64,
    total: i64,
}

impl<K: Ord, V: Ord> Sum<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this key still has a group.
    pub fn remembers(&self, key: &K) -> bool {
        self.groups.contains_key(key)
    }
}

impl<K, V> Sum<K, V>
where
    K: Clone + Ord,
    V: Copy + Ord + Into<i64>,
{
    /// Feeds one change and returns the change to `(key, total)`.
    ///
    /// Value times weight is accumulated in `i128` so deleting a stored
    /// `i64::MIN` is allowed; only a result that cannot be an `i64` is
    /// [`Error::Overflow`].
    pub fn step(&mut self, change: ZSet<(K, V)>) -> Result<ZSet<(K, i64)>, Error> {
        let mut delta: BTreeMap<K, (i64, i128)> = BTreeMap::new();
        for ((key, value), weight) in change.iter() {
            let slot = delta.entry(key.clone()).or_insert((0, 0));
            add_i64(&mut slot.0, weight)?;
            slot.1 += i128::from((*value).into()) * i128::from(weight);
        }

        let mut out = ZSet::new();
        let mut next: BTreeMap<K, Option<SumGroup>> = BTreeMap::new();
        for (key, (d_rows, d_total)) in delta {
            let old = self.groups.get(&key).copied();
            let new_rows = checked_add(old.map(|g| g.rows).unwrap_or(0), d_rows)?;
            if new_rows < 0 {
                return Err(Error::NegativeCount);
            }
            let new_total = i64::try_from(i128::from(old.map(|g| g.total).unwrap_or(0)) + d_total)
                .map_err(|_| Error::Overflow)?;
            let new = if new_rows == 0 {
                None
            } else {
                Some(SumGroup {
                    rows: new_rows,
                    total: new_total,
                })
            };
            emit(&mut out, &key, old.map(|g| g.total), new.map(|g| g.total));
            next.insert(key, new);
        }
        for (key, new) in next {
            match new {
                Some(group) => {
                    self.groups.insert(key, group);
                }
                None => {
                    self.groups.remove(&key);
                }
            }
        }
        Ok(out)
    }
}

impl<K, V> Operator for Sum<K, V>
where
    K: Clone + Ord,
    V: Copy + Ord + Into<i64>,
{
    type In = (K, V);
    type Out = (K, i64);
    type Error = Error;

    fn step(&mut self, change: ZSet<(K, V)>) -> Result<ZSet<(K, i64)>, Error> {
        Sum::step(self, change)
    }
}

/// The smallest value in each group.
///
/// Sum updates from the change alone: subtract the deleted amount and the new
/// total is known. Min cannot. If the group's minimum is 30 and the row with
/// 30 is deleted, the next minimum is some other value that is not in this
/// step's change, so the operator keeps every value with its count in an
/// ordered map. The smallest key still present is the answer.
#[derive(Debug, Clone)]
pub struct Min<K: Ord, V: Ord> {
    groups: BTreeMap<K, BTreeMap<V, i64>>,
}

impl<K: Ord, V: Ord> Default for Min<K, V> {
    fn default() -> Self {
        Self {
            groups: BTreeMap::new(),
        }
    }
}

impl<K: Ord, V: Ord> Min<K, V> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this key still has a group.
    pub fn remembers(&self, key: &K) -> bool {
        self.groups.contains_key(key)
    }
}

impl<K, V> Min<K, V>
where
    K: Clone + Ord,
    V: Clone + Ord,
{
    /// Feeds one change and returns the change to `(key, minimum)`.
    pub fn step(&mut self, change: ZSet<(K, V)>) -> Result<ZSet<(K, V)>, Error> {
        let mut delta: BTreeMap<K, BTreeMap<V, i64>> = BTreeMap::new();
        for ((key, value), weight) in change.iter() {
            let slot = delta.entry(key.clone()).or_default();
            add_i64(slot.entry(value.clone()).or_insert(0), weight)?;
        }

        let mut out = ZSet::new();
        let mut next: BTreeMap<K, Option<BTreeMap<V, i64>>> = BTreeMap::new();
        for (key, d) in delta {
            let old_values = self.groups.get(&key);
            let old_min = old_values.and_then(minimum);
            let mut new_values = old_values.cloned().unwrap_or_default();
            for (value, weight) in d {
                let count = new_values.entry(value.clone()).or_insert(0);
                add_i64(count, weight)?;
                if *count < 0 {
                    return Err(Error::NegativeCount);
                }
                if *count == 0 {
                    new_values.remove(&value);
                }
            }
            let new = if new_values.is_empty() {
                None
            } else {
                Some(new_values)
            };
            let new_min = new.as_ref().and_then(minimum);
            emit(&mut out, &key, old_min.cloned(), new_min.cloned());
            next.insert(key, new);
        }
        for (key, new) in next {
            match new {
                Some(values) => {
                    self.groups.insert(key, values);
                }
                None => {
                    self.groups.remove(&key);
                }
            }
        }
        Ok(out)
    }
}

impl<K, V> Operator for Min<K, V>
where
    K: Clone + Ord,
    V: Clone + Ord,
{
    type In = (K, V);
    type Out = (K, V);
    type Error = Error;

    fn step(&mut self, change: ZSet<(K, V)>) -> Result<ZSet<(K, V)>, Error> {
        Min::step(self, change)
    }
}

fn minimum<V: Ord>(values: &BTreeMap<V, i64>) -> Option<&V> {
    values.keys().next()
}

fn checked_add(a: i64, b: i64) -> Result<i64, Error> {
    a.checked_add(b).ok_or(Error::Overflow)
}

fn add_i64(slot: &mut i64, d: i64) -> Result<(), Error> {
    *slot = checked_add(*slot, d)?;
    Ok(())
}

fn emit<K: Clone + Ord, A: Clone + Ord>(
    out: &mut ZSet<(K, A)>,
    key: &K,
    old: Option<A>,
    new: Option<A>,
) {
    if old == new {
        return;
    }
    if let Some(old) = old {
        out.add_row((key.clone(), old), -1);
    }
    if let Some(new) = new {
        out.add_row((key.clone(), new), 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checker::{SEEDS, STEPS, shortest_failing_prefix, unary_stream};

    fn rows<T: Ord>(rows: impl IntoIterator<Item = (T, i64)>) -> ZSet<T> {
        rows.into_iter().collect()
    }

    /// Group rows by key and count them. Shares no code with [`Count::step`].
    fn count_reference(table: &ZSet<(u8, u8)>) -> ZSet<(u8, i64)> {
        let mut counts: BTreeMap<u8, i64> = BTreeMap::new();
        for ((key, _), weight) in table.iter() {
            *counts.entry(*key).or_insert(0) += weight;
        }
        counts
            .into_iter()
            .filter(|(_, count)| *count != 0)
            .map(|(key, count)| ((key, count), 1))
            .collect()
    }

    /// Group rows by key and total their values. Shares no code with [`Sum::step`].
    fn sum_reference(table: &ZSet<(u8, u8)>) -> ZSet<(u8, i64)> {
        let mut rows_by_key: BTreeMap<u8, i64> = BTreeMap::new();
        let mut totals: BTreeMap<u8, i64> = BTreeMap::new();
        for ((key, value), weight) in table.iter() {
            *rows_by_key.entry(*key).or_insert(0) += weight;
            *totals.entry(*key).or_insert(0) += i64::from(*value) * weight;
        }
        rows_by_key
            .into_iter()
            .filter(|(_, count)| *count != 0)
            .map(|(key, _)| ((key, totals[&key]), 1))
            .collect()
    }

    /// Group rows by key and take the smallest value. Shares no code with [`Min::step`].
    fn min_reference(table: &ZSet<(u8, u8)>) -> ZSet<(u8, u8)> {
        let mut mins: BTreeMap<u8, u8> = BTreeMap::new();
        for ((key, value), _) in table.iter() {
            mins.entry(*key)
                .and_modify(|current| *current = (*current).min(*value))
                .or_insert(*value);
        }
        mins.into_iter()
            .map(|(key, value)| ((key, value), 1))
            .collect()
    }

    fn count_matches(changes: &[ZSet<(u8, u8)>]) -> bool {
        let mut count = Count::new();
        let mut table = ZSet::new();
        let mut out = ZSet::new();
        for change in changes {
            let step = count
                .step(change.clone())
                .expect("tiny domain does not overflow");
            out = out + step;
            table = table + change.clone();
        }
        out == count_reference(&table)
    }

    fn sum_matches(changes: &[ZSet<(u8, u8)>]) -> bool {
        let mut sum = Sum::new();
        let mut table = ZSet::new();
        let mut out = ZSet::new();
        for change in changes {
            let step = sum
                .step(change.clone())
                .expect("tiny domain does not overflow");
            out = out + step;
            table = table + change.clone();
        }
        out == sum_reference(&table)
    }

    fn min_matches(changes: &[ZSet<(u8, u8)>]) -> bool {
        let mut min = Min::new();
        let mut table = ZSet::new();
        let mut out = ZSet::new();
        for change in changes {
            let step = min
                .step(change.clone())
                .expect("tiny domain does not overflow");
            out = out + step;
            table = table + change.clone();
        }
        out == min_reference(&table)
    }

    fn assert_seeds(operator: &str, test: &str, matches: impl Fn(&[ZSet<(u8, u8)>]) -> bool) {
        for &seed in SEEDS {
            let stream = unary_stream(seed, STEPS);
            if matches(&stream) {
                continue;
            }
            let prefix = shortest_failing_prefix(stream.len(), |len| !matches(&stream[..len]))
                .expect("full stream failed so a prefix must");
            panic!(
                "{operator} on seed {seed} disagreed with a full recompute at {prefix} step(s) (stream was {STEPS})\nrerun: CARGO_TARGET_DIR=target cargo test -- --exact {test}"
            );
        }
    }

    /// An update to one row emits one retraction and one insert, for that
    /// group only. Customer 2's total is untouched.
    #[test]
    fn an_update_to_one_row_emits_one_retraction_and_one_insert_for_that_group_only() {
        let mut sum = Sum::new();
        let established = sum
            .step(rows([((1, 420), 1), ((1, 80), 1), ((2, 15), 1)]))
            .unwrap();
        assert_eq!(established, rows([((1, 500), 1), ((2, 15), 1)]));

        let out = sum.step(rows([((1, 80), -1)])).unwrap();
        assert_eq!(out, rows([((1, 500), -1), ((1, 420), 1)]));
        assert_eq!(out.weight(&(2, 15)), 0);
        assert_eq!(out.weight(&(2, 0)), 0);
        assert!(sum.remembers(&2));
    }

    #[test]
    fn a_group_whose_last_row_is_deleted_emits_only_the_retraction_and_keeps_no_state() {
        let mut count = Count::new();
        let mut sum = Sum::new();
        let mut min = Min::new();
        let insert = rows([((1, 30), 1)]);
        assert_eq!(count.step(insert.clone()).unwrap(), rows([((1, 1), 1)]));
        assert_eq!(sum.step(insert.clone()).unwrap(), rows([((1, 30), 1)]));
        assert_eq!(min.step(insert).unwrap(), rows([((1, 30), 1)]));

        let delete = rows([((1, 30), -1)]);
        assert_eq!(count.step(delete.clone()).unwrap(), rows([((1, 1), -1)]));
        assert_eq!(sum.step(delete.clone()).unwrap(), rows([((1, 30), -1)]));
        assert_eq!(min.step(delete).unwrap(), rows([((1, 30), -1)]));
        assert!(!count.remembers(&1));
        assert!(!sum.remembers(&1));
        assert!(!min.remembers(&1));
    }

    #[test]
    fn a_step_whose_changes_cancel_emits_nothing() {
        let mut count = Count::new();
        let mut sum = Sum::new();
        let mut min = Min::new();
        let _ = count.step(rows([((1, 10), 1)])).unwrap();
        let _ = sum.step(rows([((1, 10), 1)])).unwrap();
        let _ = min.step(rows([((1, 10), 1)])).unwrap();

        let cancel = rows([((1, 4), 1), ((1, 4), -1)]);
        assert!(count.step(cancel.clone()).unwrap().is_empty());
        assert!(sum.step(cancel.clone()).unwrap().is_empty());
        assert!(min.step(cancel).unwrap().is_empty());
    }

    /// Deleting the current minimum reveals the next value, which the operator
    /// can only know because it stored the whole group. Sum would not need that.
    #[test]
    fn min_returns_the_next_value_after_the_current_minimum_is_deleted() {
        let mut min = Min::new();
        let _ = min
            .step(rows([((1, 30), 1), ((1, 40), 1), ((1, 50), 1)]))
            .unwrap();
        assert_eq!(
            min.step(rows([((1, 30), -1)])).unwrap(),
            rows([((1, 30), -1), ((1, 40), 1)])
        );
        assert!(min.remembers(&1));
    }

    #[test]
    fn a_delete_of_a_row_that_is_not_there_is_a_typed_error() {
        let mut count = Count::new();
        assert_eq!(count.step(rows([((1, 10), -1)])), Err(Error::NegativeCount));
        assert!(!count.remembers(&1));
    }

    #[test]
    fn sum_overflow_is_a_typed_error_and_leaves_state_unchanged() {
        let mut sum = Sum::new();
        let _ = sum.step(rows([((1, i64::MAX), 1)])).unwrap();
        assert_eq!(sum.step(rows([((1, 1), 1)])), Err(Error::Overflow));
        assert!(sum.remembers(&1));
        assert_eq!(
            sum.step(rows([((1, i64::MAX), -1)])).unwrap(),
            rows([((1, i64::MAX), -1)])
        );
        assert!(!sum.remembers(&1));
    }

    /// `i64::MIN` times weight `-1` is not an `i64`, but adding it to a stored
    /// total of `i64::MIN` is zero. The contribution is accumulated in `i128`
    /// so this delete is allowed.
    #[test]
    fn deleting_a_stored_i64_min_does_not_overflow() {
        let mut sum = Sum::new();
        let _ = sum.step(rows([((1, i64::MIN), 1)])).unwrap();
        assert_eq!(
            sum.step(rows([((1, i64::MIN), -1)])).unwrap(),
            rows([((1, i64::MIN), -1)])
        );
        assert!(!sum.remembers(&1));
    }

    #[test]
    fn count_on_random_streams_matches_a_full_recompute() {
        assert_seeds(
            "count",
            "aggregate::tests::count_on_random_streams_matches_a_full_recompute",
            count_matches,
        );
    }

    #[test]
    fn sum_on_random_streams_matches_a_full_recompute() {
        assert_seeds(
            "sum",
            "aggregate::tests::sum_on_random_streams_matches_a_full_recompute",
            sum_matches,
        );
    }

    #[test]
    fn min_on_random_streams_matches_a_full_recompute() {
        assert_seeds(
            "min",
            "aggregate::tests::min_on_random_streams_matches_a_full_recompute",
            min_matches,
        );
    }
}
