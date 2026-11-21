import { Box } from '@mui/material'
import { useTranslation } from 'react-i18next'

import type { NextStep } from '@/services/bindings'

export function UpdateStatus({
  next,
  fallbackReason,
}: {
  next?: NextStep
  fallbackReason?: string
}) {
  const { t } = useTranslation()
  return (
    <Box role="status" sx={{ pt: 1 }}>
      {fallbackReason
        ? t('forkSettings.update.fallback', { reason: fallbackReason })
        : next
          ? t(`forkSettings.update.${next.kind}`, { ...next })
          : t('forkSettings.update.unknown')}
      {next?.kind === 'bootstrap' && !fallbackReason && (
        <> {t('forkSettings.update.macComponent')}</>
      )}
    </Box>
  )
}
