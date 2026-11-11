# Reachability under edge deletes

## Context

Reachability is a query that refers to its own result: `(a, c)` holds when
`(a, b)` already holds and there is an edge from `b` to `c`. Inserts have a
small incremental form. Deleting an edge does not: the same pair may still be
true because another route exists.

Two ways to handle a delete were considered.

### Retract what the edge could have supported, then derive again

Forget every derived pair that might have used the deleted edge, then run the
same join-and-distinct loop on the edges that remain. What still holds comes
back. What does not, stays retracted.

The honest form of that idea, on a graph this small, is to forget all derived
pairs and close the remaining edges from scratch. Over-approximating the
dependent set more tightly is the same algorithm with a smaller start set, not
a different idea.

### Track a change at every round, not only at every step

Treat each join-and-distinct round as its own change stream, and push a
retraction through those rounds the same way an insert is pushed. Deletes
would then be incremental too.

That needs a second time axis in the state (a change of a change) and it still
cannot count derivations: a cycle produces the same pair unboundedly often, so
the inner stream has to be a set, not a bag. The extra machinery does not
remove the reason deletes are hard.

## Decision

On any step that retracts a present edge, forget the derived pairs and close
the remaining edges with the insert loop. Emit the difference from what was
stored.

Insert-only steps stay semi-naive: join only the pairs found in the last round
with the current edges.

## Consequences

- A delete can redo work proportional to the whole closure. That is accepted:
  this project is for reading the idea, not for large graphs.
- One loop implements both halves. The delete path is a different *start*, not
  a second operator.
- A later recursive query would revisit this choice; this decision is only for
  reachability.

## Source

Issue #7.
