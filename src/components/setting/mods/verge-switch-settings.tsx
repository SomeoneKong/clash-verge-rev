import { useTranslation } from 'react-i18next'

import { Switch, TooltipIcon } from '@/components/base'
import { useVerge } from '@/hooks/use-verge'
import { showNotice } from '@/services/notice-service'

import { GuardState } from './guard-state'
import { SettingItem } from './setting-comp'

type VergeSwitchKey = 'auto_close_connection' | 'auto_check_update'

const VergeSwitch = ({
  field,
  fallback,
}: {
  field: VergeSwitchKey
  fallback: boolean
}) => {
  const { verge, mutateVerge, patchVerge } = useVerge()
  return (
    <GuardState
      value={verge?.[field] ?? fallback}
      valueProps="checked"
      onCatch={showNotice.error}
      onFormat={(_: unknown, value: boolean) => value}
      onChange={(value) => mutateVerge({ ...verge, [field]: value }, false)}
      onGuard={(value) => patchVerge({ [field]: value })}
    >
      <Switch edge="end" />
    </GuardState>
  )
}

export const AutoCloseConnectionSetting = () => {
  const { t } = useTranslation()
  return (
    <SettingItem
      label={t('forkSettings.autoCloseConnections.title')}
      extra={
        <TooltipIcon
          title={t('forkSettings.autoCloseConnections.tooltip')}
          sx={{ opacity: '0.7' }}
        />
      }
    >
      <VergeSwitch field="auto_close_connection" fallback={false} />
    </SettingItem>
  )
}

export const AutoCheckUpdateSetting = () => {
  const { t } = useTranslation()
  return (
    <SettingItem label={t('settings.modals.misc.fields.autoCheckUpdate')}>
      <VergeSwitch field="auto_check_update" fallback={true} />
    </SettingItem>
  )
}
