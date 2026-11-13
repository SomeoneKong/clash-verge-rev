import { FileDownloadOutlined, RefreshRounded } from '@mui/icons-material'
import {
  Alert,
  Box,
  Checkbox,
  FormControlLabel,
  IconButton,
  MenuItem,
  Stack,
  Tooltip,
} from '@mui/material'
import { save } from '@tauri-apps/plugin-dialog'
import { useLockFn } from 'ahooks'
import dayjs from 'dayjs'
import { useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { BaseEmpty, BasePage, BaseStyledSelect } from '@/components/base'
import { UsageChart } from '@/components/usage/usage-chart'
import {
  OTHER_COLOR,
  USAGE_COLORS,
  type UsageMetric,
} from '@/components/usage/usage-format'
import { routeLabel, type RouteText } from '@/components/usage/usage-route'
import { UsageRouteTree } from '@/components/usage/usage-route-tree'
import { UsageTable, type UsageRow } from '@/components/usage/usage-table'
import {
  exportProxyUsage,
  getProxyUsage,
  type UsageGapReason,
  type UsageItem,
  type UsagePoint,
  type UsageRange,
  type UsageReport,
  type UsageView,
} from '@/services/cmds'
import { mutate } from '@/services/mutate'
import { useQuery } from '@/services/query-client'

const REFRESH_INTERVAL = 60_000

const hourTime = (hour: number) => dayjs.unix(hour * 3600)

const UsagePage = () => {
  const { t } = useTranslation()
  const [range, setRange] = useState<UsageRange>('last24_hours')
  const [view, setView] = useState<UsageView>('route')
  const [metric, setMetric] = useState<UsageMetric>('total')
  const [includeBuiltin, setIncludeBuiltin] = useState(false)

  const { data, error, refetch } = useQuery<UsageReport>({
    queryKey: ['getProxyUsage', range, view, includeBuiltin],
    queryFn: () => getProxyUsage({ range, view, includeBuiltin }),
    refetchInterval: REFRESH_INTERVAL,
  })

  const ranges: Record<UsageRange, string> = {
    last24_hours: t('usage.ranges.last24Hours'),
    last7_days: t('usage.ranges.last7Days'),
    last30_days: t('usage.ranges.last30Days'),
    last12_months: t('usage.ranges.last12Months'),
    all: t('usage.ranges.all'),
  }
  const views: Record<UsageView, string> = {
    route: t('usage.views.route'),
    name: t('usage.views.name'),
  }
  const metrics: Record<UsageMetric, string> = {
    total: t('usage.metrics.total'),
    upload: t('usage.metrics.upload'),
    download: t('usage.metrics.download'),
  }
  const gapReasons: Record<UsageGapReason, string> = {
    core_lost: t('usage.gapReasons.coreLost'),
    expired: t('usage.gapReasons.expired'),
    corrupt: t('usage.gapReasons.corrupt'),
  }

  const routeText = useMemo<RouteText>(
    () => ({
      unknownRelay: (dialer) =>
        t('usage.labels.unknownRelay', { name: dialer }),
      proxyName: (item: UsageItem) => {
        if (item.overflow) return t('usage.labels.overflow')
        if (item.unknown) return `${t('usage.labels.unknown')} (${item.label})`
        return item.label
      },
    }),
    [t],
  )
  // a route view item shows its whole route
  const series = useMemo(
    () => [
      ...(data?.items ?? []).map((item) =>
        item.route ? routeLabel(item, routeText) : routeText.proxyName(item),
      ),
      t('usage.labels.other'),
    ],
    [data, routeText, t],
  )
  const colors = useMemo(
    () => [
      ...(data?.items ?? []).map(
        (_, index) => USAGE_COLORS[index % USAGE_COLORS.length],
      ),
      OTHER_COLOR,
    ],
    [data],
  )

  const rows: UsageRow[] = useMemo(() => {
    if (!data) return []
    const items = data.items.map((item, index) => ({
      key: item.key,
      label: series[index],
      detail:
        [item.provider, item.proxyType].filter(Boolean).join(' · ') || null,
      color: colors[index],
      total: item.total,
    }))
    const hasOther = data.other[0] + data.other[1] > 0
    return hasOther
      ? [
          ...items,
          {
            key: 'other',
            label: t('usage.labels.other'),
            detail: null,
            color: OTHER_COLOR,
            total: data.other,
          },
        ]
      : items
  }, [data, series, colors, t])

  const axisLabel = (point: UsagePoint) => {
    if (point.label) {
      return data?.granularity === 'day' ? point.label.slice(5) : point.label
    }
    return range === 'last24_hours'
      ? hourTime(point.startHour).format('HH:mm')
      : hourTime(point.startHour).format('MM-DD HH:mm')
  }
  const tooltipLabel = (point: UsagePoint) =>
    point.label ?? hourTime(point.startHour).format('YYYY-MM-DD HH:mm')

  const handleExport = useLockFn(async () => {
    const path = await save({
      defaultPath: `proxy-usage-${dayjs().format('YYYYMMDD-HHmm')}.json`,
      filters: [{ name: 'JSON', extensions: ['json'] }],
    })
    if (!path || Array.isArray(path)) return
    try {
      await mutate(() => exportProxyUsage(path), {
        id: 'export-proxy-usage',
        successNotice: 'usage.messages.exported',
      })
    } catch {
      // The failure is already shown.
    }
  })

  const routeColors = useMemo(
    () =>
      new Map(
        (data?.items ?? []).map((item, index) => [item.key, colors[index]]),
      ),
    [data, colors],
  )

  const notes = data ? usageNotes(data, t, gapReasons) : []
  // Gaps are drawn on the time axis even without any recorded usage.
  const isEmpty = !!data && rows.length === 0 && data.gaps.length === 0

  return (
    <BasePage
      title={t('usage.page.title')}
      contentStyle={{ height: '100%' }}
      header={
        <Box sx={{ display: 'flex', alignItems: 'center', gap: 1 }}>
          <Tooltip title={t('usage.actions.refresh')}>
            <IconButton size="small" onClick={() => void refetch()}>
              <RefreshRounded />
            </IconButton>
          </Tooltip>
          <Tooltip title={t('usage.actions.export')}>
            <IconButton size="small" onClick={handleExport}>
              <FileDownloadOutlined />
            </IconButton>
          </Tooltip>
        </Box>
      }
    >
      <Stack spacing={1.5} sx={{ p: 2 }}>
        <Box
          sx={{
            display: 'flex',
            alignItems: 'center',
            flexWrap: 'wrap',
            gap: 1,
          }}
        >
          <BaseStyledSelect
            value={range}
            onChange={(event) => setRange(event.target.value as UsageRange)}
          >
            {Object.entries(ranges).map(([value, label]) => (
              <MenuItem key={value} value={value}>
                {label}
              </MenuItem>
            ))}
          </BaseStyledSelect>
          <BaseStyledSelect
            value={view}
            onChange={(event) => setView(event.target.value as UsageView)}
          >
            {Object.entries(views).map(([value, label]) => (
              <MenuItem key={value} value={value}>
                {label}
              </MenuItem>
            ))}
          </BaseStyledSelect>
          <BaseStyledSelect
            value={metric}
            onChange={(event) => setMetric(event.target.value as UsageMetric)}
          >
            {Object.entries(metrics).map(([value, label]) => (
              <MenuItem key={value} value={value}>
                {label}
              </MenuItem>
            ))}
          </BaseStyledSelect>
          <FormControlLabel
            sx={{ mr: 0 }}
            control={
              <Checkbox
                size="small"
                checked={includeBuiltin}
                onChange={(event) => setIncludeBuiltin(event.target.checked)}
              />
            }
            label={t('usage.actions.includeBuiltin')}
          />
        </Box>
        {error && <Alert severity="error">{String(errorDetail(error))}</Alert>}
        {notes.map((note) => (
          <Alert key={note.text} severity={note.severity}>
            {note.text}
          </Alert>
        ))}
        {isEmpty && <BaseEmpty textKey="usage.empty" />}
        {data && !isEmpty && (
          <>
            <UsageChart
              points={data.points}
              keys={[...data.items.map((item) => item.key), 'other']}
              endHour={data.toHour + 1}
              series={series}
              colors={colors}
              metric={metric}
              gaps={data.gaps}
              axisLabel={axisLabel}
              tooltipLabel={tooltipLabel}
            />
            {view === 'route'
              ? data.allItems.length > 0 && (
                  <UsageRouteTree
                    items={data.allItems}
                    colors={routeColors}
                    text={routeText}
                  />
                )
              : rows.length > 0 && <UsageTable rows={rows} />}
          </>
        )}
      </Stack>
    </BasePage>
  )
}

const errorDetail = (error: unknown) =>
  error && typeof error === 'object' && 'detail' in error
    ? (error as { detail: unknown }).detail
    : error

type Translate = ReturnType<typeof useTranslation>['t']

const usageNotes = (
  data: UsageReport,
  t: Translate,
  gapReasons: Record<UsageGapReason, string>,
) => {
  const notes: { severity: 'info' | 'warning'; text: string }[] = []
  if (data.overflow) {
    notes.push({ severity: 'warning', text: t('usage.notes.overflow') })
  }
  if (data.gaps.length > 0) {
    const reasons = [...new Set(data.gaps.map((gap) => gapReasons[gap.reason]))]
    notes.push({
      severity: 'warning',
      text: t('usage.notes.gaps', {
        count: data.gaps.length,
        reasons: reasons.join(', '),
      }),
    })
  }
  if (data.coverageUnknownBefore !== null) {
    notes.push({
      severity: 'info',
      text: t('usage.notes.coverageUnknown', {
        time: hourTime(data.coverageUnknownBefore).format('YYYY-MM-DD HH:mm'),
      }),
    })
  }
  if (data.timeAnomalyDays.length > 0) {
    notes.push({
      severity: 'info',
      text: t('usage.notes.timeAnomaly', {
        days: data.timeAnomalyDays.join(', '),
      }),
    })
  }
  // The current hour is always still being counted; earlier ones, or several, mean collection
  // is behind.
  const currentHour = Math.floor(Date.now() / 3_600_000)
  if (
    data.pending &&
    (data.pending[0] < currentHour || data.pending[0] < data.pending[1])
  ) {
    notes.push({
      severity: 'info',
      text: t('usage.notes.pending', {
        time: hourTime(data.pending[0]).format('YYYY-MM-DD HH:mm'),
      }),
    })
  }
  return notes
}

export default UsagePage
