import { Box, Typography, useTheme } from '@mui/material'
import { useSize } from 'ahooks'
import { useMemo, useRef, useState } from 'react'

import type { UsageGap, UsagePoint } from '@/services/cmds'

import { type UsageMetric, formatBytes, metricValue } from './usage-format'

const HEIGHT = 280
const PADDING = { left: 72, right: 12, top: 12, bottom: 28 }
const MAX_AXIS_LABELS = 8

interface Props {
  points: UsagePoint[]
  /** Hour after the last point. */
  endHour: number
  /** Series identities and names, in the order of each point's values. */
  keys: string[]
  series: string[]
  colors: string[]
  metric: UsageMetric
  gaps: UsageGap[]
  axisLabel: (point: UsagePoint) => string
  tooltipLabel: (point: UsagePoint) => string
}

/** Rounds up to 1, 2, 2.5 or 5 times a power of ten of the 1024-based unit. */
const niceMax = (value: number) => {
  if (value <= 0) return 1
  const unit = Math.pow(1024, Math.floor(Math.log2(value) / 10))
  const step = value / unit / 4
  const power = Math.pow(10, Math.floor(Math.log10(step)))
  const nice = [1, 2, 2.5, 5, 10].find((m) => m * power >= step) ?? 10
  return nice * power * 4 * unit
}

export const UsageChart = ({
  points,
  endHour,
  keys,
  series,
  colors,
  metric,
  gaps,
  axisLabel,
  tooltipLabel,
}: Props) => {
  const theme = useTheme()
  const containerRef = useRef<HTMLDivElement>(null)
  const width = useSize(containerRef)?.width ?? 640
  const [hovered, setHovered] = useState<number | null>(null)

  const plotWidth = Math.max(width - PADDING.left - PADDING.right, 1)
  const plotHeight = HEIGHT - PADDING.top - PADDING.bottom
  const band = plotWidth / Math.max(points.length, 1)
  const barWidth = Math.max(band * 0.72, 1)

  const totals = useMemo(
    () =>
      points.map((point) =>
        point.values.reduce(
          (sum, value) => sum + metricValue(value, metric),
          0,
        ),
      ),
    [points, metric],
  )
  const top = niceMax(Math.max(...totals, 0))
  const scale = (value: number) => (value / top) * plotHeight

  const gapColumns = useMemo(
    () =>
      points.map((point, index) => {
        const end = points[index + 1]?.startHour ?? endHour
        return gaps.some((gap) => gap.from < end && gap.to >= point.startHour)
      }),
    [points, gaps, endHour],
  )

  const labelEvery = Math.max(Math.ceil(points.length / MAX_AXIS_LABELS), 1)
  const textColor = theme.palette.text.secondary
  const gridColor = theme.palette.divider

  const hoveredPoint = hovered === null ? null : points[hovered]

  return (
    <Box ref={containerRef} sx={{ position: 'relative', width: '100%' }}>
      <svg
        width={width}
        height={HEIGHT}
        onMouseLeave={() => setHovered(null)}
        onMouseMove={(event) => {
          const box = event.currentTarget.getBoundingClientRect()
          const index = Math.floor(
            (event.clientX - box.left - PADDING.left) / band,
          )
          setHovered(index >= 0 && index < points.length ? index : null)
        }}
      >
        <defs>
          <pattern
            id="usage-gap-hatch"
            width="6"
            height="6"
            patternUnits="userSpaceOnUse"
            patternTransform="rotate(45)"
          >
            <line
              x1="0"
              y1="0"
              x2="0"
              y2="6"
              stroke={theme.palette.warning.main}
              strokeWidth="2"
              strokeOpacity="0.35"
            />
          </pattern>
        </defs>
        {[0, 1, 2, 3, 4].map((tick) => {
          const y = PADDING.top + plotHeight - (plotHeight * tick) / 4
          return (
            <g key={tick}>
              <line
                x1={PADDING.left}
                x2={PADDING.left + plotWidth}
                y1={y}
                y2={y}
                stroke={gridColor}
                strokeDasharray={tick === 0 ? undefined : '3 3'}
              />
              <text
                x={PADDING.left - 6}
                y={y + 4}
                textAnchor="end"
                fontSize="11"
                fill={textColor}
              >
                {formatBytes((top * tick) / 4)}
              </text>
            </g>
          )
        })}
        {points.map((point, index) => {
          const x = PADDING.left + index * band
          let y = PADDING.top + plotHeight
          return (
            <g key={point.startHour}>
              {gapColumns[index] && (
                <rect
                  x={x}
                  y={PADDING.top}
                  width={band}
                  height={plotHeight}
                  fill="url(#usage-gap-hatch)"
                />
              )}
              {hovered === index && (
                <rect
                  x={x}
                  y={PADDING.top}
                  width={band}
                  height={plotHeight}
                  fill={theme.palette.action.hover}
                />
              )}
              {point.values.map((value, seriesIndex) => {
                const height = scale(metricValue(value, metric))
                if (height <= 0) return null
                y -= height
                return (
                  <rect
                    key={keys[seriesIndex]}
                    x={x + (band - barWidth) / 2}
                    y={y}
                    width={barWidth}
                    height={height}
                    fill={colors[seriesIndex]}
                  />
                )
              })}
              {index % labelEvery === 0 && (
                <text
                  x={x + band / 2}
                  y={HEIGHT - 8}
                  textAnchor="middle"
                  fontSize="11"
                  fill={textColor}
                >
                  {axisLabel(point)}
                </text>
              )}
            </g>
          )
        })}
      </svg>
      {hoveredPoint && hovered !== null && (
        <Box
          sx={{
            position: 'absolute',
            top: PADDING.top,
            left:
              PADDING.left + hovered * band > width / 2
                ? undefined
                : PADDING.left + (hovered + 1) * band + 8,
            right:
              PADDING.left + hovered * band > width / 2
                ? width - (PADDING.left + hovered * band) + 8
                : undefined,
            pointerEvents: 'none',
            bgcolor: 'background.paper',
            border: 1,
            borderColor: 'divider',
            borderRadius: 1,
            boxShadow: 2,
            px: 1.5,
            py: 1,
            minWidth: 180,
            zIndex: 1,
          }}
        >
          <Typography
            variant="caption"
            component="div"
            sx={{ fontWeight: 600 }}
          >
            {tooltipLabel(hoveredPoint)}
          </Typography>
          {hoveredPoint.values
            .map((value, seriesIndex) => ({
              seriesIndex,
              amount: metricValue(value, metric),
            }))
            .filter(({ amount }) => amount > 0)
            .reverse()
            .map(({ seriesIndex, amount }) => (
              <Box
                key={keys[seriesIndex]}
                sx={{ display: 'flex', alignItems: 'center', gap: 1 }}
              >
                <Box
                  sx={{
                    width: 8,
                    height: 8,
                    borderRadius: '2px',
                    bgcolor: colors[seriesIndex],
                    flexShrink: 0,
                  }}
                />
                <Typography
                  variant="caption"
                  sx={{ flex: 1, whiteSpace: 'nowrap' }}
                >
                  {series[seriesIndex]}
                </Typography>
                <Typography variant="caption">{formatBytes(amount)}</Typography>
              </Box>
            ))}
          <Typography
            variant="caption"
            component="div"
            sx={{ mt: 0.5, textAlign: 'right' }}
          >
            {formatBytes(totals[hovered])}
          </Typography>
        </Box>
      )}
    </Box>
  )
}
