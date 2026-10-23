---
name: security-audit
description: Vulnerability and safety sweep for fossick — dependency advisories, unsafe/panic scan, and threat-surface checklists for ingest, HTTP, file formats, and app .zip handling. Run before adding dependencies, after touching ingest/HTTP/zip/parsing code, and periodically.
---

# security-audit

fossick parses untrusted bytes in three places — TCP ingest, the HTTP API,
and (Phase 3+) uploaded app .zips — and reads its own on-disk files, which
can be corrupt. Audit all of it.

## 1. Dependency advisories

```sh
# one-time installs
cargo install cargo-audit

cargo audit                      # RustSec advisories against Cargo.lock
cd ui && npm audit --omit=dev    # UI runtime deps (dev-tool noise excluded)
```

Pass = no advisories on code we ship. For each hit: upgrade if possible;
if not, document why it's unreachable in the commit message and file a
ROADMAP note. Also review `Cargo.lock` diff whenever deps change — a new
transitive tree must be explainable.

## 2. Code-level scan

```sh
# unsafe blocks — each needs a justification comment + invariant test
grep -rn "unsafe" fossick/src fossick-index/src fossick-spl/src

# panics reachable from untrusted input — unwrap/expect/panic/indexing
# on the serve + ingest + decode paths are bugs
grep -rnE "\.unwrap\(\)|\.expect\(|panic!" fossick/src/http.rs fossick/src/serve.rs fossick-index/src
```

For every hit on a path that touches network bytes or on-disk bytes:
either prove the invariant (comment + test) or convert to an error.

## 3. Threat-surface checklists

**TCP ingest (`fossick/src/serve.rs`)**
- [ ] Max line/event length enforced (no unbounded buffer growth)
- [ ] Invalid UTF-8 handled per policy, never panics
- [ ] Slow/stalled clients can't wedge ingest (read timeouts/back-pressure)

**HTTP API (`fossick/src/http.rs` — hand-rolled, so we own every bug)**
- [ ] Binds 127.0.0.1 only until the auth phase lands — verify, don't assume
- [ ] Request size + header count limits; malformed requests → 4xx, no panic
- [ ] No path traversal in any file-serving route (`..`, encoded `%2e%2e`,
      backslashes on Windows)
- [ ] Query params fully percent-decoded exactly once
- [ ] SPL from the query string can't read arbitrary files or run commands
      (today SPL has no such surface — keep it that way; `lookup`/app
      features must resolve names against an allowlisted directory, never
      accept paths)

**On-disk formats (`fossick-index/`)**
- [ ] Every length/offset read from a file is bounds-checked before use
      (a corrupt `comp_len`/`postings_off` must not cause OOM or OOB mmap
      reads)
- [ ] Decompression bounded: zstd output capped at the known
      `uncomp_len` ≤ 256 KiB block bound — no decompression bombs
- [ ] Fuzz-ish tests exist: bit-flipped / truncated journal, tsidx, bloom,
      events.tbl all return errors, never panic or hang

**App .zip install (Phase 3 — run this skill as part of that work)**
- [ ] Zip-slip: reject entries with absolute paths, `..`, drive letters,
      backslash tricks; canonicalize and verify containment under `apps/`
- [ ] Caps: max zip size, max entry count, max per-entry and total
      uncompressed size (zip bombs)
- [ ] Manifest schema strictly validated; unknown executable content
      rejected — v1 apps are static assets + conf + saved searches ONLY
- [ ] Symlinks in archives rejected
- [ ] Install is atomic: extract to temp dir, validate fully, then rename

## 4. Report

List: advisories found/resolved, unsafe/panic hits triaged (with verdicts),
checklist items verified vs. not applicable vs. FAILED. Every FAILED item
gets fixed now or lands in ROADMAP/Known Gaps explicitly — a silent skip is
a failed audit. Add any new recurring check you invented to THIS skill.
