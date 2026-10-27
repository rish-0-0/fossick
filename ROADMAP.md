# ROADMAP — fossick, phase by phase

Goal: an open-source, maintainable alternative to Splunk — ingest, index,
search (SPL), and an extensible UI platform — built **depth-first**: each
module finished to production quality (complete + tested + integrated +
optimized + documented) before the next begins. See AGENTS.md for the
quality bar; use the `breakdown` skill to turn any phase item into a
week-by-week plan in `docs/plans/` before starting it.

Rules for working this roadmap:

- **One module in flight at a time.** Finish it (per the AGENTS.md "done"
  checklist), check it off here, then start the next.
- Phases are ordered by dependency, not importance. Don't start Phase N+1
  work that depends on unfinished Phase N modules.
- Items are deliberately written as *outcomes with exit criteria*, not
  designs. The design happens in `docs/plans/<item>.md` when you pick the
  item up — write the plan, get it confirmed, then build.
- When reality diverges from this file, update this file in the same PR.

## Phase 0 — Foundation (DONE)

Shipped: TCP ingest, timestamp extraction, hot/warm bucket lifecycle with
crash recovery, zstd journal, events.tbl, tsidx + bloom indexed search,
SPL parser/executor (search, where, eval, stats, top/rare, rex, sort, …),
pushdown planning, REST search head on 127.0.0.1:8089, embedded React UI,
loggen, e2e tests.

## Phase 1 — Indexer at production quality (~4–6 weeks)

Make the storage engine trustworthy with real data volumes and real time.

- [ ] **Cold buckets + retention**: warm→cold roll by age/size,
      size/age-based deletion, all driven by `[bucket]` conf.
      *Exit: fill past limits, verify oldest data expires, newest survives,
      search spans hot+warm+cold transparently; crash mid-roll recovers.*
- [ ] **Multiple indexes**: beyond `main` — `index=` in SPL becomes real,
      per-index conf (retention, indexed fields), ingest routing.
      *Exit: two indexes with different retention; `index=foo` searches only
      foo; default index conf-selectable.*
- [ ] **fsck / repair tool** (`fossick fsck`): detect and repair torn
      buckets, orphaned sidecars, bad trailers; report-only and repair modes.
      *Exit: a corruption test suite (truncate/bitflip fixtures) that fsck
      detects 100% of and repairs where possible.*
- [ ] **Ingest hardening**: back-pressure, max line length, bad-UTF8 policy,
      per-source metadata (host/source/sourcetype as indexed fields),
      graceful shutdown with zero event loss.
      *Exit: loggen at sustained high rate with kill -9 mid-stream loses at
      most the unflushed block; metadata fields searchable.*
      *Progress: graceful shutdown (SIGINT/SIGTERM drain + hot flush) shipped;
      max line length + UTF-8 store-as-is + per-source metadata already in place.*
- [ ] **Internal metrics**: ingest rate, bucket counts, search latency,
      exposed at `/api/metrics` and self-indexed into an `_internal` index.
      *Exit: `index=_internal | stats avg(ingest_eps)` works.*

## Phase 2 — Search at Splunk depth (~6–8 weeks)

Close the SPL gaps that matter most, and make search a real job model.

- [ ] **Time binning**: `bin`/`span`, then `timechart`.
      *Exit: `timechart span=1m count by svc` matches hand-computed results
      on a fixture dataset; UI renders it (table first, chart later).*
- [ ] **`eventstats` / `streamstats`.**
- [ ] **Lookups**: CSV lookup files + `lookup`/`inputlookup`.
- [ ] **Multivalue functions** (`mvcount`, `mvindex`, `mvexpand`, `split`).
- [ ] **Time functions**: real `_time` (epoch ms internally), `strftime`/
      `strptime`, relative-time eval. *This is a breaking representation
      change — needs its own plan doc and format-compat tests.*
- [ ] **Search jobs**: async job API (`POST /api/search/jobs`, poll status,
      fetch results in pages, cancel), result caching on disk, limits from
      conf. The UI switches from blocking GET to the job model.
      *Exit: a 10M-event search streams progress, is cancelable, and two
      concurrent searches don't starve each other.*
- [ ] **Search-time field extraction config**: per-sourcetype regex
      extractions (props/transforms-style, but one sane conf format).

## Phase 3 — UI shell + app platform (~6–8 weeks)

The UI becomes a shell hosting apps, like Splunk's app model.

- [ ] **UI shell**: app switcher, navigation chrome, shared time-picker and
      search-bar components as a documented internal component library.
      *Exit: Search is just the first app registered into the shell.*
- [ ] **App manifest + registry**: an app is a directory with `app.toml`
      (id, name, version, min fossick version, nav entries) + static assets
      + optional saved searches. Server enumerates `apps/` dir, serves
      assets, exposes `/api/apps`.
- [ ] **.zip app install**: upload a zip via UI/API, validated (manifest
      schema, size caps, **zip-slip path traversal checks**, no server-side
      code execution in v1 — apps are static assets + conf + saved searches
      only), installed to `apps/`, hot-registered.
      *Exit: a demo app zip installs, appears in the switcher, uninstalls
      cleanly; a malicious zip (traversal paths, oversize, bad manifest) is
      rejected with a clear error. Run the `security-audit` skill on this.*
- [ ] **Entity Graph app** (first bundled app beyond Search): entities
      (hosts, services, users) extracted from events via conf-defined
      rules, rendered as an interactive graph with time-range filtering.
      *Exit: loggen data produces a svc↔host graph; clicking a node drills
      into a search.*
- [ ] **Dashboards v1**: saved-search-backed panels (table + single value +
      line chart), dashboard definitions storable inside apps.

## Phase 4 — Operability & multi-user (~6–8 weeks)

- [ ] **AuthN/AuthZ**: local users, sessions, roles (admin/user), per-index
      read permissions. Only after this may the server bind beyond loopback.
- [ ] **TLS** for the API and ingest.
- [ ] **Saved searches + alerting**: cron-style scheduler, threshold
      conditions, webhook action first.
- [ ] **Config API + UI**: edit conf through the API with validation,
      reload without restart where safe.
- [ ] **Packaging**: single-binary releases for Linux/macOS/Windows,
      systemd unit, data-dir migration story, versioned upgrade tests
      (old data dir opens under new binary — always).

## Phase 5 — Scale out (future, design-first)

Distributed fossick: multiple indexers, a search head that fans out and
merges, replication. Do NOT start any Phase 5 code before Phases 1–4 are
done; do write design docs early. The module boundaries we keep clean today
(head vs. storage, pushdown as a serializable plan) are what makes this
phase possible later — protect them.

## How to pick up work (for every contributor, human or model)

1. Take the top unchecked item in the lowest unfinished phase (or the item
   the maintainer assigns).
2. Run the `breakdown` skill: produce `docs/plans/<item>.md` — scope, 
   non-goals, module boundaries, week-sized milestones each ending in a
   shippable, tested state, and the exit-criteria tests spelled out.
3. Get the plan confirmed by the maintainer.
4. Build milestone by milestone, depth-first, using the `depth-first` and
   `run-tests` skills. Never leave main broken between milestones.
5. Check the item off here, link the plan doc, note any gaps created.
