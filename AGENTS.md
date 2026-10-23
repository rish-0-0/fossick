# AGENTS.md — how to work on fossick

You are working on **fossick**: an open-source log indexer and search engine
being built, module by module, to the quality bar of Splunk. This file is the
contract for every coding agent (any model, any tool) that touches this repo.
Read it fully before writing code. When in doubt, follow this file over your
own instincts.

## The one rule: Depth First

We do **not** build broad and shallow. We build one module at a time, and a
module is not "done" until it is:

1. **Complete** — handles the real cases, not the happy path.
2. **Correct** — unit tests prove the behavior, including edge cases and
   crash/recovery paths where relevant.
3. **Integrated** — integration tests prove it works with the modules around
   it (see `fossick/tests/e2e.rs` and `fossick-spl/tests/spl.rs` for the
   existing patterns).
4. **Optimized** — measured, not guessed. Hot paths are allocation-free where
   it matters, I/O is batched, readers have bounded memory. See the `perf`
   skill before optimizing anything.
5. **Documented** — module-level `//!` doc comment stating the on-disk layout
   or protocol if there is one, README/docs updated, known gaps written down
   ("documented, not silent" — see the SPL gaps list in README.md for the
   style).

If you are asked to build something big, do NOT scaffold five half-finished
modules. Pick the first module, finish it to the bar above, then move on.
Use the `breakdown` skill to turn big asks into a sequence of finishable
modules first.

## Repo map

Rust workspace of three crates plus a React UI. ~6k lines of Rust total —
you can and should read the whole module you're changing before editing it.

| Path                     | What it is |
| ------------------------ | ---------- |
| `fossick/`               | Daemon + CLI: conf parsing, TCP ingest, timestamp extraction, REST search head (`http.rs`, hand-rolled HTTP/1.1 on 127.0.0.1), SPL→pushdown planning (`spl.rs`), bucket scanning (`search.rs`), `loggen` test-data generator |
| `fossick-index/`         | Storage engine: bucket lifecycle (`bucket.rs`), zstd journal (`journal.rs`), event codec, tokenizer, time-series index (`tsidx.rs`), bloom filters (`bloom.rs`), manifest with crash-state machine (`manifest.rs`) |
| `fossick-spl/`           | SPL language: lexer/parser (`parse.rs`), expression eval (`expr.rs`), push-based executor (`exec.rs`), typed values (`value.rs`) |
| `ui/`                    | Vite + React + TypeScript. Built bundle in `ui/dist` is embedded into the binary by `fossick/build.rs` |
| `ROADMAP.md`             | The phased plan. Every piece of work should trace to a phase |
| `docs/plans/`            | Design docs / breakdowns for in-flight work (created by the `breakdown` skill) |
| `.claude/skills/`        | Executable skills (see below). If your tool doesn't load skills automatically, read them as docs — they are plain markdown |

## Search algorithm (so you don't have to reverse-engineer it)

Every search goes through the **search head**, never the storage layer
directly:

1. The head parses the SPL, resolves the time range (`earliest=`/`latest=`
   in SPL beat UI params; `latest` clamped to now), and derives a safe
   **index pushdown**: conjunctive terms + indexed-field equalities.
2. Bucket pruning: buckets whose `[min_time_ms, max_time_ms]` miss the time
   range are skipped. For warm buckets, the **bloom filter** (`bloom.bf`,
   ~10 bits/term, k=7) rejects buckets that can't contain a pushdown term.
3. Within a surviving warm bucket, the **tsidx** (`index.tsidx`, mmap'd
   term dictionary → delta-varint postings of event ordinals) is
   intersected lazily to pick candidate events; `events.tbl` maps ordinals
   to journal addresses; `journal.zj` (independently-compressed zstd
   blocks, 256 KiB / 2048-event bound per block) yields the raw events.
   Hot buckets are scanned without tsidx/bloom.
4. Decoded events feed the SPL executor, which **re-verifies the full
   expression**. This is a load-bearing invariant: pushdown only ever
   narrows I/O, never semantics. A pushdown bug can make search slow; it
   must never make search wrong.

## Non-negotiable invariants

Violating any of these is a rejected change, no matter how good the feature:

- **On-disk formats are versioned contracts.** Every file has a magic +
  version (`FZJ1`, `FTX1`, `FBL1`, `FET1`). Changing a layout means a new
  version, a reader that still reads the old version, and a test that opens
  an old-format fixture. Never silently change what bytes mean.
- **Pushdown narrows I/O, never semantics.** The executor is the source of
  truth. Any index/bloom/tsidx change needs a test proving results are
  identical with and without the pushdown.
- **Crash safety.** On-disk state must be readable at every instant: the
  journal appends per block flush, `events.tbl` only ever points at flushed
  blocks, the manifest state machine drives recovery. If your change can
  leave a torn state, it's wrong.
- **Bounded reader memory.** No reader may need more than one decompressed
  block (256 KiB) plus O(results) memory. No "read the whole file into RAM".
- **Dependency-light.** Every new crate/npm dependency must be justified in
  the PR/commit message. Prefer 50 lines of our own code over a 50k-line
  dependency. Current Rust deps are deliberately few (zstd, memmap2, ...).
- **The binary is self-contained.** UI is embedded at build time; the server
  binds 127.0.0.1 only (auth/TLS are future phases — do not expose beyond
  loopback until that work lands).
- **No `unsafe`** without a written justification comment and a test
  exercising the invariant it relies on. No `unwrap()`/`expect()` on the
  serve/ingest paths — errors are handled or propagated; `unwrap` is fine in
  tests and in code paths with proven invariants (document the proof).

## The workflow for every task

1. **Read before writing.** Read the module you're changing end to end, plus
   its tests, plus the module-level `//!` comment. The files are small; no
   excuses.
2. **State your plan** in one short paragraph: what you'll change, what test
   proves it, what you will NOT touch. If the task is ambiguous, ask —
   don't pick an interpretation silently.
3. **Test first.** Write the failing test that defines "done" before the
   implementation. Bug fix = a test that reproduces the bug first.
4. **Implement the minimum.** No speculative flexibility, no abstractions
   for single-use code, no touching adjacent code "while you're there".
   Every changed line must trace to the task.
5. **Verify with the ladder** (the `run-tests` skill is the executable
   version):
   ```sh
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   cd ui && npm test   # if you touched the UI
   ```
6. **Drive it end to end** for anything with a runtime surface (the
   `verify` skill): serve + loggen + a real query through the API.
7. **Update docs**: README feature lists, module `//!` comments, ROADMAP
   checkboxes. If you left a gap, write it in the "Known gaps" list — gaps
   are fine, silent gaps are not.

## Code style

- Match the existing code. It favors: small modules with a `//!` header
  documenting the format/protocol, plain `std` over frameworks, explicit
  byte-level encode/decode with tests, push-based iteration over
  materializing vectors.
- Comments state constraints the code can't ("count in the header is fixed
  up at finalize"), never narrate the code or your diff.
- Errors: `io::Result` at the storage layer, typed errors where callers
  branch on them. Corrupt input is an error, never a panic.
- Naming and formatting: whatever `cargo fmt` and the surrounding file do.

## Skills — use them

Executable playbooks live in `.claude/skills/<name>/SKILL.md`. They are the
distilled "how we do X here". Before doing X, read the skill for X:

| Skill | Use when |
| ----- | -------- |
| `depth-first`     | Starting ANY feature work — the module completion ladder in checklist form |
| `breakdown`       | The task is bigger than one module / one week — decompose it into a plan doc |
| `run-tests`       | Verifying a change — the full lint/test/coverage ladder with pass criteria |
| `verify`          | Proving a change works end to end in the running app |
| `security-audit`  | Periodically, and ALWAYS before adding deps or touching ingest/HTTP/zip parsing |
| `perf`            | Before and after optimizing anything — measurement discipline |
| `extend`          | Adding a conf section, file format, API endpoint, index type, or app-platform surface |
| `new-spl-command` | Adding or extending an SPL command — exact files to touch, in order |

If a skill told you something that turned out wrong or incomplete, **fix the
skill in the same PR**. Skills must improve as the project grows.

## What "done" means (paste this into your plan)

- [ ] Failing test written first, now passing
- [ ] `cargo fmt` / `clippy -D warnings` / `cargo test --workspace` clean
- [ ] Integration test with neighboring modules (or existing e2e extended)
- [ ] Crash/corrupt-input behavior tested (storage/ingest changes)
- [ ] Old-format fixtures still readable (format changes)
- [ ] Pushdown-equivalence test (index/search-planning changes)
- [ ] Measured before/after numbers (perf changes)
- [ ] Docs + Known Gaps + ROADMAP updated
- [ ] No new deps, or each one justified in the commit message
