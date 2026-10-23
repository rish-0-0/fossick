// Client for the fossick search head. All searches go through the search
// head (/api/search) — never the indexer directly; the head may rewrite the
// request (time defaults, limits) before touching the index.

export type Cell = string | number | boolean | null

export interface SearchResult {
  fields: string[]
  rows: Cell[][]
  isAggregated: boolean
  truncated: boolean
  matched: number
  tookMs: number
}

export function buildSearchUrl(spl: string, earliest?: string, latest?: string): string {
  const p = new URLSearchParams()
  p.set('q', spl)
  if (earliest) p.set('earliest', earliest)
  if (latest) p.set('latest', latest)
  return `/api/search?${p.toString()}`
}

/** Validate the search-head payload; throws on shape mismatches so bugs
 * surface as errors instead of blank tables. */
export function parseSearchResult(json: unknown): SearchResult {
  if (typeof json !== 'object' || json === null) throw new Error('response is not an object')
  const o = json as Record<string, unknown>
  if (typeof o.error === 'string') throw new Error(o.error)
  const { fields, rows } = o
  if (!Array.isArray(fields) || !fields.every((f) => typeof f === 'string'))
    throw new Error('bad fields in response')
  if (!Array.isArray(rows)) throw new Error('bad rows in response')
  for (const row of rows) {
    if (!Array.isArray(row) || row.length !== fields.length)
      throw new Error('row/field length mismatch')
    for (const c of row) {
      const t = typeof c
      if (c !== null && t !== 'string' && t !== 'number' && t !== 'boolean')
        throw new Error(`bad cell type: ${t}`)
    }
  }
  return {
    fields: fields as string[],
    rows: rows as Cell[][],
    isAggregated: o.isAggregated === true,
    truncated: o.truncated === true,
    matched: typeof o.matched === 'number' ? o.matched : 0,
    tookMs: typeof o.tookMs === 'number' ? o.tookMs : 0,
  }
}

export async function runSearch(spl: string, earliest?: string, latest?: string): Promise<SearchResult> {
  const resp = await fetch(buildSearchUrl(spl, earliest, latest))
  const text = await resp.text()
  let json: unknown
  try {
    json = JSON.parse(text)
  } catch {
    throw new Error(resp.ok ? 'malformed response from search head' : text.trim() || `HTTP ${resp.status}`)
  }
  return parseSearchResult(json)
}
