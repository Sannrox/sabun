//! Distinct: presence, not count.
//!
//! Two hundred orders that ship to Wien are one city in the set of cities.
//! `distinct` turns a counted collection into that set. It is the first
//! operator whose output depends on a threshold: a row appears or disappears
//! only when its count crosses zero.
//!
//! `distinct` of a change is not the change of `distinct`. Wien is already
//! present. A step inserts Wien again and inserts Lyon. The change, passed
//! through `distinct` with no memory, would emit both cities. The operator
//! that has kept Wien's count emits only Lyon. So this operator needs state:
//! the accumulated count of every row.

use std::error::Error as StdError;
use std::fmt;

use crate::circuit::Operator;
use crate::zset::ZSet;

/// An accumulated count would wrap.
///
/// The operator leaves its state unchanged so the caller can retry or reject
/// the step without a partial update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overflow;

impl fmt::Display for Overflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("distinct accumulator overflowed")
    }
}

impl StdError for Overflow {}

/// Incremental distinct of a `ZSet<T>`.
///
/// State is the counted table so far, itself a [`ZSet`]. A row whose count
/// returns to zero is forgotten. Output weights are only `+1` and `-1`.
/// Presence means a positive count; negative accumulated counts are treated
/// as absent.
#[derive(Debug, Clone)]
pub struct Distinct<T: Ord> {
    counts: ZSet<T>,
}

impl<T: Ord> Default for Distinct<T> {
    fn default() -> Self {
        Self {
            counts: ZSet::new(),
        }
    }
}

impl<T: Ord> Distinct<T> {
    /// An empty distinct that has not seen any rows yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this row still has a non-zero stored count.
    pub fn remembers(&self, row: &T) -> bool {
        self.counts.weight(row) != 0
    }
}

impl<T: Clone + Ord> Distinct<T> {
    /// Feeds one change and returns the change to the set of present rows.
    ///
    /// For each row, compare "was the count positive before" with "is it
    /// positive after". Only a difference emits, and it emits weight `+1` or
    /// `-1`, never the size of the crossing. Counts that would wrap are
    /// [`Overflow`] and leave state unchanged.
    pub fn step(&mut self, change: ZSet<T>) -> Result<ZSet<T>, Overflow> {
        let mut out = ZSet::new();
        let mut apply: Vec<(T, i64)> = Vec::new();
        for (row, weight) in change.iter() {
            let before = self.counts.weight(row);
            let after = before.checked_add(weight).ok_or(Overflow)?;
            let was_present = before > 0;
            let is_present = after > 0;
            if was_present != is_present {
                out.add_row(row.clone(), if is_present { 1 } else { -1 });
            }
            apply.push((row.clone(), weight));
        }
        for (row, weight) in apply {
            self.counts.add_row(row, weight);
        }
        Ok(out)
    }
}

impl<T: Clone + Ord> Operator for Distinct<T> {
    type In = T;
    type Out = T;
    type Error = Overflow;

    fn step(&mut self, change: ZSet<T>) -> Result<ZSet<T>, Overflow> {
        Distinct::step(self, change)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checker::{SEEDS, STEPS, shortest_failing_prefix, unary_stream};

    fn rows<T: Ord>(rows: impl IntoIterator<Item = (T, i64)>) -> ZSet<T> {
        rows.into_iter().collect()
    }

    /// Rows with a positive count, each with weight 1. Shares no code with
    /// [`Distinct::step`].
    fn distinct_reference(table: &ZSet<(u8, u8)>) -> ZSet<(u8, u8)> {
        table
            .iter()
            .filter(|(_, weight)| *weight > 0)
            .map(|(row, _)| (*row, 1))
            .collect()
    }

    fn distinct_matches(changes: &[ZSet<(u8, u8)>]) -> bool {
        let mut distinct = Distinct::new();
        let mut table = ZSet::new();
        let mut out = ZSet::new();
        for change in changes {
            out = out
                + distinct
                    .step(change.clone())
                    .expect("tiny domain does not overflow");
            table = table + change.clone();
        }
        out == distinct_reference(&table)
    }

    /// Output appears only when a count crosses zero. Deleting one of three
    /// Wiens does not cross; deleting the last one does.
    #[test]
    fn output_appears_only_when_a_count_crosses_zero() {
        let mut distinct = Distinct::new();
        assert_eq!(
            distinct.step(rows([("Wien", 3)])).unwrap(),
            rows([("Wien", 1)])
        );
        assert!(distinct.step(rows([("Wien", -1)])).unwrap().is_empty());
        assert_eq!(
            distinct.step(rows([("Wien", -2)])).unwrap(),
            rows([("Wien", -1)])
        );
    }

    #[test]
    fn deleting_one_of_three_emits_nothing() {
        let mut distinct = Distinct::new();
        let _ = distinct.step(rows([("Wien", 3)])).unwrap();
        assert!(distinct.step(rows([("Wien", -1)])).unwrap().is_empty());
        assert!(distinct.remembers(&"Wien"));
    }

    #[test]
    fn deleting_the_last_copy_emits_a_retraction() {
        let mut distinct = Distinct::new();
        let _ = distinct.step(rows([("Wien", 1)])).unwrap();
        assert_eq!(
            distinct.step(rows([("Wien", -1)])).unwrap(),
            rows([("Wien", -1)])
        );
    }

    #[test]
    fn two_inserts_and_one_delete_from_zero_emit_one_insert() {
        let mut distinct = Distinct::new();
        assert_eq!(
            distinct.step(rows([("Lyon", 2), ("Lyon", -1)])).unwrap(),
            rows([("Lyon", 1)])
        );
    }

    /// Wien is already present. The step inserts Wien again and inserts Lyon.
    /// `distinct` of that change would emit both cities; the change of
    /// `distinct` emits only Lyon.
    #[test]
    fn distinct_of_a_change_is_not_the_change_of_distinct() {
        let mut distinct = Distinct::new();
        let _ = distinct.step(rows([("Wien", 1)])).unwrap();
        let change = rows([("Wien", 1), ("Lyon", 1)]);
        assert_eq!(distinct.step(change.clone()).unwrap(), rows([("Lyon", 1)]));

        let of_the_change = Distinct::new().step(change).unwrap();
        assert_eq!(of_the_change, rows([("Wien", 1), ("Lyon", 1)]));
    }

    #[test]
    fn the_output_accumulated_over_a_stream_has_every_weight_equal_to_1() {
        let mut distinct = Distinct::new();
        let mut out = ZSet::new();
        let steps = [
            rows([("Wien", 2)]),
            rows([("Lyon", 1), ("Wien", -1)]),
            rows([("Wien", -1)]),
            rows([("Lyon", 1), ("Graz", 1)]),
        ];
        for change in steps {
            out = out + distinct.step(change).unwrap();
        }
        for (_, weight) in out.iter() {
            assert_eq!(weight, 1);
        }
        assert_eq!(out, rows([("Lyon", 1), ("Graz", 1)]));
    }

    #[test]
    fn a_row_whose_count_returns_to_zero_holds_no_state() {
        let mut distinct = Distinct::new();
        let _ = distinct.step(rows([("Wien", 1)])).unwrap();
        let _ = distinct.step(rows([("Wien", -1)])).unwrap();
        assert!(!distinct.remembers(&"Wien"));
    }

    #[test]
    fn overflow_is_a_typed_error_and_leaves_state_unchanged() {
        let mut distinct = Distinct::new();
        let _ = distinct.step(rows([("Wien", i64::MAX)])).unwrap();
        assert_eq!(distinct.step(rows([("Wien", 1)])), Err(Overflow));
        assert!(distinct.remembers(&"Wien"));
        assert_eq!(
            distinct.step(rows([("Wien", -i64::MAX)])).unwrap(),
            rows([("Wien", -1)])
        );
        assert!(!distinct.remembers(&"Wien"));
    }

    #[test]
    fn distinct_on_random_streams_matches_a_full_recompute() {
        for &seed in SEEDS {
            let stream = unary_stream(seed, STEPS);
            if distinct_matches(&stream) {
                continue;
            }
            let prefix =
                shortest_failing_prefix(stream.len(), |len| !distinct_matches(&stream[..len]))
                    .expect("full stream failed so a prefix must");
            panic!(
                "distinct on seed {seed} disagreed with a full recompute at {prefix} step(s) (stream was {STEPS})\nrerun: CARGO_TARGET_DIR=target cargo test -- --exact distinct::tests::distinct_on_random_streams_matches_a_full_recompute"
            );
        }
    }
}
