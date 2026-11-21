import { ask } from '@tauri-apps/plugin-dialog'
import { useTranslation } from 'react-i18next'

import { notifyProbeMemoryCleared } from '@/services/bus/probe-memory'
import { clearProbeMemory } from '@/services/cmds'
import { mutate } from '@/services/mutate'
import { showNotice } from '@/services/notice-service'

import { SettingItem } from './setting-comp'

// The clear in flight outlives the settings page: one opened again meanwhile waits for it
// instead of sending another.
let clearing: Promise<void> | null = null

export const ProbeMemorySetting = () => {
  const { t } = useTranslation()

  const clear = async () => {
    const confirmed = await ask(t('probeView.clear.confirm'), {
      kind: 'warning',
    }).catch((error) => {
      showNotice.error(error)
      return false
    })
    if (!confirmed) return

    const mutation = await mutate(clearProbeMemory, {
      id: 'clear-probe-memory',
      errorNotice: false,
    }).catch(() => undefined)
    const outcome = mutation?.value
    switch (outcome?.status) {
      case 'ok': {
        const { groups, pending } = outcome.result
        notifyProbeMemoryCleared()
        if (pending > 0) {
          showNotice.success('probeView.clear.successPending', {
            groups,
            pending,
          })
        } else {
          showNotice.success('probeView.clear.success', { groups })
        }
        break
      }
      case 'unsupported':
        showNotice.info('probeView.clear.unsupported')
        break
      default:
        showNotice.warning('probeView.clear.unconfirmed')
    }
  }
  const onClear = () => {
    clearing ??= clear().finally(() => {
      clearing = null
    })
    return clearing
  }

  // SettingItem disables the row while a native async callback is pending.
  return (
    <SettingItem
      label={t('probeView.clear.title')}
      onClick={async () => onClear()}
    />
  )
}
