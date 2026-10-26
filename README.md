# fossick

A small, dependency-light log indexer and search engine (a mini-Splunk):
TCP ingest → hot/warm buckets with a tsidx + bloom index → SPL search →
web UI. Rust workspace of three crates plus a React UI:

| Path            | What it is                                                        |
| --------------- | ----------------------------------------------------------------- |
| `fossick/`      | Daemon + CLI: conf, TCP ingest, timestamp extraction, search head |
| `fossick-index/`| Bucket lifecycle, event codec, tokenizer, tsidx, bloom filters    |
| `fossick-spl/`  | SPL lexer, parser, and push-based executor                        |
| `ui/`           | Web UI: Vite + React + TypeScript, TanStack Table/Virtual         |

**Where this is going:** an open-source alternative to Splunk — indexer,
search head, SPL, and a UI shell whose core apps (Search, Entity Graph) sit
on the same app platform as user-installed `.zip` apps (like Splunk TAs).
The multi-month plan with per-phase exit criteria lives in
[ROADMAP.md](ROADMAP.md).

**How it's built:** depth-first — one module at a time, each finished to
production quality (complete, unit + integration tested, measured/optimized,
documented) before the next starts. Product lifecycle over raw speed of
delivery. [AGENTS.md](AGENTS.md) is the contract for anyone (human or
coding agent) working here; executable playbooks live in
[`.claude/skills/`](.claude/skills/).

## Quick start

```sh
# 1. Build the web UI (embedded into the fossick binary at compile time)
cd ui && npm install && npm run build && cd ..

# 2. Build and start fossick (UI + API on http://127.0.0.1:8089, ingest on 5514)
cargo run -p fossick -- serve

# 3. In another terminal, generate some sample events
cargo run -p fossick -- loggen --port 5514 --count 100000

# 4. Open http://127.0.0.1:8089/ and search, e.g.:
#      needle_3 | stats count avg(amount) by svc
# or use the CLI:
cargo run -p fossick -- search "error | top 5 svc"
```

Configuration is read from `--conf <path>`, else `$FOSSICK_CONF`, else
`./fossick.conf`, else built-in defaults. See `fossick/src/conf.rs` for the
sections (`[general]`, `[bucket]`, `[input:tcp:<port>]`, `[indexed_fields]`,
`[limits]`). `[limits] max_line_bytes` (default 1 MiB) caps how much of each TCP line is
indexed; longer lines are truncated. TCP lines are opaque bytes—invalid UTF-8 is stored as-is.

## Architecture notes

- Every search request hits the **search head** (`fossick/src/http.rs` +
  `fossick/src/spl.rs`), never the indexer directly. The head resolves and may
  rewrite the request (SPL `earliest=`/`latest=` beat the UI time-picker
  params, `latest` is clamped to now, result limits applied), derives a safe
  index **pushdown** (conjunctive terms + indexed-field equalities), and feeds
  decoded events through the SPL executor. The executor re-verifies the full
  expression, so pushdown only ever narrows I/O, never semantics.
- `GET /api/search?q=<spl>[&earliest=..&latest=..]` returns
  `{"fields": [...], "rows": [[...]], "isAggregated": bool, "truncated": bool,
  "matched": n, "tookMs": n}`. Cells are typed JSON (string/number/null) so the
  UI sorts numerically where it should.
- The UI bundle (`ui/dist`) is embedded into the binary by `fossick/build.rs`
  and served at `/`. Without a built UI you get a placeholder page.

## SPL support

Base search: bare terms (case-insensitive substrings of `_raw`), `*` globs,
quoted phrases, `field=value` (case-insensitive, globs allowed), `!=`,
`< <= > >=`, `field IN (a, b)`, `AND`/`OR`/`NOT` (implicit AND), parens,
`earliest=`/`latest=` (`-15m`, `now`, ISO-8601, epoch), `index=` (ignored).

Commands: `search`, `where`, `eval`, `fields [+|-]`, `table`, `rename`,
`sort [N] [-]field`, `head`, `tail`, `reverse`, `dedup [N] fields`,
`stats <aggs> [by ...]`, `top`/`rare [N] fields [by ...]`,
`rex [field=f] "(?<name>...)"`, `regex [field[!]=]"pat"`,
`fillnull [value=v] [fields]`.

Stats functions: `count`, `dc`, `sum`, `avg`/`mean`, `min`, `max`, `first`,
`last`, `values`, `list`, `stdev`, `median`, `range`, `percN`/`pN`.

Eval functions: `if case coalesce isnull isnotnull null now len lower upper
trim ltrim rtrim substr replace tonumber tostring typeof abs ceil floor round
sqrt exp ln log pow min max like match in pi`.

Known gaps (documented, not silent): no `timechart`/`bin`/`eventstats`/
`streamstats`/subsearches, no multivalue functions, no `strftime`/`strptime`
(`_time` is an ISO-8601 string, so lexicographic order is chronological).
Fields are auto-extracted from `_raw` as `key=value` at search time.
Ingest stores raw bytes; invalid UTF-8 is preserved on disk but search/API strings
use lossy conversion, so distinct invalid sequences may look identical in results.

## Tests

```sh
# Rust: unit + integration + e2e for all three crates (SPL tests live in
# fossick-spl — `src/*` unit tests and tests/spl.rs end-to-end pipeline tests)
cargo test --workspace

# Just the SPL engine
cargo test -p fossick-spl

# UI unit tests (vitest; no DOM/snapshot tests by design)
cd ui && npm test
```

## Coverage

Rust (needs the llvm-cov cargo plugin once: `cargo install cargo-llvm-cov`):

```sh
cargo llvm-cov --workspace            # summary in the terminal
cargo llvm-cov --workspace --html     # report in target/llvm-cov/html/index.html
cargo llvm-cov -p fossick-spl         # SPL engine only
```

UI (V8 coverage via `@vitest/coverage-v8`, already a dev dependency):

```sh
cd ui && npm run coverage             # summary + report in ui/coverage/
```

## Working on fossick

Read [AGENTS.md](AGENTS.md) first — the depth-first methodology, the
non-negotiable invariants (versioned on-disk formats, pushdown never changes
semantics, crash safety, bounded reader memory, dependency-light), and the
definition of done. Then:

1. Pick work from [ROADMAP.md](ROADMAP.md) (top unchecked item, lowest
   phase).
2. If it's bigger than a week, decompose it with the `breakdown` skill into
   a plan doc under `docs/plans/` and get it confirmed.
3. Build with the `depth-first` skill; verify with `run-tests` and `verify`;
   run `security-audit` when touching deps, ingest, HTTP, or (later) app-zip
   code; follow `extend` when evolving any contract and `new-spl-command`
   for SPL work; `perf` before/after any optimization.

Skills are plain markdown in `.claude/skills/<name>/SKILL.md` — tools that
don't auto-load them should read them as docs. If a skill is stale, fixing
it is part of the change that discovered it.

## UI development

`npm run dev` in `ui/` starts Vite on :5173 with `/api` proxied to a running
`fossick serve` on :8089 — hot reload against real data. `npm run build`
writes `ui/dist` with fixed asset names (`assets/app.js`, `assets/app.css`);
rebuild fossick afterwards to embed the new bundle.
