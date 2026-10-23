---
name: new-spl-command
description: Exact recipe for adding or extending an SPL command (or stats/eval function) in fossick-spl — which files to touch in which order, the executor contract, and the required tests. Use for any SPL language-surface work.
---

# new-spl-command — the recipe

The SPL engine is `fossick-spl`: `parse.rs` (text → `Cmd`), `exec.rs`
(`Cmd` → `Stage`, push-based execution), `expr.rs` (eval expressions),
`value.rs` (typed values). Read the `//!` header of `exec.rs` first — the
push/feed/finish contract drives everything.

Decide what you're adding:
- **Pipeline command** (`| mycommand ...`) → full recipe below.
- **Stats function** (`stats myfn(x)`) → `Agg` enum in `exec.rs` + agg
  parsing in `parse.rs` (`parse_stats`), same test rules.
- **Eval function** (`eval y=myfn(x)`) → `expr.rs` function table + eval
  tests; no executor changes.

## Step 1 — Specify against Splunk semantics

Write, in the test file as doc text or in your plan: the exact syntax
accepted, behavior on missing fields, null handling, interaction with
`by` clauses, and the relevant conf limit. Match Splunk behavior unless
there's a written reason not to; name the deviation in README's SPL
section.

## Step 2 — `Cmd` variant (`exec.rs`)

Add the variant to `pub enum Cmd`. Keep it **state-free parsed form** —
all runtime state lives in the `Stage` you build from it.

## Step 3 — Parser (`parse.rs`)

Add the `"mycommand" => ...` arm in `parse_command` (names match
case-insensitively, like `head`/`tail`). Reuse the existing helpers
(word lists, `limit=`, quoted strings) — don't hand-roll tokenizing.
Bad syntax → `Err` with a message naming the command and what was
expected; never a panic.

**Parser unit tests first** (bottom of `parse.rs`, see the existing
`Cmd::` matching style): happy path, defaults, each option, and at least
two malformed inputs asserting clean errors.

## Step 4 — Stage (`exec.rs`)

Add a `Stage` variant and its arm in the `Cmd → Stage` builder, then
implement the contract — this is the part that's easy to get wrong:

- **Streaming command** (transforms records one by one: like `rex`,
  `rename`): transform in `feed`, pass through.
- **Blocking command** (needs all input: like `stats`, `sort`, `tail`):
  buffer in `feed`, emit in `finish`. Buffers MUST respect a `Limits`
  cap (add a conf-backed limit if none fits) and set `truncated` when
  they clip — unbounded buffering is a rejected change.
- **Early termination**: if no further input can change your output
  (like `head` after N), return `false` from `feed` so the scan stops.
  Getting this right is a real perf feature; test it.
- Field-name bookkeeping: if your command creates/removes columns, make
  sure the output `Table.fields` reflects it (see how `Table`/`Stats`
  stages handle `fields`).

## Step 5 — Pipeline tests (`fossick-spl/tests/spl.rs`)

End-to-end: feed fixture records through a full pipeline string and
assert the output table. Cover: alone, composed before AND after an
aggregating command, empty input, missing fields, the limit/truncation
path, and the early-termination behavior if any.

## Step 6 — Head integration (usually nothing, verify anyway)

`fossick/src/spl.rs` derives the index pushdown from the base search
only — pipeline commands don't affect it. But if your command can
FILTER records (like `regex`), confirm the executor re-verification
still holds: results must be identical whether or not pushdown pruned
(add a case to `fossick/tests/e2e.rs` with a query shape that defeats
pushdown). If your command needs head cooperation (time ranges, job
model), that's a design question — plan doc first.

## Step 7 — Document + verify

- README.md SPL section: add the command/function to the supported list;
  remove it from Known Gaps if it was listed there.
- `run-tests` ladder, then `verify` end to end:
  `cargo run -p fossick -- search "error | mycommand ..."` and the same
  via `curl` on `/api/search`.
