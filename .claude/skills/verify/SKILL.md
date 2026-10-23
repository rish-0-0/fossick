---
name: verify
description: Drive fossick end to end to prove a change actually works — build, serve, generate events, query the real API/UI. Use before committing any change with a runtime surface (ingest, storage, search, API, UI).
---

# verify — prove it works in the running app

Unit tests prove the function; this proves the product. Exercise the flow
your change affects through the real binary.

## 1. Build and start

```sh
# Only needed if you changed ui/: rebuild the bundle so build.rs embeds it
cd ui && npm run build && cd ..

cargo run -p fossick -- serve
```

Run `serve` in the background (it blocks). UI + API on
`http://127.0.0.1:8089`, TCP ingest on `5514` (defaults; a `--conf` file
can change them — see `fossick/src/conf.rs`).

## 2. Feed it data

```sh
cargo run -p fossick -- loggen --port 5514 --count 100000
```

For ingest/timestamp changes, don't rely only on loggen — also pipe a few
hand-crafted lines that hit your edge cases:

```sh
# bash
printf '2026-10-23T10:00:00Z svc=pay amount=42 needle_x\n' | nc 127.0.0.1 5514
```

```powershell
# PowerShell equivalent
$c = New-Object Net.Sockets.TcpClient('127.0.0.1', 5514)
$w = New-Object IO.StreamWriter($c.GetStream())
$w.WriteLine('2026-10-23T10:00:00Z svc=pay amount=42 needle_x'); $w.Flush(); $c.Close()
```

## 3. Query through the real path

```sh
# API — check fields/rows/matched/truncated look right
curl "http://127.0.0.1:8089/api/search?q=needle_x"
curl "http://127.0.0.1:8089/api/search?q=error%20%7C%20stats%20count%20by%20svc&earliest=-15m"

# CLI path too (separate code path from the HTTP head)
cargo run -p fossick -- search "needle_x | head 5"
```

Checks that matter per area:

- **Search/index changes**: run the same query in a way that forces a full
  scan (e.g. a NOT/OR shape the pushdown can't use) and compare `matched` —
  pushdown must never change results, only speed.
- **Bucket lifecycle changes**: ingest past rollover (loggen with enough
  count), kill the server mid-ingest (`kill -9` / Stop-Process -Force),
  restart, confirm recovery and that pre-crash events are searchable.
- **API changes**: curl the exact new/changed endpoint, including one
  malformed request — confirm a clean 4xx, not a hang or 500.
- **UI changes**: `cd ui && npm run dev` (Vite on :5173 proxying /api to
  :8089) and exercise the changed screen against real data; or use the
  Playwright browser tools against `http://127.0.0.1:8089/`.

## 4. Clean up and report

Stop the server. Delete any throwaway data dir you created. Report what
you drove, what you observed (paste the actual API response for the key
query), and anything that surprised you — surprises are findings, not
noise.
