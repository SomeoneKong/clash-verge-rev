import { Box, Button, CircularProgress, Stack, Typography } from '@mui/material'
import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { SWRConfig } from 'swr'

import { BaseDialog } from '@/components/base'
import {
  displayVersion,
  useUpdateHistory,
  useUpdateOverview,
} from '@/hooks/use-fork-update'
import { forkRepairUpdate, upgradeClashCore } from '@/services/cmds'
import { mutate } from '@/services/mutate'
import { errorDetail, failureCode, showNotice } from '@/services/notice-service'
import { formatTime } from '@/types/probe-view'

import { SettingItem } from './setting-comp'

const eventKeys = {
  confirm_timeout: 'forkSettings.updateEvents.confirm_timeout',
  recovery_blocked: 'forkSettings.updateEvents.recovery_blocked',
  relaunch_skipped: 'forkSettings.updateEvents.relaunch_skipped',
  relaunched: 'forkSettings.updateEvents.relaunched',
  relaunch_failed: 'forkSettings.updateEvents.relaunch_failed',
  abandoned: 'forkSettings.updateEvents.abandoned',
  rejected: 'forkSettings.updateEvents.rejected',
  cancelled: 'forkSettings.updateEvents.cancelled',
  worker_stop_requested: 'forkSettings.updateEvents.worker_stop_requested',
  user_phase: 'forkSettings.updateEvents.user_phase',
  repair_superseded: 'forkSettings.updateEvents.repair_superseded',
  floor_reset: 'forkSettings.updateEvents.floor_reset',
  installed: 'forkSettings.updateEvents.installed',
  installed_unchanged: 'forkSettings.updateEvents.installed_unchanged',
  log_truncated: 'forkSettings.updateEvents.log_truncated',
  claim: 'forkSettings.updateEvents.claim',
  install: 'forkSettings.updateEvents.install',
  ok: 'forkSettings.updateEvents.ok',
  keep: 'forkSettings.updateEvents.keep',
  revert: 'forkSettings.updateEvents.revert',
  staged: 'forkSettings.updateEvents.staged',
  applying: 'forkSettings.updateEvents.applying',
  applied: 'forkSettings.updateEvents.applied',
  failed: 'forkSettings.updateEvents.failed',
  rolled_back: 'forkSettings.updateEvents.rolled_back',
  needs_repair: 'forkSettings.updateEvents.needs_repair',
  superseded: 'forkSettings.updateEvents.superseded',
  published: 'forkSettings.updateEvents.published',
  confirmed: 'forkSettings.updateEvents.confirmed',
  reconciled: 'forkSettings.updateEvents.reconciled',
  auto_install_failed: 'forkSettings.updateEvents.auto_install_failed',
  fallback: 'forkSettings.updateEvents.fallback',
} as const

const triggerKeys = {
  auto: 'forkSettings.update.auto',
  manual: 'forkSettings.update.manual',
  core: 'forkSettings.updateEvents.core',
  repair: 'forkSettings.updateEvents.repair',
} as const

export function HistoryList() {
  const { t, i18n } = useTranslation()
  const { data, error, size, setSize, isValidating } = useUpdateHistory()
  const sentinelRef = useRef<HTMLDivElement>(null)
  const containerRef = useRef<HTMLDivElement>(null)
  const ended = data?.at(-1)?.length === 0
  useEffect(() => {
    const target = sentinelRef.current
    if (!target || isValidating || ended || error || data?.length !== size)
      return
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry.isIntersecting) {
          observer.disconnect()
          void setSize(size + 1)
        }
      },
      { root: containerRef.current },
    )
    observer.observe(target)
    return () => observer.disconnect()
  }, [isValidating, ended, data, size, setSize, error])
  return (
    <Box ref={containerRef} sx={{ overflowY: 'auto', minHeight: 0 }}>
      {data?.flat().map((record) => {
        const source = record.source as
          | 'actual'
          | 'installer'
          | 'claimed'
          | 'observed'
        const trigger =
          record.trigger && Object.hasOwn(triggerKeys, record.trigger)
            ? t(triggerKeys[record.trigger as keyof typeof triggerKeys])
            : record.trigger
        const eventLabel = (value: string) =>
          Object.hasOwn(eventKeys, value)
            ? t(eventKeys[value as keyof typeof eventKeys])
            : value
        return (
          <Box
            key={record.seq}
            sx={{
              py: 1,
              borderBottom: 1,
              borderColor: 'divider',
              overflowWrap: 'anywhere',
            }}
          >
            <Typography variant="body2">
              {formatTime(record.time, i18n.language)} ·{' '}
              {t(`forkSettings.update.${source}`)}
            </Typography>
            {source !== 'observed' && (
              <Typography>{eventLabel(record.event)}</Typography>
            )}
            <Typography variant="body2">
              {[
                record.from && record.to
                  ? `${displayVersion(record.from)} → ${displayVersion(record.to)}`
                  : record.to && displayVersion(record.to),
                trigger,
                record.result !== record.event &&
                  source !== 'observed' &&
                  record.result &&
                  eventLabel(record.result),
              ]
                .filter(Boolean)
                .join(' · ')}
            </Typography>
            {record.error && (
              <Typography color="error" variant="body2">
                {record.error}
              </Typography>
            )}
          </Box>
        )
      })}
      {failureCode(error) === 'UPDATE_COMPONENT_UNAVAILABLE' ? (
        <Typography>
          {t('forkSettings.update.historyUnavailable')}{' '}
          <code>clash-verge-updater log</code>
        </Typography>
      ) : error ? (
        <Typography color="error">{errorDetail(error)}</Typography>
      ) : !data?.[0]?.length && ended ? (
        <Typography>{t('forkSettings.update.empty')}</Typography>
      ) : (
        !ended && (
          <Box ref={sentinelRef} sx={{ minHeight: 20 }}>
            {isValidating && <CircularProgress size={16} />}
          </Box>
        )
      )}
    </Box>
  )
}

export function UpdateHistorySetting() {
  const { t } = useTranslation()
  const [session, setSession] = useState<number | null>(null)
  const [working, setWorking] = useState(false)
  const overview = useUpdateOverview()
  const app = overview.data?.appRepair
  const core = overview.data?.coreRepair
  const repair = async (action: () => Promise<unknown>) => {
    setWorking(true)
    try {
      await action()
    } catch (error) {
      showNotice.error(error)
    } finally {
      await overview.refetch()
      setSession((value) => (value === null ? null : Date.now()))
      setWorking(false)
    }
  }
  return (
    <>
      <SettingItem
        label={t('forkSettings.update.history')}
        secondary={(app || core) && t('forkSettings.update.pending')}
        onClick={() => {
          setSession(Date.now())
          void overview.refetch()
        }}
      />
      <BaseDialog
        open={session !== null}
        title={t('forkSettings.update.history')}
        disableOk
        cancelBtn={t('shared.actions.close')}
        onCancel={() => setSession(null)}
        onClose={() => setSession(null)}
        contentSx={{
          width: { xs: 'calc(100vw - 64px)', sm: 500 },
          maxWidth: '100%',
          height: '60vh',
          display: 'flex',
          flexDirection: 'column',
        }}
      >
        <Stack spacing={1}>
          {app && overview.data?.next.kind === 'fullRepair' && (
            <Typography>
              {t('forkSettings.update.fullRepair', { version: app.version })}
            </Typography>
          )}
          {app &&
            (app.available ? (
              <Button
                disabled={working || overview.data?.next.kind === 'busy'}
                onClick={() => void repair(() => forkRepairUpdate(app.txn))}
              >
                {t('forkSettings.update.repairApp', { version: app.version })}
              </Button>
            ) : (
              <Typography>
                {t('forkSettings.update.dmg', { version: app.version })}
              </Typography>
            ))}
          {core && (
            <Button
              disabled={working || overview.data?.next.kind === 'busy'}
              onClick={() =>
                void repair(() =>
                  mutate(() => upgradeClashCore(), {
                    id: 'upgrade-clash-core',
                    errorNotice: false,
                  }),
                )
              }
            >
              {t('forkSettings.update.repairCore', {
                version: displayVersion(core),
              })}
            </Button>
          )}
        </Stack>
        {session !== null && (
          <SWRConfig key={session} value={{ provider: () => new Map() }}>
            <HistoryList />
          </SWRConfig>
        )}
      </BaseDialog>
    </>
  )
}
