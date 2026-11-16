import {
  DeleteSweepRounded,
  ExpandLessRounded,
  ExpandMoreRounded,
} from '@mui/icons-material'
import {
  Alert,
  Box,
  FormControlLabel,
  IconButton,
  MenuItem,
  Switch,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableRow,
  Tooltip,
  Typography,
} from '@mui/material'
import { ask } from '@tauri-apps/plugin-dialog'
import { useLockFn } from 'ahooks'
import dayjs from 'dayjs'
import { Fragment, useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { mutate as swrMutate } from 'swr'

import { BaseSearchBox, BaseStyledSelect } from '@/components/base'
import { formatBytes } from '@/components/usage/usage-format'
import type { ClosedFeed } from '@/hooks/use-connection-data'
import {
  type ConnCoverage,
  type ConnDimension,
  type ConnGroup,
  type ConnReport,
  type ConnStatsStatus,
  clearConnectionStats,
  getConnectionStatsStatus,
  queryConnectionStats,
  setConnectionStats,
} from '@/services/cmds'
import { mutate } from '@/services/mutate'
import { useQuery } from '@/services/query-client'

type AggregateRange = 'active' | 'm5' | 'm15' | 'm30' | 'today' | 'h24' | 'd7'

const RANGES: AggregateRange[] = [
  'active',
  'm5',
  'm15',
  'm30',
  'today',
  'h24',
  'd7',
]
const RECENT_MINUTES: Partial<Record<AggregateRange, number>> = {
  m5: 5,
  m15: 15,
  m30: 30,
}
const DIMENSIONS: ConnDimension[] = [
  'target',
  'process',
  'chain',
  'exit',
  'rule',
  'source',
  'inbound',
  'network',
]
const COVERAGE_KEYS = {
  unknown: 'connStats.coverage.unknown',
  not_recorded: 'connStats.coverage.not_recorded',
  gap: 'connStats.coverage.gap',
  pending: 'connStats.coverage.pending',
  not_counted: 'connStats.coverage.not_counted',
  partly_counted: 'connStats.coverage.partly_counted',
} as const
type IncompleteCoverage = keyof typeof COVERAGE_KEYS
const HOURLY_REFRESH = 60_000
const MAX_LISTED = 50

interface AggregateRow {
  value: string
  conns: number
  up: number
  down: number
  // speeds come from the active connections only
  upSpeed: number
  downSpeed: number
  items: IConnectionsItem[]
}

const isHourly = (range: AggregateRange) =>
  range === 'today' || range === 'h24' || range === 'd7'

/** The value of a connection by a dimension, as the core groups the connection stats. */
const valueOf = (item: IConnectionsItem, dimension: ConnDimension) => {
  const { metadata } = item
  switch (dimension) {
    case 'target':
      return metadata.host || metadata.destinationIP || ''
    case 'process':
      return metadata.processPath || metadata.process || ''
    case 'chain':
      return [...item.chains].reverse().join(' → ')
    case 'exit':
      return item.chains[0] ?? ''
    case 'rule':
      return item.rulePayload ? `${item.rule},${item.rulePayload}` : item.rule
    case 'source':
      return metadata.sourceIP
    case 'inbound':
      return metadata.type
    case 'network':
      return metadata.network
  }
}

const groupConnections = (
  items: IConnectionsItem[],
  dimension: ConnDimension,
  closed: (item: IConnectionsItem) => boolean,
) => {
  const rows = new Map<string, AggregateRow>()
  for (const item of items) {
    const value = valueOf(item, dimension)
    let row = rows.get(value)
    if (!row) {
      row = {
        value,
        conns: 0,
        up: 0,
        down: 0,
        upSpeed: 0,
        downSpeed: 0,
        items: [],
      }
      rows.set(value, row)
    }
    row.conns += 1
    row.up += item.upload
    row.down += item.download
    if (!closed(item)) {
      row.upSpeed += item.curUpload ?? 0
      row.downSpeed += item.curDownload ?? 0
    }
    row.items.push(item)
  }
  return [...rows.values()].sort(
    (a, b) => b.up + b.down - (a.up + a.down) || b.conns - a.conns,
  )
}

const hourRange = (range: AggregateRange): [number, number] => {
  const now = Math.floor(Date.now() / 3_600_000)
  if (range === 'today') {
    return [Math.floor(dayjs().startOf('day').valueOf() / 3_600_000), now]
  }
  return [now - (range === 'd7' ? 7 * 24 - 1 : 23), now]
}

const groupRow = (group: ConnGroup): AggregateRow => ({
  value: group.values[0] ?? '',
  conns: group.conns,
  up: group.up,
  down: group.down,
  upSpeed: 0,
  downSpeed: 0,
  items: [],
})

interface Props {
  active: IConnectionsItem[]
  /** Every closed connection kept, newest first. */
  closed: IConnectionsItem[]
  feed: ClosedFeed
  /** When the connections were read (ms). */
  polledAt: number
}

export const ConnectionAggregate = ({
  active,
  closed,
  feed,
  polledAt,
}: Props) => {
  const { t } = useTranslation()
  const [range, setRange] = useState<AggregateRange>('active')
  const [dimension, setDimension] = useState<ConnDimension>('target')
  const [drill, setDrill] = useState<ConnDimension | ''>('')
  const [match, setMatch] = useState<(input: string) => boolean>(
    () => () => true,
  )
  const [expanded, setExpanded] = useState<string | null>(null)
  const hourly = isHourly(range)

  const { data: status, refetch: refetchStatus } = useQuery<ConnStatsStatus>({
    queryKey: ['getConnectionStatsStatus'],
    queryFn: () => getConnectionStatsStatus(),
    refetchInterval: 10_000,
  })
  const hours = hourly ? hourRange(range) : null
  const { data: report, error } = useQuery<ConnReport>({
    queryKey: ['queryConnectionStats', range, dimension, hours?.[1]],
    queryFn: () =>
      queryConnectionStats({
        fromHour: hours![0],
        toHour: hours![1],
        groupBy: [dimension],
      }),
    enabled: hourly && !!status?.hasHistory,
    refetchInterval: HOURLY_REFRESH,
  })

  const minutes = RECENT_MINUTES[range]
  const windowStart = minutes ? polledAt - minutes * 60_000 : polledAt
  const closedInWindow = useMemo(
    () =>
      minutes
        ? closed.filter(
            (item) => item.closedAt && Date.parse(item.closedAt) >= windowStart,
          )
        : [],
    [minutes, closed, windowStart],
  )

  const recentRows = useMemo(() => {
    if (hourly) return []
    const closedIds = new Set(closedInWindow.map((item) => item.id))
    return groupConnections(
      [...active, ...closedInWindow],
      dimension,
      (item) => closedIds.has(item.id) && !!item.closedAt,
    )
  }, [hourly, active, closedInWindow, dimension])

  const rows = (
    hourly ? (report?.groups.map(groupRow) ?? []) : recentRows
  ).filter((row) => match(row.value))

  // a core not answered yet may keep closed connections it was not read for
  const recentIncomplete =
    !!RECENT_MINUTES[range] &&
    feed.supported !== false &&
    (!feed.caughtUp ||
      windowStart <= feed.missingUntil ||
      windowStart < feed.startedAt)

  const coverageCounts = useMemo(() => {
    const counts = new Map<IncompleteCoverage, number>()
    for (const hour of report?.hours ?? []) {
      const coverage: ConnCoverage = hour.coverage
      if (coverage === 'complete') continue
      counts.set(coverage, (counts.get(coverage) ?? 0) + 1)
    }
    return counts
  }, [report])
  const partlyRecorded = (report?.hours ?? []).filter(
    (hour) => hour.recorded === 'part',
  ).length

  const toggleRecording = useLockFn(async (enabled: boolean) => {
    await mutate(() => setConnectionStats(enabled), {
      id: 'set-connection-stats',
    })
    await refetchStatus()
  })
  const onClear = useLockFn(async () => {
    const confirmed = await ask(t('connStats.clear.confirm'), {
      kind: 'warning',
    })
    if (!confirmed) return
    await mutate(() => clearConnectionStats(), { id: 'clear-connection-stats' })
    setExpanded(null)
    await refetchStatus()
    // every range and drill-down read before, not only those shown
    await swrMutate(
      (key) => Array.isArray(key) && key[0] === 'queryConnectionStats',
      undefined,
      { revalidate: true },
    )
  })

  const dimensionLabel = (value: ConnDimension) =>
    t(`connStats.dimensions.${value}`)

  return (
    <Box
      sx={{
        flex: 1,
        minHeight: 0,
        display: 'flex',
        flexDirection: 'column',
        mx: '10px',
        gap: 1,
      }}
    >
      <Box
        sx={{ display: 'flex', alignItems: 'center', gap: 1, flexWrap: 'wrap' }}
      >
        <BaseStyledSelect
          value={range}
          onChange={(e) => {
            setRange(e.target.value as AggregateRange)
            setExpanded(null)
          }}
        >
          {RANGES.map((value) => (
            <MenuItem
              key={value}
              value={value}
              disabled={isHourly(value) && !status?.hasHistory}
            >
              <span style={{ fontSize: 14 }}>
                {t(`connStats.ranges.${value}`)}
              </span>
            </MenuItem>
          ))}
        </BaseStyledSelect>
        <BaseStyledSelect
          value={dimension}
          onChange={(e) => {
            setDimension(e.target.value as ConnDimension)
            setExpanded(null)
          }}
        >
          {DIMENSIONS.map((value) => (
            <MenuItem key={value} value={value}>
              <span style={{ fontSize: 14 }}>{dimensionLabel(value)}</span>
            </MenuItem>
          ))}
        </BaseStyledSelect>
        <BaseStyledSelect
          value={drill}
          displayEmpty
          onChange={(e) => {
            setDrill(e.target.value as ConnDimension | '')
            setExpanded(null)
          }}
        >
          <MenuItem value="">
            <span style={{ fontSize: 14 }}>{t('connStats.drill.none')}</span>
          </MenuItem>
          {DIMENSIONS.filter((value) => value !== dimension).map((value) => (
            <MenuItem key={value} value={value}>
              <span style={{ fontSize: 14 }}>
                {t('connStats.drill.by', { dimension: dimensionLabel(value) })}
              </span>
            </MenuItem>
          ))}
        </BaseStyledSelect>
        <Box sx={{ flex: 1, minWidth: 160 }}>
          <BaseSearchBox onSearch={(next) => setMatch(() => next)} />
        </Box>
        <FormControlLabel
          sx={{ mr: 0 }}
          control={
            <Switch
              size="small"
              checked={!!status?.recording}
              disabled={!status}
              onChange={(e) => toggleRecording(e.target.checked)}
            />
          }
          label={
            <Typography variant="body2">{t('connStats.recording')}</Typography>
          }
        />
        <Tooltip title={t('connStats.clear.title')}>
          <span>
            <IconButton
              size="small"
              disabled={!status?.hasHistory}
              onClick={onClear}
            >
              <DeleteSweepRounded fontSize="small" />
            </IconButton>
          </span>
        </Tooltip>
      </Box>

      {status?.settingLost && (
        <Alert severity="warning">{t('connStats.status.settingLost')}</Alert>
      )}
      {status?.recording && status.coreCounting === false && (
        <Alert severity="info">{t('connStats.status.applying')}</Alert>
      )}
      {RECENT_MINUTES[range] && (
        <Alert severity="info">{t('connStats.notes.recentTotals')}</Alert>
      )}
      {RECENT_MINUTES[range] && feed.supported === false && (
        <Alert severity="warning">{t('connStats.notes.oldCore')}</Alert>
      )}
      {recentIncomplete && (
        <Alert severity="warning">
          {feed.caughtUp
            ? t('connStats.notes.recentMissing')
            : t('connStats.notes.recentReading')}
        </Alert>
      )}
      {hourly && !status?.hasHistory && (
        <Alert severity="info">{t('connStats.notes.enableToRecord')}</Alert>
      )}
      {hourly && error && <Alert severity="error">{String(error)}</Alert>}
      {hourly && (coverageCounts.size > 0 || partlyRecorded > 0) && (
        <Alert severity="warning">
          {[...coverageCounts.entries()]
            .map(([coverage, count]) => t(COVERAGE_KEYS[coverage], { count }))
            .concat(
              partlyRecorded > 0
                ? [
                    t('connStats.coverage.partlyRecorded', {
                      count: partlyRecorded,
                    }),
                  ]
                : [],
            )
            .join(t('connStats.coverage.separator'))}
        </Alert>
      )}

      <Box sx={{ flex: 1, minHeight: 0, overflow: 'auto' }}>
        <Table size="small" stickyHeader>
          <TableHead>
            <TableRow>
              <TableCell>{dimensionLabel(dimension)}</TableCell>
              <TableCell align="right">
                {t('connStats.columns.conns')}
              </TableCell>
              <TableCell align="right">
                {t('connStats.columns.download')}
              </TableCell>
              <TableCell align="right">
                {t('connStats.columns.upload')}
              </TableCell>
              {!hourly && (
                <>
                  <TableCell align="right">
                    {t('connStats.columns.downloadSpeed')}
                  </TableCell>
                  <TableCell align="right">
                    {t('connStats.columns.uploadSpeed')}
                  </TableCell>
                </>
              )}
            </TableRow>
          </TableHead>
          <TableBody>
            {rows.map((row) => {
              const expandable = drill !== '' || !hourly
              const open = expanded === row.value
              return (
                <Fragment key={row.value}>
                  <AggregateTableRow
                    row={row}
                    hourly={hourly}
                    expandable={expandable}
                    open={open}
                    onToggle={() => setExpanded(open ? null : row.value)}
                  />
                  {open && (
                    <TableRow>
                      <TableCell
                        colSpan={hourly ? 4 : 6}
                        sx={{ py: 0.5, pl: 4 }}
                      >
                        {drill !== '' ? (
                          hourly ? (
                            <HourlyDrill
                              range={range}
                              dimension={dimension}
                              value={row.value}
                              drill={drill}
                            />
                          ) : (
                            <DrillTable
                              rows={groupConnections(
                                row.items,
                                drill,
                                (item) => !!item.closedAt,
                              )}
                              hourly={false}
                            />
                          )
                        ) : (
                          <ConnectionList items={row.items} />
                        )}
                      </TableCell>
                    </TableRow>
                  )}
                </Fragment>
              )
            })}
            {hourly && report?.other && (
              <AggregateTableRow
                row={{ ...groupRow(report.other), value: t('connStats.other') }}
                hourly
                expandable={false}
                open={false}
                onToggle={() => {}}
              />
            )}
          </TableBody>
        </Table>
      </Box>
    </Box>
  )
}

const label = (value: string) => value || '—'

const AggregateTableRow = ({
  row,
  hourly,
  expandable,
  open,
  onToggle,
}: {
  row: AggregateRow
  hourly: boolean
  expandable: boolean
  open: boolean
  onToggle: () => void
}) => (
  <TableRow
    hover
    sx={{ cursor: expandable ? 'pointer' : 'default' }}
    onClick={expandable ? onToggle : undefined}
  >
    <TableCell sx={{ wordBreak: 'break-all' }}>
      <Box sx={{ display: 'flex', alignItems: 'center', gap: 0.5 }}>
        {expandable &&
          (open ? (
            <ExpandLessRounded fontSize="small" />
          ) : (
            <ExpandMoreRounded fontSize="small" />
          ))}
        <Typography variant="body2">{label(row.value)}</Typography>
      </Box>
    </TableCell>
    <TableCell align="right">{row.conns}</TableCell>
    <TableCell align="right">{formatBytes(row.down)}</TableCell>
    <TableCell align="right">{formatBytes(row.up)}</TableCell>
    {!hourly && (
      <>
        <TableCell align="right">{formatBytes(row.downSpeed)}/s</TableCell>
        <TableCell align="right">{formatBytes(row.upSpeed)}/s</TableCell>
      </>
    )}
  </TableRow>
)

const DrillTable = ({
  rows,
  hourly,
}: {
  rows: AggregateRow[]
  hourly: boolean
}) => (
  <Table size="small">
    <TableBody>
      {rows.map((row) => (
        <TableRow key={row.value}>
          <TableCell sx={{ wordBreak: 'break-all', border: 0 }}>
            <Typography variant="body2">{label(row.value)}</Typography>
          </TableCell>
          <TableCell align="right" sx={{ border: 0 }}>
            {row.conns}
          </TableCell>
          <TableCell align="right" sx={{ border: 0 }}>
            {formatBytes(row.down)}
          </TableCell>
          <TableCell align="right" sx={{ border: 0 }}>
            {formatBytes(row.up)}
          </TableCell>
          {!hourly && (
            <TableCell align="right" sx={{ border: 0 }}>
              {formatBytes(row.downSpeed)}/s
            </TableCell>
          )}
        </TableRow>
      ))}
    </TableBody>
  </Table>
)

const HourlyDrill = ({
  range,
  dimension,
  value,
  drill,
}: {
  range: AggregateRange
  dimension: ConnDimension
  value: string
  drill: ConnDimension
}) => {
  const { t } = useTranslation()
  const hours = hourRange(range)
  const { data } = useQuery<ConnReport>({
    queryKey: [
      'queryConnectionStats',
      range,
      dimension,
      value,
      drill,
      hours[1],
    ],
    queryFn: () =>
      queryConnectionStats({
        fromHour: hours[0],
        toHour: hours[1],
        groupBy: [drill],
        filters: [{ dimension, value }],
      }),
    refetchInterval: HOURLY_REFRESH,
  })
  const rows = (data?.groups ?? []).map(groupRow)
  if (data?.other) {
    rows.push({ ...groupRow(data.other), value: t('connStats.other') })
  }
  return <DrillTable rows={rows} hourly />
}

const ConnectionList = ({ items }: { items: IConnectionsItem[] }) => {
  const { t } = useTranslation()
  const listed = [...items]
    .sort((a, b) => b.upload + b.download - (a.upload + a.download))
    .slice(0, MAX_LISTED)
  return (
    <Table size="small">
      <TableBody>
        {listed.map((item) => (
          <TableRow key={item.id}>
            <TableCell sx={{ wordBreak: 'break-all', border: 0 }}>
              <Typography variant="body2">
                {item.metadata.host || item.metadata.destinationIP}:
                {item.metadata.destinationPort}
              </Typography>
              <Typography variant="caption" color="text.secondary">
                {item.metadata.process} ·{' '}
                {[...item.chains].reverse().join(' → ')}
              </Typography>
            </TableCell>
            <TableCell align="right" sx={{ border: 0 }}>
              {formatBytes(item.download)} / {formatBytes(item.upload)}
            </TableCell>
            <TableCell align="right" sx={{ border: 0, whiteSpace: 'nowrap' }}>
              {item.closedAt
                ? t('connStats.closedAt', {
                    time: dayjs(item.closedAt).format('HH:mm:ss'),
                  })
                : t('connStats.active')}
            </TableCell>
          </TableRow>
        ))}
      </TableBody>
    </Table>
  )
}
