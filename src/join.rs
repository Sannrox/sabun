//! Equality join that remembers both sides.
//!
//! `map` and `filter` can run on a change alone. A join cannot: the matching
//! row on the other side arrived earlier and is not in this step's change.
//! So a join stores everything it has seen, indexed by key, and uses that
//! memory to turn a change on either side into a change to the result.
//!
//! Write the tables as old plus change and expand the product:
//!
//! `(L + dL) ⋈ (R + dR) = L ⋈ R + dL ⋈ R + L ⋈ dR + dL ⋈ dR`
//!
//! The first term is the old result. The other three are the change. Weights
//! multiply because a result row exists once for every pair of input copies.

use std::collections::BTreeMap;

use crate::zset::ZSet;

/// Incremental equality join of `ZSet<(K, L)>` with `ZSet<(K, R)>`.
///
/// State is the rows seen so far on each side, grouped by key. A key that
/// holds no rows on a side is forgotten, so the stored maps stay exactly the
/// current tables.
#[derive(Debug, Clone)]
pub struct Join<K: Ord, L: Ord, R: Ord> {
    left: BTreeMap<K, ZSet<L>>,
    right: BTreeMap<K, ZSet<R>>,
}

impl<K: Ord, L: Ord, R: Ord> Default for Join<K, L, R> {
    fn default() -> Self {
        Self {
            left: BTreeMap::new(),
            right: BTreeMap::new(),
        }
    }
}

impl<K: Ord, L: Ord, R: Ord> Join<K, L, R> {
    /// An empty join that has not seen either side yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the left side still holds rows for `key`.
    pub fn remembers_left(&self, key: &K) -> bool {
        self.left.contains_key(key)
    }

    /// Whether the right side still holds rows for `key`.
    pub fn remembers_right(&self, key: &K) -> bool {
        self.right.contains_key(key)
    }
}

impl<K, L, R> Join<K, L, R>
where
    K: Clone + Ord,
    L: Clone + Ord,
    R: Clone + Ord,
{
    /// Feeds one change per side and returns the change to the joined result.
    ///
    /// The three change terms are computed against the stored tables *before*
    /// those tables absorb the inputs. Applying first would count `dL ⋈ dR`
    /// twice: once in `dL ⋈ R'` and once in `L' ⋈ dR`.
    pub fn step(&mut self, d_left: ZSet<(K, L)>, d_right: ZSet<(K, R)>) -> ZSet<(K, L, R)> {
        let d_left = index_by_key(&d_left);
        let d_right = index_by_key(&d_right);

        let mut change = ZSet::new();
        for (key, left_delta) in &d_left {
            if let Some(right) = self.right.get(key) {
                append_product(&mut change, key, left_delta, right);
            }
        }
        for (key, right_delta) in &d_right {
            if let Some(left) = self.left.get(key) {
                append_product(&mut change, key, left, right_delta);
            }
        }
        for (key, left_delta) in &d_left {
            if let Some(right_delta) = d_right.get(key) {
                append_product(&mut change, key, left_delta, right_delta);
            }
        }

        apply(&mut self.left, d_left);
        apply(&mut self.right, d_right);
        change
    }
}

fn index_by_key<K: Clone + Ord, V: Clone + Ord>(change: &ZSet<(K, V)>) -> BTreeMap<K, ZSet<V>> {
    let mut by_key: BTreeMap<K, ZSet<V>> = BTreeMap::new();
    for ((key, value), weight) in change.iter() {
        by_key
            .entry(key.clone())
            .or_default()
            .add_row(value.clone(), weight);
    }
    by_key
}

fn append_product<K: Clone + Ord, L: Clone + Ord, R: Clone + Ord>(
    out: &mut ZSet<(K, L, R)>,
    key: &K,
    left: &ZSet<L>,
    right: &ZSet<R>,
) {
    for (left_row, left_weight) in left.iter() {
        for (right_row, right_weight) in right.iter() {
            out.add_row(
                (key.clone(), left_row.clone(), right_row.clone()),
                left_weight * right_weight,
            );
        }
    }
}

fn apply<K: Clone + Ord, V: Ord>(store: &mut BTreeMap<K, ZSet<V>>, change: BTreeMap<K, ZSet<V>>) {
    for (key, delta) in change {
        match store.entry(key) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                if !delta.is_empty() {
                    slot.insert(delta);
                }
            }
            std::collections::btree_map::Entry::Occupied(slot) => {
                let key = slot.key().clone();
                let combined = slot.remove() + delta;
                if !combined.is_empty() {
                    store.insert(key, combined);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows<T: Ord>(rows: impl IntoIterator<Item = (T, i64)>) -> ZSet<T> {
        rows.into_iter().collect()
    }

    /// Nested-loop join of two full tables. Used as the oracle for the
    /// incremental operator: it shares no code with [`Join::step`].
    fn join_tables<K, L, R>(left: &ZSet<(K, L)>, right: &ZSet<(K, R)>) -> ZSet<(K, L, R)>
    where
        K: Clone + Ord,
        L: Clone + Ord,
        R: Clone + Ord,
    {
        let mut out = ZSet::new();
        for ((left_key, left_row), left_weight) in left.iter() {
            for ((right_key, right_row), right_weight) in right.iter() {
                if left_key == right_key {
                    out.add_row(
                        (left_key.clone(), left_row.clone(), right_row.clone()),
                        left_weight * right_weight,
                    );
                }
            }
        }
        out
    }

    /// Feeding changes step by step and adding up the outputs equals joining
    /// the two full tables once.
    #[test]
    fn joining_step_by_step_equals_joining_the_final_tables() {
        let mut join = Join::new();
        let mut orders = ZSet::new();
        let mut customers = ZSet::new();
        let mut accumulated = ZSet::new();

        // orders: (customer_id, order_id); customers: (customer_id, name)
        let steps = [
            (rows([((1, 10), 1)]), rows([((1, "Ada"), 1)])),
            (rows([((1, 11), 1)]), rows([])),
            (rows([((2, 20), 1)]), rows([((2, "Bo"), 1)])),
            (rows([((1, 10), -1)]), rows([((1, "Ada"), -1)])),
            (rows([]), rows([((2, "Bo"), -1), ((2, "Cy"), 1)])),
        ];

        for (d_orders, d_customers) in steps {
            accumulated = accumulated + join.step(d_orders.clone(), d_customers.clone());
            orders = orders + d_orders;
            customers = customers + d_customers;
        }

        assert_eq!(accumulated, join_tables(&orders, &customers));
    }

    /// One new order arrives. The stored customer with that id is the only
    /// right-side row needed to produce the result change.
    #[test]
    fn a_new_order_joins_the_stored_customer() {
        let mut join = Join::new();
        assert!(join.step(rows([]), rows([((1, "Ada"), 1)])).is_empty());

        assert_eq!(
            join.step(rows([((1, 10), 1)]), rows([])),
            rows([((1, 10, "Ada"), 1)])
        );
    }

    /// Deleting a customer retracts every result row that customer produced,
    /// with weight equal to the stored order weight times -1.
    #[test]
    fn deleting_a_customer_retracts_its_joined_orders() {
        let mut join = Join::new();
        let _ = join.step(rows([((1, 10), 1), ((1, 11), 1)]), rows([((1, "Ada"), 1)]));

        assert_eq!(
            join.step(rows([]), rows([((1, "Ada"), -1)])),
            rows([((1, 10, "Ada"), -1), ((1, 11, "Ada"), -1)])
        );
    }

    /// A new customer and that customer's first order arrive together. Only
    /// `dL ⋈ dR` is non-empty, so one result row appears; `dL ⋈ R` and
    /// `L ⋈ dR` are empty because neither side had the key stored.
    #[test]
    fn a_customer_and_first_order_in_the_same_step_produce_one_row() {
        let mut join = Join::new();
        assert_eq!(
            join.step(rows([((2, 20), 1)]), rows([((2, "Bo"), 1)])),
            rows([((2, 20, "Bo"), 1)])
        );
    }

    #[test]
    fn a_delete_on_one_side_removes_exactly_the_result_rows_it_produced() {
        let mut join = Join::new();
        let inserted = join.step(rows([((1, 10), 1)]), rows([((1, "Ada"), 1)]));
        let deleted = join.step(rows([((1, 10), -1)]), rows([]));
        assert_eq!(deleted, -inserted);
        assert!(!join.remembers_left(&1));
        assert!(join.remembers_right(&1));
    }

    /// Weights multiply: two copies on the left and three on the right make
    /// six copies of the pair, because a result row exists once for every
    /// pair of input copies.
    #[test]
    fn weights_multiply_because_each_pair_of_copies_is_one_result() {
        let mut join = Join::new();
        assert_eq!(
            join.step(rows([((1, "x"), 2)]), rows([((1, "y"), 3)])),
            rows([((1, "x", "y"), 6)])
        );
    }

    #[test]
    fn a_key_with_no_rows_left_holds_no_state() {
        let mut join = Join::new();
        let _ = join.step(rows([((1, 10), 1)]), rows([((1, "Ada"), 1)]));
        let _ = join.step(rows([((1, 10), -1)]), rows([((1, "Ada"), -1)]));
        assert!(!join.remembers_left(&1));
        assert!(!join.remembers_right(&1));
    }
}
