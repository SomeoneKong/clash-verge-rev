import { ExpandLessRounded, ExpandMoreRounded } from '@mui/icons-material'
import {
  alpha,
  Box,
  Chip,
  Collapse,
  IconButton,
  Typography,
} from '@mui/material'
import { memo, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { BaseLoading } from '@/components/base'
import { useProxyDelayState } from '@/hooks/use-proxy-delay-state'
import delayManager from '@/services/delay'
import {
  CHIP_COLOR,
  levelOf,
  pickText,
  type ProbeViewMember,
} from '@/types/probe-view'
import {
  memberDetails,
  providerNameOf,
  type ProxyGroupView,
  type ResolvedProxyMember,
} from '@/types/proxy-view'

import { ProbeElements, ProbeSections } from './probe-elements'
import type { ResolvedMemberOccurrence } from './use-render-list'

interface Props {
  group: ProxyGroupView
  occurrence: ResolvedMemberOccurrence
  strategy: ProbeViewMember | null
  selected: boolean
  onClick: (member: ResolvedProxyMember) => void
}

const TAG_SX = {
  fontSize: 10,
  lineHeight: 1.5,
  px: 0.5,
  border: '1px solid',
  borderColor: 'text.secondary',
  borderRadius: 1,
  color: 'text.secondary',
} as const

/** A member in the strategy mode: one row with its place in the group, what it is, its role
 * and state, and its delay; the details of its strategy open within the row. */
export const MemberStrategyRow = memo(function MemberStrategyRow({
  group,
  occurrence,
  strategy,
  selected,
  onClick,
}: Props) {
  const { t, i18n } = useTranslation()
  const language = i18n.language
  const [open, setOpen] = useState(false)
  const member = occurrence.member
  const unresolved = member.kind === 'unresolved'
  const details = memberDetails(member)
  const name = member.ref.name
  const type = unresolved ? member.ref.reason : (details?.type ?? '')
  // Tells apart members of the same name from different providers.
  const provider =
    member.kind === 'node' ? providerNameOf(member.node) : undefined
  const tags = unresolved
    ? []
    : [
        details?.udp && 'UDP',
        details?.xudp && 'XUDP',
        details?.tfo && 'TFO',
        details?.mptcp && 'MPTCP',
        details?.smux && 'SMUX',
      ].filter((tag): tag is string => !!tag)
  const { delayValue, isPreset, timeout, onDelay } = useProxyDelayState(
    member,
    group.name,
  )
  const hasDetails = !!strategy && strategy.sections.length > 0

  return (
    <Box sx={{ px: 2, py: 0.375 }}>
      <Box
        sx={[
          {
            borderRadius: 1.5,
            border: '1px solid',
            borderColor: 'divider',
            overflow: 'hidden',
          },
          ({ palette: { mode, primary } }) => ({
            bgcolor: selected
              ? alpha(primary.main, mode === 'light' ? 0.12 : 0.3)
              : mode === 'light'
                ? '#ffffff'
                : '#24252f',
            borderLeft: selected
              ? `3px solid ${mode === 'light' ? primary.main : primary.light}`
              : undefined,
          }),
        ]}
      >
        <Box
          onClick={unresolved ? undefined : () => onClick(member)}
          sx={{
            display: 'flex',
            alignItems: 'center',
            flexWrap: { xs: 'wrap', md: 'nowrap' },
            columnGap: 1.5,
            rowGap: 0.5,
            px: 1.5,
            py: 0.75,
            cursor: unresolved ? 'default' : 'pointer',
            '&:hover': { bgcolor: 'action.hover' },
          }}
        >
          <Typography
            variant="body2"
            sx={{
              width: 36,
              flexShrink: 0,
              color: 'text.secondary',
              fontVariantNumeric: 'tabular-nums',
            }}
          >
            #{occurrence.memberIndex + 1}
          </Typography>
          <Box sx={{ flex: '1 1 220px', minWidth: 0 }} title={name}>
            <Typography
              variant="body2"
              noWrap
              sx={{ color: unresolved ? 'text.disabled' : 'text.primary' }}
            >
              {name}
              {group.fixed === name && ' 📌'}
            </Typography>
            <Box sx={{ display: 'flex', gap: 0.5, mt: 0.25 }}>
              {provider && (
                <Typography
                  component="span"
                  noWrap
                  title={provider}
                  sx={{ ...TAG_SX, minWidth: 0 }}
                >
                  {provider}
                </Typography>
              )}
              {[type, ...tags].filter(Boolean).map((tag) => (
                <Typography key={tag} component="span" sx={TAG_SX}>
                  {tag}
                </Typography>
              ))}
            </Box>
          </Box>
          <Box sx={{ width: 84, flexShrink: 0 }}>
            {strategy ? (
              <Chip
                size="small"
                color={CHIP_COLOR[levelOf(strategy.role.level)]}
                label={pickText(strategy.role.text, language)}
                title={pickText(strategy.role.tip, language) || undefined}
                sx={{ height: 22, fontSize: 12 }}
              />
            ) : null}
          </Box>
          <Box sx={{ flex: '2 1 300px', minWidth: 0 }}>
            {strategy ? (
              <ProbeElements items={strategy.headline} />
            ) : (
              <Typography variant="caption" sx={{ color: 'text.disabled' }}>
                {t('probeView.member.unavailable')}
              </Typography>
            )}
          </Box>
          <Box
            sx={{
              width: 64,
              flexShrink: 0,
              textAlign: 'right',
              visibility: isPreset || unresolved ? 'hidden' : undefined,
            }}
          >
            {delayValue === -2 ? (
              <BaseLoading />
            ) : (
              <Typography
                variant="body2"
                component="span"
                onClick={(event) => {
                  event.stopPropagation()
                  void onDelay()
                }}
                sx={{
                  px: 0.5,
                  borderRadius: 1,
                  cursor: 'pointer',
                  color:
                    delayValue >= 0
                      ? delayManager.formatDelayColor(delayValue, timeout)
                      : 'primary.main',
                  '&:hover': { bgcolor: 'action.selected' },
                }}
              >
                {delayValue >= 0
                  ? delayManager.formatDelay(delayValue, timeout)
                  : t('shared.actions.check')}
              </Typography>
            )}
          </Box>
          <Box sx={{ width: 32, flexShrink: 0 }}>
            {hasDetails && (
              <IconButton
                size="small"
                title={
                  open
                    ? t('probeView.member.collapse')
                    : t('probeView.member.expand')
                }
                onClick={(event) => {
                  event.stopPropagation()
                  setOpen(!open)
                }}
              >
                {open ? (
                  <ExpandLessRounded fontSize="small" />
                ) : (
                  <ExpandMoreRounded fontSize="small" />
                )}
              </IconButton>
            )}
          </Box>
        </Box>
        {hasDetails && (
          <Collapse in={open} unmountOnExit>
            <Box sx={{ px: 1.5, pb: 1, pl: 7 }}>
              <ProbeSections sections={strategy.sections} />
            </Box>
          </Collapse>
        )}
      </Box>
    </Box>
  )
})
