import { Alert, Box, Chip, Collapse, Typography } from '@mui/material'
import { memo, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { useProbeView } from '@/hooks/use-probe-view'
import {
  formatMoment,
  levelOf,
  pickText,
  PROBE_VIEW_VERSION,
  type ProbeElement,
  type ProbeLevel,
  type ProbeSection,
  type ProbeViewGroup,
  type ProbeViewMember,
  elementKey,
} from '@/types/probe-view'

const CHIP_COLOR: Record<
  ProbeLevel,
  'success' | 'info' | 'warning' | 'error' | 'default'
> = {
  ok: 'success',
  info: 'info',
  warn: 'warning',
  error: 'error',
  muted: 'default',
}

const TEXT_COLOR: Record<ProbeLevel, string> = {
  ok: 'success.main',
  info: 'text.primary',
  warn: 'warning.main',
  error: 'error.main',
  muted: 'text.disabled',
}

/** One element of the view; one of a type this GUI does not know is left out. */
const ProbeElementView = memo(function ProbeElementView({
  element,
}: {
  element: ProbeElement
}) {
  const { i18n } = useTranslation()
  const language = i18n.language
  const level = levelOf(element.level)
  switch (element.type) {
    case 'badge':
      return (
        <Chip
          size="small"
          variant="outlined"
          color={CHIP_COLOR[level]}
          label={pickText(element.text, language)}
          title={
            pickText(element.tip, language) ||
            pickText(element.text, language) ||
            undefined
          }
          sx={{ height: 20, fontSize: 11 }}
        />
      )
    case 'metric':
      return (
        <Typography
          component="span"
          variant="caption"
          sx={{ color: TEXT_COLOR[level], whiteSpace: 'nowrap' }}
        >
          {pickText(element.label, language)}:{' '}
          {pickText(element.text, language)}
        </Typography>
      )
    case 'detail':
      return (
        <Typography
          variant="caption"
          component="div"
          sx={{ color: TEXT_COLOR[level] }}
        >
          <b>{pickText(element.label, language)}</b>
          {': '}
          {pickText(element.text, language)}
        </Typography>
      )
    case 'time':
      return (
        <Typography
          variant="caption"
          component="div"
          sx={{ color: 'text.secondary' }}
        >
          <b>{pickText(element.label, language)}</b>
          {': '}
          {element.at ? formatMoment(element.at, language) : ''}
          {element.text ? ` ${pickText(element.text, language)}` : ''}
        </Typography>
      )
    default:
      return null
  }
})

const ProbeElements = ({ items }: { items: ProbeElement[] }) => (
  <Box
    sx={{ display: 'flex', flexWrap: 'wrap', alignItems: 'center', gap: 0.75 }}
  >
    {items.map((element) => (
      <ProbeElementView key={elementKey(element)} element={element} />
    ))}
  </Box>
)

const ProbeSections = ({ sections }: { sections: ProbeSection[] }) => {
  const { i18n } = useTranslation()
  return (
    <Box sx={{ display: 'grid', gap: 1, mt: 1 }}>
      {sections.map((section) => (
        <Box key={pickText(section.title, 'en')}>
          <Typography
            variant="caption"
            sx={{ fontWeight: 700, color: 'text.secondary' }}
          >
            {pickText(section.title, i18n.language)}
          </Typography>
          <Box sx={{ display: 'grid', gap: 0.25, pl: 1 }}>
            {section.items.map((element) => (
              <ProbeElementView key={elementKey(element)} element={element} />
            ))}
          </Box>
        </Box>
      ))}
    </Box>
  )
}

/** The headline of the strategy of a group and its history, in the header of the group,
 * folded or not. */
export const GroupStrategyLine = ({ name }: { name: string }) => {
  const { t, i18n } = useTranslation()
  const { groups, openHistory } = useProbeView()
  const group = groups?.get(name)
  if (!group) return null
  // the headline gives way to the history, and shows whole on hover
  const whole = group.headline
    .map((element) =>
      [
        pickText(element.label, i18n.language),
        pickText(element.text, i18n.language),
      ]
        .filter(Boolean)
        .join(': '),
    )
    .join(' · ')
  return (
    <Box
      component="span"
      sx={{
        display: 'inline-flex',
        alignItems: 'center',
        gap: 0.75,
        ml: 1,
        // as narrow as the history at least: the headline shrinks first
        flex: '0 1 auto',
      }}
    >
      <Typography
        component="span"
        variant="caption"
        sx={{
          cursor: 'pointer',
          color: 'primary.main',
          whiteSpace: 'nowrap',
          flexShrink: 0,
        }}
        onClick={(event) => {
          event.stopPropagation()
          openHistory(name)
        }}
      >
        {t('probeView.actions.history')}
      </Typography>
      <Box
        component="span"
        title={whole}
        sx={{
          display: 'inline-flex',
          alignItems: 'center',
          gap: 0.75,
          minWidth: 0,
          overflow: 'hidden',
        }}
      >
        {group.headline.map((element) => (
          <ProbeElementView key={elementKey(element)} element={element} />
        ))}
      </Box>
    </Box>
  )
}

/** The details of the strategy of a group, under its header once it is open. */
export const GroupStrategy = memo(function GroupStrategy({
  group,
}: {
  group: ProbeViewGroup
}) {
  return (
    <Box
      sx={{
        mx: 2,
        my: 0.5,
        px: 1.5,
        pb: 1,
        borderRadius: 1,
        bgcolor: 'action.hover',
      }}
    >
      <ProbeSections sections={group.sections} />
    </Box>
  )
})

/** The strategy of a member, under its row; unavailable when the view does not match it. */
export const MemberStrategy = memo(function MemberStrategy({
  member,
}: {
  member: ProbeViewMember | null
}) {
  const { t, i18n } = useTranslation()
  const [open, setOpen] = useState(false)
  if (!member) {
    return (
      <Typography
        variant="caption"
        sx={{ pl: 4, color: 'text.disabled' }}
        component="div"
      >
        {t('probeView.member.unavailable')}
      </Typography>
    )
  }
  const level = levelOf(member.role.level)
  return (
    <Box sx={{ pl: 4, pr: 2, pb: 0.5 }}>
      <Box
        sx={{
          display: 'flex',
          alignItems: 'center',
          gap: 0.75,
          cursor: 'pointer',
        }}
        onClick={() => setOpen(!open)}
      >
        <Chip
          size="small"
          color={CHIP_COLOR[level]}
          label={pickText(member.role.text, i18n.language)}
          sx={{ height: 20, fontSize: 11 }}
        />
        <ProbeElements items={member.headline} />
      </Box>
      <Collapse in={open} unmountOnExit>
        <ProbeSections sections={member.sections} />
      </Collapse>
    </Box>
  )
})

/** What the page shows above the list in the strategy mode: the state of the view and the
 * banners of the core. */
export const ProbeViewBanner = () => {
  const { t } = useTranslation()
  const { enabled, status, data, stale, fetchedAt, error } = useProbeView()
  const outdated = !!data && data.view > PROBE_VIEW_VERSION
  const banners = data?.banners ?? []
  if (
    !enabled ||
    (status !== 'unsupported' &&
      status !== 'noGroups' &&
      !stale &&
      !outdated &&
      banners.length === 0)
  ) {
    return null
  }
  return (
    <Box sx={{ display: 'grid', gap: 0.5, mx: 1.5, mt: 1 }}>
      {status === 'unsupported' && (
        <Alert severity="info" variant="outlined">
          {t('probeView.status.unsupported')}
        </Alert>
      )}
      {status === 'noGroups' && (
        <Alert severity="info" variant="outlined">
          {t('probeView.status.noGroups')}
        </Alert>
      )}
      {stale && (
        <Alert severity="warning" variant="outlined">
          {t('probeView.status.stale', {
            time: fetchedAt ? new Date(fetchedAt).toLocaleTimeString() : '-',
            error: error ?? '',
          })}
        </Alert>
      )}
      {outdated && (
        <Alert severity="info" variant="outlined">
          {t('probeView.status.outdated')}
        </Alert>
      )}
      {banners.length > 0 && <ProbeElements items={banners} />}
    </Box>
  )
}
