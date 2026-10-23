---
name: run-tests
description: The executable verification ladder for fossick — format, lint, unit/integration/e2e tests, and coverage, with pass criteria. Use after every change and before claiming any work is done.
---

# run-tests — the verification ladder

Run these in order. Each rung must be clean before you claim the change
works. Don't cherry-pick a single test and call it verified.

## 1. Format + lint (fast, run constantly)

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Pass = zero output / zero warnings. Never fix a clippy warning with
`#[allow]` unless the lint is genuinely wrong for the code — and then say
why in a comment.

## 2. Targeted tests (while iterating)

```sh
cargo test -p fossick-index            # storage engine
cargo test -p fossick-spl              # SPL unit + tests/spl.rs pipeline
cargo test -p fossick                  # daemon + tests/e2e.rs
cargo test -p <crate> <name_substring> # one test while red/green looping
```

## 3. Full workspace (before every commit)

```sh
cargo test --workspace
```

Pass = every test green. A flaky test is a bug — fix it or file it in
Known Gaps; never rerun-until-green.

## 4. UI (only if you touched `ui/`)

```sh
cd ui
npm test            # vitest unit tests (no DOM/snapshot tests by design)
npm run build       # must succeed — the bundle embeds into the binary
cd ..
cargo build -p fossick   # re-embed the fresh bundle
```

## 5. Coverage (when finishing a module, not every edit)

Needs `cargo install cargo-llvm-cov` once.

```sh
cargo llvm-cov --workspace             # terminal summary
cargo llvm-cov --workspace --html      # target/llvm-cov/html/index.html
cd ui && npm run coverage              # ui/coverage/
```

Bar: the module you just finished should have its logic paths covered —
aim for every branch of encode/decode, every manifest state transition,
every SPL command's happy + error path. Don't chase 100% on glue code;
do chase 100% on byte-level format code.

## 6. End to end

If the change has a runtime surface (ingest, search, API, UI), finish with
the `verify` skill — tests passing is necessary, watching it work is
sufficient.

## Reporting

Report honestly: paste the failing output if anything fails, say which
rungs you ran and which you skipped and why. "Tests pass" with rungs
silently skipped is a broken report.
