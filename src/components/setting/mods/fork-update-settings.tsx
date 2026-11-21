import { useTranslation } from 'react-i18next'

import { useUpdateOverview } from '@/hooks/use-fork-update'
import { useVerge } from '@/hooks/use-verge'
import { formatTime } from '@/types/probe-view'
import getSystem from '@/utils/get-system'

import { SettingItem } from './setting-comp'
import { UpdateHistorySetting } from './update-history-viewer'
import { VergeSwitch } from './verge-switch-settings'

const os = getSystem()

function AutoInstallUpdateSetting() {
  const { t, i18n } = useTranslation()
  const { verge } = useVerge()
  const enabled =
    verge?.auto_check_update !== false && !!verge?.auto_install_update
  const { data } = useUpdateOverview(null, enabled)
  const status = enabled ? data?.auto : null
  return (
    <SettingItem
      label={t('forkSettings.update.autoInstall')}
      secondary={
        status &&
        t(`forkSettings.update.${status.kind}`, {
          ...status,
          ...(status.kind === 'waiting' && {
            after: formatTime(status.after * 1000, i18n.language),
          }),
        })
      }
    >
      <VergeSwitch field="auto_install_update" fallback={false} />
    </SettingItem>
  )
}

export function ForkUpdateSettings() {
  return (
    <>
      {(os === 'windows' || os === 'linux') && <AutoInstallUpdateSetting />}
      <UpdateHistorySetting />
    </>
  )
}
