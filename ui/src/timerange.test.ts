import { describe, expect, it } from 'vitest'
import { DEFAULT_RANGE, rangeByLabel, TIME_RANGES } from './timerange'

describe('time ranges', () => {
  it('labels are unique', () => {
    const labels = TIME_RANGES.map((r) => r.label)
    expect(new Set(labels).size).toBe(labels.length)
  })

  it('looks up by label and falls back to the default', () => {
    expect(rangeByLabel('Last 15 minutes').earliest).toBe('-15m')
    expect(rangeByLabel('nonsense')).toBe(DEFAULT_RANGE)
  })

  it('all-time sends no bounds so the search head decides', () => {
    const all = rangeByLabel('All time')
    expect(all.earliest).toBeUndefined()
    expect(all.latest).toBeUndefined()
  })
})
