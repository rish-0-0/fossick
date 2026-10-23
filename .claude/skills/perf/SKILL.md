---
name: perf
description: Measurement-first optimization discipline for fossick — how to benchmark ingest and search, what the standing performance invariants are, and what evidence a perf change must ship with. Use before AND after optimizing anything.
---

# perf — optimize with numbers or not at all

A perf change without a before/after measurement is a style change with
risk. Every optimization ships with numbers in the commit message.

## Standing invariants (don't regress these)

- **Hot paths are allocation-free**: ingest tokenize/append and the
  per-event search match loop must not allocate per event. (See the
  alloc-free case-insensitive match in `fossick/src/search.rs` — that's
  the standard.)
- **No fsync on the hot path**: journal syncs every `SYNC_EVERY_BLOCKS`
  blocks, manifest writes are off the per-event path. Any new fsync needs
  a durability argument.
- **Bounded reader memory**: one decompressed block (256 KiB) + O(results).
  tsidx postings decode lazily — never materialize full postings lists to
  intersect them.
- **Pushdown prunes aggressively but safely**: time range → bloom → tsidx.
  More pruning is the highest-leverage search optimization; it must come
  with a results-equivalence test (same `matched` with and without).

## How to measure

Always `--release`. Debug-build numbers are noise.

**Ingest throughput** — loggen end to end:

```sh
cargo build --release -p fossick
./target/release/fossick serve   # fresh data dir; background it
# time 1M events in; events/sec = 1e6 / seconds
time ./target/release/fossick loggen --port 5514 --count 1000000
```

**Search latency** — the API reports it (`tookMs`); the CLI path can be
timed directly. Use three query shapes, because they stress different
layers:

```sh
# 1. rare term (bloom + tsidx pruning dominates)
curl "http://127.0.0.1:8089/api/search?q=needle_3"
# 2. common term + stats (decode + executor dominates)
curl "http://127.0.0.1:8089/api/search?q=error%20%7C%20stats%20count%20by%20svc"
# 3. full scan shape (journal decode bandwidth dominates)
curl "http://127.0.0.1:8089/api/search?q=NOT%20zzz_absent"
```

Run each 5×, report median, on the same dataset (record the loggen count
and bucket count alongside the numbers).

**Micro-benchmarks** — for a single function (tokenizer, varint, bloom
hash), add a `#[bench]`-style criterion bench only if the end-to-end
numbers can't isolate it; prefer end-to-end. If you add criterion, it's a
dev-dependency of one crate, justified in the commit.

**Allocation checks** — on the paths claimed alloc-free, eyeball the code
for `Vec::new`/`to_vec`/`String` per event; when in doubt, a quick
counting-allocator test or heaptrack/dhat run settles it.

## The procedure

1. Pick the metric and record the baseline (median of 5) BEFORE touching
   code. Save the exact commands.
2. Profile or reason to a hypothesis — name the suspected cost.
3. Change ONE thing.
4. Re-measure with the same commands/dataset. No improvement ≥ noise?
   Revert, keep the finding.
5. Run the full `run-tests` ladder — fast-but-wrong is wrong. Index/planner
   changes additionally need the pushdown-equivalence check.
6. Commit message includes: metric, baseline, after, dataset size, and the
   one-line why.

## What NOT to do

- No speculative caching, no threading "for speed" without a measured
  single-thread bottleneck, no unsafe for perf without numbers proving the
  safe version is the bottleneck AND the invariant documented + tested.
- Don't optimize code that isn't on a measured hot path — readability
  loses only when numbers say it must.
