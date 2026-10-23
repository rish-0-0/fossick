import { FormEvent, KeyboardEvent, useState } from 'react'
import { runSearch, SearchResult } from '../api'
import { DEFAULT_RANGE, rangeByLabel, TIME_RANGES } from '../timerange'
import ResultsTable from './ResultsTable'

const PLACEHOLDER = 'error host=web* | stats count by host   —   Enter to search, Shift+Enter for a new line'

export default function SearchPage() {
  const [spl, setSpl] = useState('')
  const [rangeLabel, setRangeLabel] = useState(DEFAULT_RANGE.label)
  const [result, setResult] = useState<SearchResult | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)

  async function search() {
    if (loading) return
    setLoading(true)
    setError(null)
    try {
      const range = rangeByLabel(rangeLabel)
      setResult(await runSearch(spl, range.earliest, range.latest))
    } catch (e) {
      setResult(null)
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setLoading(false)
    }
  }

  function onSubmit(e: FormEvent) {
    e.preventDefault()
    void search()
  }

  function onKeyDown(e: KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault()
      void search()
    }
  }

  return (
    <div className="search-page">
      <form className="search-bar" onSubmit={onSubmit}>
        <textarea
          className="spl-input"
          value={spl}
          placeholder={PLACEHOLDER}
          onChange={(e) => setSpl(e.target.value)}
          onKeyDown={onKeyDown}
          rows={2}
          spellCheck={false}
        />
        <div className="search-controls">
          <select value={rangeLabel} onChange={(e) => setRangeLabel(e.target.value)}>
            {TIME_RANGES.map((r) => (
              <option key={r.label}>{r.label}</option>
            ))}
          </select>
          <button type="submit" disabled={loading}>
            {loading ? 'Searching…' : 'Search'}
          </button>
        </div>
      </form>

      {error && <div className="error-box">{error}</div>}

      {result && (
        <>
          <div className="status-line">
            {result.rows.length.toLocaleString()} {result.isAggregated ? 'result rows' : 'events'}
            {' · '}
            {result.matched.toLocaleString()} events matched
            {' · '}
            {result.tookMs} ms
            {result.truncated && <span className="badge-warn">truncated by limits</span>}
          </div>
          {result.rows.length > 0 ? (
            <ResultsTable fields={result.fields} rows={result.rows} />
          ) : (
            <div className="empty-box">No results.</div>
          )}
        </>
      )}
      {!result && !error && <div className="empty-box">Run a search to see results.</div>}
    </div>
  )
}
