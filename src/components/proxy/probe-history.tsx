import { RefreshRounded } from '@mui/icons-material'
import {
  Alert,
  Box,
  Button,
  Chip,
  Dialog,
  DialogContent,
  DialogTitle,
  Divider,
  FormControlLabel,
  MenuItem,
  Select,
  Switch,
  Typography,
} from '@mui/material'
import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { useProbeView } from '@/hooks/use-probe-view'
import { queryProbeHistory, refreshProbeHistory } from '@/services/cmds'
import { showNotice } from '@/services/notice-service'
import {
  formatMoment,
  formatTime,
  levelOf,
  pickText,
  type ProbeHistoryEvent,
  type ProbeHistoryGap,
  type ProbeHistoryPage,
} from '@/types/probe-view'

const RANGES = ['1h', '6h', '24h', '3d', '7d'] as const
type Range = (typeof RANGES)[number]
const RANGE_SECONDS: Record<Range, number> = {
  '1h': 3600,
  '6h': 6 * 3600,
  '24h': 24 * 3600,
  '3d': 3 * 86400,
  '7d': 7 * 86400,
}
/** Rows drawn at a time; more on demand. */
const PAGE_ROWS = 300

const LEVEL_COLOR = {
  ok: 'success.main',
  info: 'text.primary',
  warn: 'warning.main',
  error: 'error.main',
  muted: 'text.disabled',
} as const

/** The member filter: by name, or one object of it in one core. */
interface MemberFilter {
  name: string
  instance?: string
  object?: number
}

/** A row of the timeline: an event, or a gap of records whose times are known. */
type Row =
  | { kind: 'event'; time: string; instance: string; event: ProbeHistoryEvent }
  | { kind: 'gap'; time: string; instance: string; gap: ProbeHistoryGap }

const rowKey = (row: Row) =>
  row.kind === 'event'
    ? `${row.instance}:${row.event.seq}`
    : `${row.instance}:gap:${row.gap.from}`

export const ProbeHistoryDialog = () => {
  const { history, closeHistory } = useProbeView()
  return (
    <ProbeHistoryBody
      key={history.token}
      open={history.open}
      initialGroup={history.group}
      onClose={closeHistory}
    />
  )
}

const ProbeHistoryBody = memo(function ProbeHistoryBody({
  open,
  initialGroup,
  onClose,
}: {
  open: boolean
  initialGroup?: string
  onClose: () => void
}) {
  const { t, i18n } = useTranslation()
  const language = i18n.language
  const [range, setRange] = useState<Range>('24h')
  const [group, setGroup] = useState<string>(initialGroup ?? '')
  const [includeGlobal, setIncludeGlobal] = useState(true)
  const [member, setMember] = useState<MemberFilter | null>(null)
  const [categories, setCategories] = useState<string[]>([])
  const [page, setPage] = useState<ProbeHistoryPage | null>(null)
  const [failed, setFailed] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [refreshing, setRefreshing] = useState(false)
  const [reload, setReload] = useState(0)
  const [shown, setShown] = useState(PAGE_ROWS)
  // Only the answer to the latest query is shown.
  const requestRef = useRef(0)

  const load = useCallback(async () => {
    const request = ++requestRef.current
    const until = Math.floor(Date.now() / 1000)
    setLoading(true)
    try {
      const result = await queryProbeHistory({
        since: until - RANGE_SECONDS[range],
        until,
        groups: group ? [group] : null,
        includeGlobal,
        member: member?.name ?? null,
        instance: member?.instance ?? null,
        object: member?.object ?? null,
        categories: categories.length > 0 ? categories : null,
      })
      if (request !== requestRef.current) return
      setPage(result as unknown as ProbeHistoryPage)
      setFailed(null)
      setShown(PAGE_ROWS)
    } catch (error) {
      if (request !== requestRef.current) return
      setPage(null)
      setFailed(String(error))
    } finally {
      if (request === requestRef.current) setLoading(false)
    }
  }, [range, group, includeGlobal, member, categories])

  useEffect(() => {
    if (open) void load()
  }, [open, load, reload])

  const refresh = async () => {
    setRefreshing(true)
    try {
      const result = await refreshProbeHistory()
      switch (result.status) {
        case 'caughtUp':
          showNotice.success(
            t('probeView.refresh.caughtUp', {
              seq: result.observedLatest ?? 0,
            }),
          )
          break
        case 'partial':
          showNotice.info(
            t('probeView.refresh.partial', { count: result.remaining ?? 0 }),
          )
          break
        case 'changed':
          showNotice.info(t('probeView.refresh.changed'))
          break
        case 'busy':
          showNotice.info(t('probeView.refresh.busy'))
          break
        case 'unsupported':
          showNotice.info(t('probeView.refresh.unsupported'))
          break
        default:
          showNotice.error(t('probeView.refresh.unreachable'))
      }
    } catch (error) {
      showNotice.error(t('probeView.refresh.failed', { error: String(error) }))
    } finally {
      setRefreshing(false)
      // read again with the filters as they are by then
      setReload((current) => current + 1)
    }
  }

  const coverageOf = useMemo(
    () =>
      new Map(
        (page?.instances ?? []).map((instance) => [
          instance.instance,
          instance,
        ]),
      ),
    [page],
  )

  // Newest first; a gap comes before the event it follows.
  const rows = useMemo(() => {
    const gaps: Row[] = (page?.instances ?? []).flatMap((instance) =>
      instance.gaps
        .filter((gap) => gap.afterTime && gap.beforeTime)
        .map((gap) => ({
          kind: 'gap' as const,
          time: gap.afterTime!,
          instance: instance.instance,
          gap,
        })),
    )
    const events: Row[] = [...(page?.events ?? [])].reverse().map((event) => ({
      kind: 'event' as const,
      time: event.time,
      instance: event.instance,
      event,
    }))
    return [...gaps, ...events].sort(
      (a, b) => Date.parse(b.time) - Date.parse(a.time),
    )
  }, [page])

  // the cores a restart separator shown stands after, which tells their tail there
  const separated = useMemo(() => {
    const set = new Set<string>()
    rows.slice(0, shown).forEach((row, index) => {
      if (index > 0 && rows[index - 1].instance !== row.instance) {
        set.add(row.instance)
      }
    })
    return set
  }, [rows, shown])

  const notes = (page?.instances ?? []).flatMap((instance) => {
    const short = instance.instance.slice(0, 8)
    const list: string[] = []
    if (instance.pending && instance.pending > 0)
      list.push(
        t(
          instance.ended
            ? 'probeView.coverage.lost'
            : 'probeView.coverage.pending',
          {
            count: instance.pending,
            instance: short,
          },
        ),
      )
    if (instance.gaps.some((gap) => !gap.afterTime || !gap.beforeTime))
      list.push(t('probeView.coverage.gapUnknown', { instance: short }))
    if (instance.tailUnknown && !separated.has(instance.instance))
      list.push(
        instance.observedAt
          ? t('probeView.coverage.tailAt', {
              instance: short,
              time: formatTime(instance.observedAt, language),
            })
          : t('probeView.coverage.tail', { instance: short }),
      )
    if (instance.coverageUnknown)
      list.push(t('probeView.coverage.unknown', { instance: short }))
    if (instance.display === false)
      list.push(t('probeView.coverage.noDisplay', { instance: short }))
    if (instance.display === null)
      list.push(t('probeView.coverage.displayUnknown', { instance: short }))
    return list
  })
  const running = (page?.instances ?? []).filter(
    (instance) => !instance.ended && instance.observedAt,
  )

  const memberNote = (event: ProbeHistoryEvent) => {
    if (!member) return null
    const matched = (event.display.members ?? []).filter(
      (m) =>
        m.name === member.name &&
        (member.object === undefined ||
          (event.instance === member.instance && m.object === member.object)),
    )
    if (matched.length === 0) return null
    return matched
      .map((m) =>
        t('probeView.history.object', {
          provider: m.provider || '-',
          object: m.object ?? 0,
        }),
      )
      .join(' ')
  }

  const memberOptions = [
    ...new Set([...(page?.members ?? []), ...(member ? [member.name] : [])]),
  ]
  const categoryOptions = [
    ...new Set([...(page?.categories ?? []), ...categories]),
  ]
  // the names the core gives the categories, on the events of the page
  const categoryNames = new Map<string, string>()
  for (const event of page?.events ?? []) {
    const { category, categoryText } = event.display
    if (category && categoryText && !categoryNames.has(category)) {
      categoryNames.set(category, pickText(categoryText, language))
    }
  }

  return (
    <Dialog open={open} onClose={onClose} maxWidth="lg" fullWidth>
      <DialogTitle>{t('probeView.history.title')}</DialogTitle>
      <DialogContent sx={{ maxHeight: '75vh' }}>
        <Box
          sx={{
            display: 'flex',
            flexWrap: 'wrap',
            gap: 1,
            alignItems: 'center',
            mb: 1,
          }}
        >
          <Select
            size="small"
            value={range}
            onChange={(e) => setRange(e.target.value as Range)}
          >
            {RANGES.map((r) => (
              <MenuItem key={r} value={r}>
                {t(`probeView.ranges.${r}`)}
              </MenuItem>
            ))}
          </Select>
          <Select
            size="small"
            displayEmpty
            value={group}
            onChange={(e) => setGroup(e.target.value)}
          >
            <MenuItem value="">{t('probeView.filters.allGroups')}</MenuItem>
            {[
              ...new Set([...(page?.groups ?? []), ...(group ? [group] : [])]),
            ].map((name) => (
              <MenuItem key={name} value={name}>
                {name}
              </MenuItem>
            ))}
          </Select>
          <Select
            size="small"
            displayEmpty
            value={member?.name ?? ''}
            onChange={(e) =>
              setMember(e.target.value ? { name: e.target.value } : null)
            }
          >
            <MenuItem value="">{t('probeView.filters.allMembers')}</MenuItem>
            {memberOptions.map((name) => (
              <MenuItem key={name} value={name}>
                {name}
              </MenuItem>
            ))}
          </Select>
          <FormControlLabel
            control={
              <Switch
                size="small"
                checked={includeGlobal}
                onChange={(e) => setIncludeGlobal(e.target.checked)}
              />
            }
            label={t('probeView.filters.includeGlobal')}
          />
          <Box sx={{ flex: 1 }} />
          <Button
            size="small"
            startIcon={<RefreshRounded />}
            onClick={() => void refresh()}
            disabled={loading || refreshing}
          >
            {t('probeView.actions.refresh')}
          </Button>
        </Box>
        <Box sx={{ display: 'flex', flexWrap: 'wrap', gap: 0.5, mb: 1 }}>
          {categoryOptions.map((category) => (
            <Chip
              key={category}
              size="small"
              label={categoryNames.get(category) ?? category}
              color={categories.includes(category) ? 'primary' : 'default'}
              onClick={() =>
                setCategories((current) =>
                  current.includes(category)
                    ? current.filter((c) => c !== category)
                    : [...current, category],
                )
              }
            />
          ))}
          {member && (
            <>
              <Chip
                size="small"
                color="secondary"
                label={t('probeView.filters.member', { name: member.name })}
                onDelete={() => setMember(null)}
              />
              {member.object === undefined ? null : (
                <Chip
                  size="small"
                  variant="outlined"
                  label={t('probeView.filters.allObjects')}
                  onClick={() => setMember({ name: member.name })}
                />
              )}
            </>
          )}
        </Box>
        {running.map((instance) => (
          <Typography
            key={instance.instance}
            variant="caption"
            sx={{ color: 'text.secondary' }}
            component="div"
          >
            {t('probeView.coverage.observed', {
              instance: instance.instance.slice(0, 8),
              time: formatMoment(instance.observedAt!, language),
            })}
          </Typography>
        ))}
        {failed && (
          <Alert severity="error" variant="outlined" sx={{ my: 1 }}>
            {t('probeView.history.failed', { error: failed })}
          </Alert>
        )}
        {notes.length > 0 && (
          <Alert severity="warning" variant="outlined" sx={{ my: 1 }}>
            {notes.map((note) => (
              <div key={note}>{note}</div>
            ))}
          </Alert>
        )}
        {page?.truncated && (
          <Alert severity="info" variant="outlined" sx={{ my: 1 }}>
            {t('probeView.history.truncated')}
          </Alert>
        )}
        {page && rows.length === 0 && !loading && (
          <Typography variant="body2" sx={{ color: 'text.secondary', py: 2 }}>
            {t('probeView.history.empty')}
          </Typography>
        )}
        <Box sx={{ display: 'grid', gap: 0.5 }}>
          {rows.slice(0, shown).map((row, index) => {
            const previous = rows[index - 1]
            const restarted = previous && previous.instance !== row.instance
            const older = restarted ? coverageOf.get(row.instance) : undefined
            const tail = older?.tailUnknown
              ? older.observedAt
                ? t('probeView.history.restartTailAt', {
                    time: formatTime(older.observedAt, language),
                  })
                : t('probeView.history.restartTail')
              : ''
            return (
              <Box key={rowKey(row)}>
                {restarted && (
                  <Divider sx={{ my: 0.5 }}>
                    <Typography
                      variant="caption"
                      sx={{ color: 'text.secondary' }}
                    >
                      {t('probeView.history.restart')}
                      {tail ? ` · ${tail}` : ''}
                    </Typography>
                  </Divider>
                )}
                {row.kind === 'gap' ? (
                  <Typography
                    variant="caption"
                    component="div"
                    sx={{ color: 'warning.main' }}
                  >
                    {t('probeView.history.gap', {
                      from: formatTime(row.gap.afterTime!, language),
                      to: formatTime(row.gap.beforeTime!, language),
                      count: row.gap.to - row.gap.from + 1,
                    })}
                  </Typography>
                ) : (
                  <EventRow
                    event={row.event}
                    language={language}
                    note={memberNote(row.event)}
                    member={member}
                    onMember={setMember}
                  />
                )}
              </Box>
            )
          })}
        </Box>
        {rows.length > shown && (
          <Button
            size="small"
            sx={{ mt: 1 }}
            onClick={() => setShown((current) => current + PAGE_ROWS)}
          >
            {t('probeView.history.loadMore', {
              count: Math.min(PAGE_ROWS, rows.length - shown),
            })}
          </Button>
        )}
      </DialogContent>
    </Dialog>
  )
})

const EventRow = ({
  event,
  language,
  note,
  member,
  onMember,
}: {
  event: ProbeHistoryEvent
  language: string
  note: string | null
  member: MemberFilter | null
  onMember: (member: MemberFilter) => void
}) => {
  const level = levelOf(event.display.level)
  const members = (event.display.members ?? []).filter((m) => m.name)
  return (
    <>
      <Box sx={{ display: 'flex', gap: 1, alignItems: 'baseline' }}>
        <Typography
          variant="caption"
          sx={{ color: 'text.secondary', whiteSpace: 'nowrap' }}
        >
          {formatTime(event.time, language)}
        </Typography>
        <Chip
          size="small"
          label={
            pickText(event.display.categoryText, language) ||
            event.display.category ||
            '-'
          }
          sx={{ height: 18, fontSize: 10, flexShrink: 0 }}
        />
        <Typography variant="body2" sx={{ color: LEVEL_COLOR[level], flex: 1 }}>
          {pickText(event.display.text, language)}
          {note && (
            <Typography
              component="span"
              variant="caption"
              sx={{ color: 'text.secondary', ml: 1 }}
            >
              {note}
            </Typography>
          )}
        </Typography>
      </Box>
      <Box sx={{ display: 'flex', gap: 1, pl: 2 }}>
        {members.map((m) => {
          // members of the same name are told apart by where they come from
          const same = members.filter((other) => other.name === m.name).length
          const from = [m.provider, same > 1 ? `#${m.object ?? 0}` : '']
            .filter(Boolean)
            .join(' ')
          return (
            <Typography
              key={`${m.name}:${m.provider ?? ''}:${m.object ?? 0}`}
              variant="caption"
              sx={{ cursor: 'pointer', color: 'primary.main' }}
              onClick={() =>
                onMember(
                  member?.name === m.name && member.object === undefined
                    ? {
                        name: m.name,
                        instance: event.instance,
                        object: m.object,
                      }
                    : { name: m.name },
                )
              }
            >
              {m.name}
              {from ? ` (${from})` : ''}
            </Typography>
          )
        })}
      </Box>
    </>
  )
}
