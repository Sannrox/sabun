# sabun

A small incremental view maintenance engine, built to learn how one works.

Define a query once. When an input row changes, compute only the change to the
result instead of running the query again.

## Stages

| Stage | Adds | What it teaches |
|-------|------|-----------------|
| 1 (done) | Z-sets, `map`, `filter` | Data and changes are the same type; some queries can run on the change alone |
| 2 (done) | Join | A join cannot run on the change alone: it needs memory of both sides |
| 3 (done) | Checker | Random change streams: the incremental result must equal a full recompute |
| 4 (done) | Group and aggregate | An update to one row retracts an old result and emits a new one |
| 5 | Distinct | Output changes only when a count crosses zero |
| 6 | Circuits | Wiring operators into a graph that is fed one change at a time |
| 7 | Recursion | Reachability in a graph, kept current under edge deletes |

## Run

```sh
CARGO_TARGET_DIR=target cargo test
```

## License

Apache-2.0
