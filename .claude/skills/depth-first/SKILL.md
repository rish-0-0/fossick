---
name: depth-first
description: The fossick module-completion ladder. Use when starting ANY feature or module work in this repo — it defines the order of operations (understand → specify → test-first → implement → optimize → integrate → document) and the definition of done.
---

# Depth-first module development

fossick is built one module at a time to Splunk quality. "Half of everything"
is worth nothing; "all of one module" compounds. This skill is the ladder you
climb for every piece of feature work. Do the steps in order; don't skip.

## Step 0 — Scope check

If the task spans more than one module or more than ~1 week of work, STOP
and run the `breakdown` skill first. You should arrive here holding one
module-sized, week-or-less milestone with exit criteria already written.

## Step 1 — Understand (read, don't skim)

- Read the whole module you'll touch, its `//!` header, and its tests.
  Every Rust file here is < 1200 lines; read all of it.
- Read the neighbors it talks to (e.g. touching `tsidx.rs`? read how
  `bucket.rs` writes it and `fossick/src/search.rs` reads it).
- Write down, in your plan: the invariants you found (crash safety,
  format magics/versions, memory bounds) that your change must preserve.

## Step 2 — Specify

One short paragraph before any code:
- What changes, in which files.
- What test will prove it works (name the test file).
- What you will NOT touch.
- Which AGENTS.md invariants are in play.

If two interpretations of the task exist, present both and ask. Do not
pick silently.

## Step 3 — Test first

Write the failing test before the implementation:

- Unit tests live next to the code (`#[cfg(test)] mod tests`).
- Cross-module behavior goes in `fossick/tests/e2e.rs` or
  `fossick-spl/tests/spl.rs` — extend the existing patterns.
- Bug fix? The reproducing test comes first, and it must fail on the old
  code.
- Storage change? Add the crash/corruption case: truncate the file, flip a
  byte, reopen — assert error-not-panic and recovery where promised.
- Format change? Keep a fixture of the OLD format in the test and assert
  it still reads.

Run it, watch it fail for the right reason:

```sh
cargo test -p <crate> <test_name>
```

## Step 4 — Implement the minimum

- Smallest change that makes the test pass. No speculative config, no
  single-use abstractions, no drive-by refactors of adjacent code.
- Match the module's style: explicit byte-level encode/decode, plain std,
  push-based iteration, `io::Result` at the storage layer.
- Corrupt input is an `Err`, never a panic. No `unwrap()` on serve/ingest
  paths.

## Step 5 — Optimize (only with numbers)

Correct first, fast second — but a module isn't DONE until it's fast.
Follow the `perf` skill: measure a baseline, change one thing, measure
again, keep the numbers in the commit message. Respect the standing perf
invariants: alloc-free hot loops (ingest tokenize/append, search match),
no fsync on the hot path, bounded reader memory (one 256 KiB block).

## Step 6 — Integrate and verify

Run the full ladder (the `run-tests` skill is the executable version):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Then drive it live via the `verify` skill if it has any runtime surface
(serve + loggen + real API query).

## Step 7 — Document

- Update the module `//!` header if the format/protocol changed.
- Update README.md feature lists and the Known Gaps list — gaps are fine,
  silent gaps are rejected.
- Check the item off in ROADMAP.md / the plan doc in `docs/plans/`.
- If a skill (including this one) gave you stale guidance, fix the skill
  in the same change.

## Definition of done (all boxes, every time)

- [ ] Failing test written first, now passing
- [ ] fmt + clippy(-D warnings) + full workspace tests clean
- [ ] Integration/e2e coverage for cross-module behavior
- [ ] Crash/corrupt-input tests (storage/ingest changes)
- [ ] Old-format fixture still reads (format changes)
- [ ] Pushdown-equivalence test (index/planner changes)
- [ ] Before/after numbers (perf changes)
- [ ] Docs, Known Gaps, ROADMAP updated
- [ ] Zero unjustified new dependencies
