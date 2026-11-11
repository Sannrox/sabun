//! Reachability kept current from edge changes.
//!
//! A pair `(a, b)` is in the result when `b` is reachable from `a` along one
//! or more directed edges. Direct edges are paths of length one. Longer paths
//! come from the query referring to its own result: `a` reaches `c` if `a`
//! reaches `b` and there is an edge from `b` to `c`.
//!
//! `map` and `filter` run on a change alone. Reachability cannot: a new edge
//! from `c` to `d` also creates `(a, d)` for every stored pair that already
//! ends at `c`. So the operator stores the present edges and the present
//! pairs.
//!
//! Inserts repeat a join-and-distinct round on only the newly found pairs.
//! That is enough because any new path of length `n+1` extends a new path of
//! length `n`, or a new edge. Old pairs joined with old edges already produced
//! everything they can. Example: edges `a→b` and `b→c` hold, so the pairs are
//! `(a, b)`, `(b, c)`, `(a, c)`. Adding `c→d` seeds three new pairs — the edge
//! itself, then `(a, c) ⋈ (c, d) = (a, d)` and `(b, c) ⋈ (c, d) = (b, d)`. One
//! further round joins those new pairs with the edges, finds nothing, and
//! stops.
//!
//! Counts do not work. In a cycle `a → b → a` the pair `(a, a)` is derived at
//! length 2, again at length 4, and so on. [`Distinct`] keeps presence, so
//! `(a, a)` appears once.
//!
//! Deletes are the other half. Edges `a→b`, `b→c` and `a→c`: deleting `b→c`
//! leaves `(a, c)` reachable. Retracting every pair the deleted edge helped
//! derive would drop `(a, c)` even though `a→c` remains. This operator forgets
//! the derived pairs, closes the edges that are left, and emits the
//! difference. See `docs/decisions/0001-reachability-deletes.md`.
//!
//! A round that finds no new pair stops. More than `|V|²` join rounds means
//! the loop failed to forget already-known pairs, and is [`Error::UnboundedRound`]
//! instead of a hang.

use std::collections::BTreeSet;
use std::error::Error as StdError;
use std::fmt;

use crate::circuit::Operator;
use crate::distinct::{Distinct, Overflow};
use crate::join::Join;
use crate::zset::ZSet;

/// An edge-count overflow, or a join loop that did not stop.
///
/// The operator leaves its state unchanged so the caller can retry or reject
/// the step without a partial update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Overflow,
    UnboundedRound,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overflow => f.write_str("reachability edge-count overflowed"),
            Self::UnboundedRound => f.write_str("reachability join rounds exceeded the pair bound"),
        }
    }
}

impl StdError for Error {}

impl From<Overflow> for Error {
    fn from(_: Overflow) -> Self {
        Self::Overflow
    }
}

/// Incremental reachability of `ZSet<(N, N)>` edges.
///
/// State is the counted edges (so a duplicate insert is not a new edge) and
/// the set of reachable pairs. A pair or edge whose count returns to zero is
/// forgotten.
#[derive(Debug, Clone)]
pub struct Reachability<N: Ord> {
    edge_counts: Distinct<(N, N)>,
    edges: ZSet<(N, N)>,
    pairs: ZSet<(N, N)>,
}

impl<N: Ord> Default for Reachability<N> {
    fn default() -> Self {
        Self {
            edge_counts: Distinct::new(),
            edges: ZSet::new(),
            pairs: ZSet::new(),
        }
    }
}

impl<N: Ord> Reachability<N> {
    /// An empty view that has not seen any edges yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this edge is still present.
    pub fn remembers_edge(&self, edge: &(N, N)) -> bool {
        self.edges.weight(edge) != 0
    }

    /// Whether this pair is still reachable.
    pub fn remembers_pair(&self, pair: &(N, N)) -> bool {
        self.pairs.weight(pair) != 0
    }
}

impl<N: Clone + Ord> Reachability<N> {
    /// Feeds one edge change and returns the change to the reachable pairs.
    ///
    /// Insert-only steps join the last new pairs with the current edges until
    /// a round finds nothing. A step that retracts a present edge forgets the
    /// derived pairs and closes the remaining edges. Empty input is a no-op.
    pub fn step(&mut self, change: ZSet<(N, N)>) -> Result<ZSet<(N, N)>, Error> {
        if change.is_empty() {
            return Ok(ZSet::new());
        }

        let mut edge_counts = self.edge_counts.clone();
        let mut edges = self.edges.clone();
        let edge_change = edge_counts.step(change)?;
        if edge_change.is_empty() {
            self.edge_counts = edge_counts;
            return Ok(ZSet::new());
        }
        edges = edges + edge_change.clone();

        let pairs = if edge_change.iter().any(|(_, weight)| weight < 0) {
            close(&edges)?
        } else {
            insert(&self.pairs, &edges, &edge_change)?
        };
        let out = diff(&self.pairs, &pairs);
        self.edge_counts = edge_counts;
        self.edges = edges;
        self.pairs = pairs;
        Ok(out)
    }
}

impl<N: Clone + Ord> Operator for Reachability<N> {
    type In = (N, N);
    type Out = (N, N);
    type Error = Error;

    fn step(&mut self, change: ZSet<(N, N)>) -> Result<ZSet<(N, N)>, Error> {
        Reachability::step(self, change)
    }
}

/// One equality join of paths `(src, mid)` with edges `(mid, dst)`.
fn extend<N: Clone + Ord>(paths: &ZSet<(N, N)>, edges: &ZSet<(N, N)>) -> ZSet<(N, N)> {
    if paths.is_empty() || edges.is_empty() {
        return ZSet::new();
    }
    let left = paths.map(|(src, mid)| (mid.clone(), src.clone()));
    let mut join = Join::new();
    join.step(left, edges.clone())
        .map(|(_, src, dst)| (src.clone(), dst.clone()))
}

/// Presence of pairs derived this round that `known` does not already hold.
fn fresh<N: Clone + Ord>(raw: ZSet<(N, N)>, known: &ZSet<(N, N)>) -> Result<ZSet<(N, N)>, Error> {
    let derived = Distinct::new().step(raw)?;
    let mut next = ZSet::new();
    for (pair, weight) in derived.iter() {
        if weight > 0 && known.weight(pair) == 0 {
            next.add_row(pair.clone(), 1);
        }
    }
    Ok(next)
}

fn node_bound<N: Clone + Ord>(edges: &ZSet<(N, N)>) -> usize {
    let mut nodes = BTreeSet::new();
    for ((src, dst), _) in edges.iter() {
        nodes.insert(src.clone());
        nodes.insert(dst.clone());
    }
    let n = nodes.len();
    n.saturating_mul(n).max(1)
}

/// Join-and-distinct from `delta` until a round finds nothing.
///
/// `known` is the pairs already in the result. Seed pairs in `delta` are
/// emitted first; each later round joins only that last delta with `edges`.
fn saturate<N: Clone + Ord>(
    mut delta: ZSet<(N, N)>,
    edges: &ZSet<(N, N)>,
    mut known: ZSet<(N, N)>,
    max_rounds: usize,
) -> Result<ZSet<(N, N)>, Error> {
    for (pair, weight) in delta.iter() {
        known.add_row(pair.clone(), weight);
    }
    let mut rounds = 0;
    loop {
        if delta.is_empty() {
            return Ok(known);
        }
        if rounds >= max_rounds {
            return Err(Error::UnboundedRound);
        }
        rounds += 1;
        delta = fresh(extend(&delta, edges), &known)?;
        for (pair, weight) in delta.iter() {
            known.add_row(pair.clone(), weight);
        }
    }
}

fn insert<N: Clone + Ord>(
    pairs: &ZSet<(N, N)>,
    edges: &ZSet<(N, N)>,
    new_edges: &ZSet<(N, N)>,
) -> Result<ZSet<(N, N)>, Error> {
    let mut seed = ZSet::new();
    for (edge, weight) in new_edges.iter() {
        if weight > 0 && pairs.weight(edge) == 0 {
            seed.add_row(edge.clone(), 1);
        }
    }
    let extended = fresh(extend(pairs, new_edges), pairs)?;
    for (pair, weight) in extended.iter() {
        if seed.weight(pair) == 0 {
            seed.add_row(pair.clone(), weight);
        }
    }
    saturate(seed, edges, pairs.clone(), node_bound(edges))
}

fn close<N: Clone + Ord>(edges: &ZSet<(N, N)>) -> Result<ZSet<(N, N)>, Error> {
    saturate(edges.clone(), edges, ZSet::new(), node_bound(edges))
}

fn diff<N: Clone + Ord>(old: &ZSet<(N, N)>, new: &ZSet<(N, N)>) -> ZSet<(N, N)> {
    let mut out = ZSet::new();
    for (pair, weight) in old.iter() {
        if new.weight(pair) == 0 {
            out.add_row(pair.clone(), -weight);
        }
    }
    for (pair, weight) in new.iter() {
        if old.weight(pair) == 0 {
            out.add_row(pair.clone(), weight);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::checker::{SEEDS, STEPS, shortest_failing_prefix, unary_stream};

    fn rows<T: Ord>(rows: impl IntoIterator<Item = (T, i64)>) -> ZSet<T> {
        rows.into_iter().collect()
    }

    /// Walk the present edges from each source. Shares no code with
    /// [`Reachability::step`].
    fn reachability_reference(edges: &ZSet<(u8, u8)>) -> ZSet<(u8, u8)> {
        let mut adj: BTreeMap<u8, Vec<u8>> = BTreeMap::new();
        for ((src, dst), weight) in edges.iter() {
            if weight > 0 {
                adj.entry(*src).or_default().push(*dst);
            }
        }
        let mut out = ZSet::new();
        let starts: Vec<u8> = adj.keys().copied().collect();
        for start in starts {
            let mut seen = BTreeSet::new();
            let mut stack = adj.get(&start).cloned().unwrap_or_default();
            while let Some(node) = stack.pop() {
                if !seen.insert(node) {
                    continue;
                }
                out.add_row((start, node), 1);
                if let Some(next) = adj.get(&node) {
                    stack.extend(next.iter().copied());
                }
            }
        }
        out
    }

    fn insert_only(changes: &[ZSet<(u8, u8)>]) -> Vec<ZSet<(u8, u8)>> {
        changes
            .iter()
            .map(|change| {
                change
                    .iter()
                    .filter(|(_, weight)| *weight > 0)
                    .map(|(row, weight)| (*row, weight))
                    .collect()
            })
            .collect()
    }

    fn reachability_matches(changes: &[ZSet<(u8, u8)>]) -> bool {
        let mut reach = Reachability::new();
        let mut table = ZSet::new();
        let mut out = ZSet::new();
        for change in changes {
            out = out
                + reach
                    .step(change.clone())
                    .expect("tiny domain does not overflow or diverge");
            table = table + change.clone();
        }
        out == reachability_reference(&table)
    }

    /// Adding `c→d` when `a→b` and `b→c` already hold produces `(c, d)`,
    /// `(b, d)` and `(a, d)`, then the next round finds nothing.
    #[test]
    fn adding_an_edge_joins_only_the_new_pairs_with_the_edges() {
        let mut reach = Reachability::new();
        let first = reach
            .step(rows([(("a", "b"), 1), (("b", "c"), 1)]))
            .unwrap();
        assert_eq!(
            first,
            rows([(("a", "b"), 1), (("b", "c"), 1), (("a", "c"), 1)])
        );

        assert_eq!(
            reach.step(rows([(("c", "d"), 1)])).unwrap(),
            rows([(("c", "d"), 1), (("b", "d"), 1), (("a", "d"), 1)])
        );
    }

    /// Edges `a→b`, `b→c` and `a→c`. Deleting `b→c` leaves `(a, c)`: the
    /// direct edge is another route. A naive retract of everything derived
    /// from `b→c` would drop `(a, c)` too.
    #[test]
    fn deleting_an_edge_keeps_pairs_that_still_have_another_route() {
        let mut reach = Reachability::new();
        let _ = reach
            .step(rows([(("a", "b"), 1), (("b", "c"), 1), (("a", "c"), 1)]))
            .unwrap();

        assert_eq!(
            reach.step(rows([(("b", "c"), -1)])).unwrap(),
            rows([(("b", "c"), -1)])
        );
        assert!(reach.remembers_pair(&("a", "c")));
        assert!(reach.remembers_edge(&("a", "c")));
        assert!(!reach.remembers_pair(&("b", "c")));
    }

    /// A cycle `a → b → a` derives `(a, a)` and `(b, b)` once each. Another
    /// insert of the same edge does not grow the result.
    #[test]
    fn a_cycle_derives_each_pair_once_and_then_stops() {
        let mut reach = Reachability::new();
        assert_eq!(
            reach
                .step(rows([(("a", "b"), 1), (("b", "a"), 1)]))
                .unwrap(),
            rows([
                (("a", "b"), 1),
                (("b", "a"), 1),
                (("a", "a"), 1),
                (("b", "b"), 1)
            ])
        );
        assert!(reach.step(rows([(("a", "b"), 1)])).unwrap().is_empty());
        assert!(reach.remembers_pair(&("a", "a")));
        assert!(reach.remembers_pair(&("b", "b")));
    }

    #[test]
    fn exceeding_the_round_bound_is_a_typed_error() {
        let edges = rows([((0, 1), 1)]);
        let delta = rows([((0, 1), 1)]);
        assert_eq!(
            saturate(delta, &edges, ZSet::new(), 0),
            Err(Error::UnboundedRound)
        );
    }

    #[test]
    fn an_empty_change_produces_an_empty_change_and_touches_no_state() {
        let mut reach = Reachability::new();
        let _ = reach.step(rows([(("a", "b"), 1)])).unwrap();
        assert!(reach.remembers_edge(&("a", "b")));
        assert!(reach.step(ZSet::new()).unwrap().is_empty());
        assert!(reach.remembers_edge(&("a", "b")));
        assert!(reach.remembers_pair(&("a", "b")));
    }

    #[test]
    fn a_duplicate_edge_insert_emits_nothing() {
        let mut reach = Reachability::new();
        let _ = reach.step(rows([(("a", "b"), 1)])).unwrap();
        assert!(reach.step(rows([(("a", "b"), 1)])).unwrap().is_empty());
        assert!(reach.remembers_edge(&("a", "b")));
    }

    #[test]
    fn deleting_the_last_edge_forgets_its_pairs() {
        let mut reach = Reachability::new();
        let _ = reach
            .step(rows([(("a", "b"), 1), (("b", "c"), 1)]))
            .unwrap();
        let out = reach
            .step(rows([(("a", "b"), -1), (("b", "c"), -1)]))
            .unwrap();
        assert_eq!(
            out,
            rows([(("a", "b"), -1), (("b", "c"), -1), (("a", "c"), -1)])
        );
        assert!(!reach.remembers_edge(&("a", "b")));
        assert!(!reach.remembers_pair(&("a", "c")));
    }

    #[test]
    fn overflow_is_a_typed_error_and_leaves_state_unchanged() {
        let mut reach = Reachability::new();
        let _ = reach.step(rows([(("a", "b"), i64::MAX)])).unwrap();
        assert_eq!(reach.step(rows([(("a", "b"), 1)])), Err(Error::Overflow));
        assert!(reach.remembers_edge(&("a", "b")));
        assert!(reach.remembers_pair(&("a", "b")));
    }

    /// Adding up per-step reachability changes equals walking the final graph.
    #[test]
    fn insert_only_reachability_on_random_streams_matches_a_full_recompute() {
        for &seed in SEEDS {
            let stream = insert_only(&unary_stream(seed, STEPS));
            if reachability_matches(&stream) {
                continue;
            }
            let prefix =
                shortest_failing_prefix(stream.len(), |len| !reachability_matches(&stream[..len]))
                    .expect("full stream failed so a prefix must");
            panic!(
                "reachability (insert-only) on seed {seed} disagreed with a full recompute at {prefix} step(s) (stream was {STEPS})\nrerun: CARGO_TARGET_DIR=target cargo test -- --exact reachability::tests::insert_only_reachability_on_random_streams_matches_a_full_recompute"
            );
        }
    }

    /// Adding up per-step reachability changes equals walking the final graph,
    /// including deletes, cycles, and alternative routes.
    #[test]
    fn reachability_on_random_streams_matches_a_full_recompute() {
        for &seed in SEEDS {
            let stream = unary_stream(seed, STEPS);
            if reachability_matches(&stream) {
                continue;
            }
            let prefix =
                shortest_failing_prefix(stream.len(), |len| !reachability_matches(&stream[..len]))
                    .expect("full stream failed so a prefix must");
            panic!(
                "reachability on seed {seed} disagreed with a full recompute at {prefix} step(s) (stream was {STEPS})\nrerun: CARGO_TARGET_DIR=target cargo test -- --exact reachability::tests::reachability_on_random_streams_matches_a_full_recompute"
            );
        }
    }
}
