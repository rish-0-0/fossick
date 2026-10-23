import { describe, expect, it } from 'vitest'
import { buildSearchUrl, parseSearchResult } from './api'

describe('buildSearchUrl', () => {
  it('encodes the SPL and time params', () => {
    const url = buildSearchUrl('error host=web* | stats count by host', '-15m', 'now')
    expect(url.startsWith('/api/search?')).toBe(true)
    const p = new URLSearchParams(url.split('?')[1])
    expect(p.get('q')).toBe('error host=web* | stats count by host')
    expect(p.get('earliest')).toBe('-15m')
    expect(p.get('latest')).toBe('now')
  })

  it('omits absent time bounds', () => {
    const p = new URLSearchParams(buildSearchUrl('*').split('?')[1])
    expect(p.get('earliest')).toBeNull()
    expect(p.get('latest')).toBeNull()
  })

  it('survives quotes, pipes and unicode in the query', () => {
    const spl = '"a b" | where x="ü&=?"'
    const p = new URLSearchParams(buildSearchUrl(spl).split('?')[1])
    expect(p.get('q')).toBe(spl)
  })
})

describe('parseSearchResult', () => {
  const ok = {
    fields: ['host', 'count'],
    rows: [
      ['web01', 5],
      ['web02', null],
    ],
    isAggregated: true,
    truncated: false,
    matched: 6,
    tookMs: 12,
  }

  it('accepts a well-formed payload with typed cells', () => {
    const r = parseSearchResult(ok)
    expect(r.fields).toEqual(['host', 'count'])
    expect(r.rows[0][1]).toBe(5) // numbers stay numbers
    expect(r.rows[1][1]).toBeNull()
    expect(r.isAggregated).toBe(true)
    expect(r.matched).toBe(6)
  })

  it('surfaces search-head errors', () => {
    expect(() => parseSearchResult({ error: 'bad query: unknown function' })).toThrow(/unknown function/)
  })

  it('rejects malformed payloads', () => {
    expect(() => parseSearchResult(null)).toThrow()
    expect(() => parseSearchResult({ fields: 'nope', rows: [] })).toThrow(/fields/)
    expect(() => parseSearchResult({ fields: ['a'], rows: [['x', 'extra']] })).toThrow(/mismatch/)
    expect(() => parseSearchResult({ fields: ['a'], rows: [[{ nested: true }]] })).toThrow(/cell/)
  })

  it('defaults optional metadata safely', () => {
    const r = parseSearchResult({ fields: [], rows: [] })
    expect(r.matched).toBe(0)
    expect(r.truncated).toBe(false)
  })
})
