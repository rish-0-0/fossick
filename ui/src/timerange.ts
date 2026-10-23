// Time-range presets for the search bar. `earliest`/`latest` are fossick
// timespecs (-15m, now, ISO-8601, epoch) forwarded to the search head, which
// applies them only when the SPL itself has no earliest=/latest=.

export interface TimeRange {
  label: string
  earliest?: string
  latest?: string
}

export const TIME_RANGES: TimeRange[] = [
  { label: 'Last 15 minutes', earliest: '-15m' },
  { label: 'Last hour', earliest: '-1h' },
  { label: 'Last 4 hours', earliest: '-4h' },
  { label: 'Last 24 hours', earliest: '-24h' },
  { label: 'Last 7 days', earliest: '-7d' },
  { label: 'Last 30 days', earliest: '-30d' },
  { label: 'All time' },
]

export const DEFAULT_RANGE = TIME_RANGES[6] // All time

export function rangeByLabel(label: string): TimeRange {
  return TIME_RANGES.find((r) => r.label === label) ?? DEFAULT_RANGE
}
