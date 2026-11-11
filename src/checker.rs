//! Random change streams compared with a full recompute.
//!
//! Every operator promises the same thing: feed it changes one step at a time,
//! add up what it emits, and you get the plain query on the final tables. This
//! module is the test that nobody wrote the cases for. It draws a stream from a
//! seed, runs the operator incrementally, and checks the result against a
//! nested-loop query that shares no code with the incremental path.
//!
//! Three design choices make the checker useful rather than decorative:
//!
//! - The row domain is tiny (three keys and three values, nine rows). A huge
//!   domain almost never deletes a row it inserted, so retractions and
//!   re-inserts (the cases that need state) never run.
//! - Generated tables stay valid: a delete is only drawn for a row whose
//!   current count is positive. A negative count is not a table, and operators
//!   such as distinct stop making sense on one.
//! - A failing run of hundreds of steps is cut to the shortest prefix of the
//!   same stream that still fails. Later retractions can make a longer prefix
//!   pass, so every length is checked; the first failure is the one to read.

use crate::join::Join;
use crate::zset::ZSet;

/// Seeds later operator tests reuse. Ordinary numbers, not a special sequence.
pub(crate) const SEEDS: &[u64] = &[1, 2, 7, 42, 99, 12345];

/// Fifty steps over nine rows revisits every row many times.
pub(crate) const STEPS: usize = 50;

const KEYS: [u8; 3] = [0, 1, 2];
const VALUES: [u8; 3] = [0, 1, 2];

/// One step of a two-sided stream: a left-table change and a right-table change.
type PairStep = (ZSet<(u8, u8)>, ZSet<(u8, u8)>);

/// SplitMix64. A few lines so the crate does not grow a dependency, and so a
/// printed seed is enough to rebuild the exact stream.
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn choose(&mut self, n: usize) -> usize {
        (self.next() as usize) % n
    }
}

/// One change that cannot take `table` negative.
fn next_change(rng: &mut Rng, table: &ZSet<(u8, u8)>) -> ZSet<(u8, u8)> {
    if rng.choose(5) == 0 {
        return ZSet::new();
    }
    let mut change = ZSet::new();
    let present: Vec<(u8, u8)> = table.iter().map(|(row, _)| *row).collect();
    let delete = !present.is_empty() && rng.choose(2) == 0;
    if delete {
        change.add_row(present[rng.choose(present.len())], -1);
    } else {
        let key = KEYS[rng.choose(KEYS.len())];
        let value = VALUES[rng.choose(VALUES.len())];
        change.add_row((key, value), 1);
    }
    change
}

/// A stream of valid changes to one table, determined by `seed`.
pub(crate) fn unary_stream(seed: u64, steps: usize) -> Vec<ZSet<(u8, u8)>> {
    let mut rng = Rng::new(seed);
    let mut table = ZSet::new();
    let mut stream = Vec::with_capacity(steps);
    for _ in 0..steps {
        let change = next_change(&mut rng, &table);
        table = table + change.clone();
        stream.push(change);
    }
    stream
}

/// A stream of valid left/right changes, determined by `seed`.
pub(crate) fn pair_stream(seed: u64, steps: usize) -> Vec<PairStep> {
    let mut rng = Rng::new(seed);
    let mut left = ZSet::new();
    let mut right = ZSet::new();
    let mut stream = Vec::with_capacity(steps);
    for _ in 0..steps {
        let d_left = next_change(&mut rng, &left);
        let d_right = next_change(&mut rng, &right);
        left = left + d_left.clone();
        right = right + d_right.clone();
        stream.push((d_left, d_right));
    }
    stream
}

/// Shortest prefix length that still fails.
///
/// Incremental mismatches are not monotonic: a later retraction can make a
/// longer prefix agree with the recompute again. Binary search would skip a
/// short failure that sits before a passing range, so this walks from length 1.
pub(crate) fn shortest_failing_prefix(max: usize, fails: impl Fn(usize) -> bool) -> Option<usize> {
    if !fails(max) {
        return None;
    }
    (1..=max).find(|&len| fails(len))
}

fn mismatch_message(operator: &str, test: &str, seed: u64, steps: usize, prefix: usize) -> String {
    format!(
        "{operator} on seed {seed} disagreed with a full recompute at {prefix} step(s) (stream was {steps})\nrerun: CARGO_TARGET_DIR=target cargo test -- --exact {test}"
    )
}

fn panic_mismatch(operator: &str, test: &str, seed: u64, steps: usize, prefix: usize) -> ! {
    panic!("{}", mismatch_message(operator, test, seed, steps, prefix));
}

fn map_incremental(change: &ZSet<(u8, u8)>) -> ZSet<u8> {
    change.map(|(_, value)| *value)
}

/// Project each row to its value by looping. Shares no code with [`ZSet::map`].
fn map_reference(table: &ZSet<(u8, u8)>) -> ZSet<u8> {
    let mut out = ZSet::new();
    for ((_, value), weight) in table.iter() {
        out.add_row(*value, weight);
    }
    out
}

fn filter_incremental(change: &ZSet<(u8, u8)>) -> ZSet<(u8, u8)> {
    change.filter(|(_, value)| *value > 0)
}

/// Keep rows whose value is positive by looping. Shares no code with [`ZSet::filter`].
fn filter_reference(table: &ZSet<(u8, u8)>) -> ZSet<(u8, u8)> {
    let mut out = ZSet::new();
    for (row, weight) in table.iter() {
        if row.1 > 0 {
            out.add_row(*row, weight);
        }
    }
    out
}

/// Nested-loop equality join on two finished tables. Shares no code with [`Join::step`].
fn join_reference(left: &ZSet<(u8, u8)>, right: &ZSet<(u8, u8)>) -> ZSet<(u8, u8, u8)> {
    let mut out = ZSet::new();
    for ((left_key, left_row), left_weight) in left.iter() {
        for ((right_key, right_row), right_weight) in right.iter() {
            if left_key == right_key {
                out.add_row(
                    (*left_key, *left_row, *right_row),
                    left_weight * right_weight,
                );
            }
        }
    }
    out
}

fn map_matches(changes: &[ZSet<(u8, u8)>]) -> bool {
    let mut table = ZSet::new();
    let mut out = ZSet::new();
    for change in changes {
        out = out + map_incremental(change);
        table = table + change.clone();
    }
    out == map_reference(&table)
}

fn filter_matches(changes: &[ZSet<(u8, u8)>]) -> bool {
    let mut table = ZSet::new();
    let mut out = ZSet::new();
    for change in changes {
        out = out + filter_incremental(change);
        table = table + change.clone();
    }
    out == filter_reference(&table)
}

fn join_matches(changes: &[PairStep]) -> bool {
    let mut join = Join::new();
    let mut left = ZSet::new();
    let mut right = ZSet::new();
    let mut out = ZSet::new();
    for (d_left, d_right) in changes {
        out = out + join.step(d_left.clone(), d_right.clone());
        left = left + d_left.clone();
        right = right + d_right.clone();
    }
    out == join_reference(&left, &right)
}

/// Incremental join that drops `dL ⋈ dR`. Wrong exactly when both sides
/// introduce a matching key in the same step.
fn broken_join_matches(changes: &[PairStep]) -> bool {
    let mut left = ZSet::new();
    let mut right = ZSet::new();
    let mut out = ZSet::new();
    for (d_left, d_right) in changes {
        out = out + join_reference(d_left, &right) + join_reference(&left, d_right);
        left = left + d_left.clone();
        right = right + d_right.clone();
    }
    out == join_reference(&left, &right)
}

fn assert_unary(
    operator: &str,
    test: &str,
    seed: u64,
    matches: impl Fn(&[ZSet<(u8, u8)>]) -> bool,
) {
    let stream = unary_stream(seed, STEPS);
    if matches(&stream) {
        return;
    }
    let prefix = shortest_failing_prefix(stream.len(), |len| !matches(&stream[..len]))
        .expect("full stream failed so a prefix must");
    panic_mismatch(operator, test, seed, STEPS, prefix);
}

fn assert_join(operator: &str, test: &str, seed: u64, matches: impl Fn(&[PairStep]) -> bool) {
    let stream = pair_stream(seed, STEPS);
    if matches(&stream) {
        return;
    }
    let prefix = shortest_failing_prefix(stream.len(), |len| !matches(&stream[..len]))
        .expect("full stream failed so a prefix must");
    panic_mismatch(operator, test, seed, STEPS, prefix);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property the checker exists to test: adding up per-step outputs
    /// equals running the obvious query on the tables at the end.
    #[test]
    fn map_on_random_streams_matches_a_full_recompute() {
        for &seed in SEEDS {
            assert_unary(
                "map",
                "checker::tests::map_on_random_streams_matches_a_full_recompute",
                seed,
                map_matches,
            );
        }
    }

    #[test]
    fn filter_on_random_streams_matches_a_full_recompute() {
        for &seed in SEEDS {
            assert_unary(
                "filter",
                "checker::tests::filter_on_random_streams_matches_a_full_recompute",
                seed,
                filter_matches,
            );
        }
    }

    #[test]
    fn join_on_random_streams_matches_a_full_recompute() {
        for &seed in SEEDS {
            assert_join(
                "join",
                "checker::tests::join_on_random_streams_matches_a_full_recompute",
                seed,
                join_matches,
            );
        }
    }

    /// Dropping `dL ⋈ dR` is the bug the join formula exists to catch. Seed 7
    /// is a named case: the checker must fail it, and the prefix must be short
    /// enough to read.
    #[test]
    fn dropping_the_same_step_join_term_fails_on_seed_7() {
        const SEED: u64 = 7;
        let stream = pair_stream(SEED, STEPS);
        assert!(
            join_matches(&stream),
            "the real join must still pass seed {SEED}"
        );
        assert!(
            !broken_join_matches(&stream),
            "seed {SEED} should expose a missing dL ⋈ dR term"
        );
        let prefix =
            shortest_failing_prefix(stream.len(), |len| !broken_join_matches(&stream[..len]))
                .expect("the broken join failed the full stream");
        assert!(
            prefix <= 8,
            "shortest failing prefix was {prefix} steps, too long to read"
        );
        // Same text panic_mismatch prints when a real operator fails.
        let message = mismatch_message(
            "join (broken)",
            "checker::tests::dropping_the_same_step_join_term_fails_on_seed_7",
            SEED,
            STEPS,
            prefix,
        );
        assert!(
            message.contains(
                "CARGO_TARGET_DIR=target cargo test -- --exact checker::tests::dropping_the_same_step_join_term_fails_on_seed_7"
            ),
            "failure message must contain a command that reruns this case:\n{message}"
        );
    }

    #[test]
    fn shortest_failing_prefix_is_the_first_length_that_fails() {
        // Length 2 fails, 3..=9 pass, 10+ fail. The shortest is 2, not 10.
        let prefix = shortest_failing_prefix(50, |len| len == 2 || len >= 10);
        assert_eq!(prefix, Some(2));
    }
}
