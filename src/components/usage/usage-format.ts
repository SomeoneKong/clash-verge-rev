import parseTraffic from '@/utils/parse-traffic'

export type UsageMetric = 'total' | 'upload' | 'download'

/** Colors of the largest items, in rank order; everything else is grey. */
export const USAGE_COLORS = [
  '#4e79a7',
  '#f28e2b',
  '#e15759',
  '#76b7b2',
  '#59a14f',
  '#edc948',
  '#b07aa1',
  '#ff9da7',
  '#9c755f',
  '#17becf',
]
export const OTHER_COLOR = '#9e9e9e'

export const metricValue = (
  [upload, download]: [number, number],
  metric: UsageMetric,
) =>
  metric === 'upload'
    ? upload
    : metric === 'download'
      ? download
      : upload + download

export const formatBytes = (bytes: number) => {
  const [value, unit] = parseTraffic(bytes)
  return `${value} ${unit}`
}
