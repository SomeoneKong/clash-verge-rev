import { ask } from '@tauri-apps/plugin-dialog'
import { useLockFn } from 'ahooks'
import { useTranslation } from 'react-i18next'
import { mutate as swrMutate } from 'swr'

import { Switch, TooltipIcon } from '@/components/base'
import {
  type ConnStatsStatus,
  clearConnectionStats,
  getConnectionStatsStatus,
  setConnectionStats,
} from '@/services/cmds'
import { mutate } from '@/services/mutate'
import { useQuery } from '@/services/query-client'

import { SettingItem } from './setting-comp'

export const ConnectionStatsSetting = () => {
  const { t } = useTranslation()
  const { data: status, refetch } = useQuery<ConnStatsStatus>({
    queryKey: ['getConnectionStatsStatus'],
    queryFn: () => getConnectionStatsStatus(),
  })

  const toggle = useLockFn(async (enabled: boolean) => {
    await mutate(() => setConnectionStats(enabled), {
      id: 'set-connection-stats',
    })
    await refetch()
  })
  const onClear = useLockFn(async () => {
    const confirmed = await ask(t('connStats.clear.confirm'), {
      kind: 'warning',
    })
    if (!confirmed) return
    await mutate(() => clearConnectionStats(), { id: 'clear-connection-stats' })
    await refetch()
    // every range and drill-down the connections page read before
    await swrMutate(
      (key) => Array.isArray(key) && key[0] === 'queryConnectionStats',
      undefined,
      { revalidate: true },
    )
  })

  return (
    <>
      <SettingItem
        label={t('connStats.setting.title')}
        secondary={
          status?.settingLost ? t('connStats.status.settingLost') : undefined
        }
        extra={
          <TooltipIcon
            title={t('connStats.setting.tooltip')}
            sx={{ opacity: '0.7' }}
          />
        }
      >
        <Switch
          edge="end"
          checked={!!status?.recording}
          disabled={!status}
          onChange={(_, checked) => toggle(checked)}
        />
      </SettingItem>
      {status?.hasHistory && (
        <SettingItem label={t('connStats.clear.title')} onClick={onClear} />
      )}
    </>
  )
}
