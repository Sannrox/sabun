# Repository Guidelines

`sabun` is a small incremental view maintenance engine, built to learn how one
works. It is a study project: clarity beats speed, and every stage has to be
readable by someone who has not seen the idea before.

## Project Structure & Module Organization

A Rust 2024 library crate with no dependencies.

- `src/zset.rs`: collections with signed row counts. Every other module builds
  on it.
- `src/` gains one module per operator (join, aggregate, distinct) and one for
  wiring operators together. The stage table in `README.md` is the plan.
- Unit tests sit beside the code they test. `tests/` holds tests that span
  operators.

## Dependencies and Ownership

This repository owns everything in it and depends on nothing. Adding a
dependency needs a reason written in the Issue: the point of the project is to
build the parts, not to import them. Shared agent workflows are owned by
[Sannrox/agent-kit](https://github.com/Sannrox/agent-kit); fix them there.

## Build, Test, and Development Commands

- `cargo fmt` formats the code.
- `CARGO_TARGET_DIR=target cargo test` runs the tests in this checkout's own
  target directory.
- `cargo clippy --all-targets --locked -- -D warnings` matches CI.
- `bash scripts/validate-workflow-skills.sh` checks the agent-kit stubs.

GitHub Issues are the planning source of truth. Shared Skills are stubs under
`.agents/skills/`.

## Agent delivery closeout (required)

For a change to behavior or a public type that will be committed, pushed, or
opened as a Pull Request: run the three commands above through the
`verify-change` Skill, then run the shared `autoreview` skill and fix or
consciously reject each finding. Report the commands run and the review result
in the Pull Request. Documentation typos and pure formatting may skip review;
say so.

## Parallel delivery lanes

A delivery lane is one Issue, one branch, one isolated checkout, one Pull
Request, and one owner. Several agents may deliver several Issues at the same
time only as separate lanes.

### Claims live on GitHub

An Issue is claimed when any of these exists: an open Pull Request that
references it; a branch `<type>/<issue>` or matching `*/<issue>-*`; the
authenticated login assigned to it less than six hours ago. A lane claims
before it implements with
`bash .agents/skills/deliver-ready-issue/scripts/issue-lane.sh claim <issue>`.
`<type>` is the Conventional Commit type in the Issue title, otherwise the type
label, otherwise `chore`.

### Isolation on each machine

Each lane checks out its claim branch in `.worktrees/issue-<issue>`, ignored
by Git, and uses that worktree's own `target/`. The primary checkout belongs
to the maintainer: agents never switch its branch, reset it, stash it, or run
long jobs in it.

### Limits, collisions, and landing

At most three lanes run at once. The collision surfaces are the public API of
`ZSet` in `src/zset.rs` and the operator trait once it exists: lanes that would
both change one of them run in sequence. Landing is sequential and by squash
merge; after each merge the remaining lanes recheck against the new `main`.

Public lane fields list only the claim branch, the repo-relative worktree,
base SHA and published SHA.

## Coding Style & Naming Conventions

Standard `cargo fmt`. Concrete types by default; a generic or a trait is added
when a second real use exists, and the commit says which. Library code returns
typed errors and does not `unwrap`. Every public item has a doc comment that
explains the idea, not only the signature: the comments are part of what this
project is for. Keep them current when the code changes.

## Testing Guidelines

Tests are deterministic: no clock, no network, no files, no shared state, so
any number of copies can run at once. Randomized tests take an explicit seed
and print it on failure. Each operator has one test that states its key
property in plain words, and is covered by the checker that compares
incremental results with a full recompute.

## Commit & Pull Request Guidelines

Short imperative subjects in Conventional Commit style, for example
`feat(join): keep both sides indexed by key`. A Pull Request closes one Issue
and states the tests run and the review result.

## Security & Configuration Tips

Never commit secrets, hostnames, home directories, absolute paths or other
private environment details, and never put them on Issues, Pull Requests,
comments or commit messages.

## Agent skill profile

Repository facts the shared agent-kit skills read.

Feature: a new operator, or a new guarantee an existing operator gives.

### Compatibility and failure obligations

No released version and no stored format yet, so nothing to stay compatible
with. An operator must never emit a change that a full recompute would not
produce; a state it cannot handle is a typed error, not a wrong answer.

### Verification matrix

| Surface | Required evidence |
| --- | --- |
| an operator | its key-property test, and the checker passing on the fixed seeds |
| `ZSet` | every operator's tests, since all depend on it |
| documentation | the stage table in `README.md` matches what is built |

### Release audit

- no releases are planned; if one is, the stage table and the crate version agree.
