---
name: extend
description: How to extend fossick without breaking it — recipes for adding a conf section, an on-disk format (or changing one), an API endpoint, an index-time field, and app-platform surfaces. Use whenever you add a new extension point or evolve an existing contract.
---

# extend — growing fossick without breaking it

fossick must stay extendible at every point in its life. That works only if
every contract (file bytes, conf keys, API JSON, app manifests) evolves by
**addition with compatibility**, never by silent mutation. These are the
recipes.

## The general law

1. **Contracts are versioned.** Files carry magic+version; API responses
   grow new keys but never change existing key meanings; conf unknown-key
   handling is deliberate (warn, don't crash — old binaries must survive
   new conf files where feasible).
2. **New binary, old data: always works.** Every release must open every
   previous release's data dir. Format changes keep the old reader and add
   a fixture test (a checked-in old-format file that must keep decoding).
3. **Old binary, new data: fails loudly.** Unknown version → clear error
   naming the version, never garbage output.
4. **One module, one owner.** An extension point is a narrow, documented
   interface (the `//!` header). If extending X forces edits in 4 crates,
   the boundary is wrong — fix the boundary first, flag it to the
   maintainer.

## Recipe: change / add an on-disk format

1. Read the writer AND every reader of the file first.
2. New file → new 4-byte magic + u16 version, documented in the module
   `//!` header (layout table, like `journal.rs`/`tsidx.rs` do).
   Changed file → bump the version; reader matches on version and keeps
   decoding v(n-1).
3. Tests: round-trip at new version; checked-in old-version fixture still
   reads; truncated + bit-flipped file errors cleanly (see
   `security-audit`).
4. Update `fossick fsck` expectations once it exists (Phase 1).

## Recipe: add a conf section/key

1. All conf lives in `fossick/src/conf.rs` — add parsing there, with a
   default so absent = today's behavior (new keys must be opt-in).
2. Document the key in README's conf paragraph and in the parsing code.
3. Tests: default applies when absent; value parses; garbage value → clear
   error with the section/key named.

## Recipe: add an API endpoint

1. Routes live in `fossick/src/http.rs`. Keep the shape: handler returns
   typed JSON; cells/values typed (string/number/null), never stringly.
2. Additive responses only: new fields OK, renaming/retyping existing
   fields is a breaking change — needs maintainer sign-off and a `/api/v2`
   style escape hatch, not a silent edit.
3. Malformed input → 4xx with a JSON error body; verify with a curl of the
   broken request (see `verify`). Limits (size, result caps) from conf.
4. UI client calls go through `ui/src/api.ts` — extend it and its tests.

## Recipe: add an indexed field / tokenizer change

Index-time behavior changes what's IN existing indexes vs. new ones.
1. The change must be conf-driven (`[indexed_fields]`) — never hardcoded.
2. Search must stay correct over buckets indexed under the OLD config:
   pushdown may only use an indexed field if the bucket's manifest says
   that bucket indexed it. Test: old bucket + new conf + query → correct
   results via scan fallback.

## Recipe: app platform surfaces (Phase 3+)

The app platform is fossick's main extension story — like Splunk TAs.
Apps are data, not code (v1): manifest + static assets + conf + saved
searches, installed from .zip.
1. Anything an app can contribute (nav entry, dashboard, lookup, saved
   search) is a declared, schema-validated manifest capability — apps
   never get arbitrary server-side hooks.
2. Manifest schema is versioned (`manifest_version`); the server rejects
   newer-than-known versions loudly and accepts all older ones forever.
3. Every new capability ships with: schema validation tests, a demo-app
   fixture zip exercising it, uninstall leaving zero residue, and a pass
   of the `security-audit` zip checklist.
4. Core apps (Search, Entity Graph) must use the same registration surface
   as installed apps — if a core app needs a private hook, the platform is
   too weak; strengthen the platform instead.

## Before you invent a new extension point

Ask: is this generality the maintainer asked for, or speculative? A second
concrete use case is the bar for an abstraction. One use case = write it
concrete, note the future extension in ROADMAP instead. When you DO add an
extension point, add its recipe to this skill.
