import {
  Alert,
  Box,
  Button,
  Chip,
  List,
  ListItem,
  ListSubheader,
  Stack,
  Typography,
} from '@mui/material'
import dayjs from 'dayjs'
import { Ref, useImperativeHandle, useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { BaseDialog, BaseEmpty, DialogRef } from '@/components/base'
import { useProfiles } from '@/hooks/use-profiles'
import {
  getNodeLogins,
  getProfileLoginNodes,
  logoutNode,
  type NodeLogins,
  type NodeRow,
  type NodeState,
  type RunningNode,
} from '@/services/cmds'
import { errorDetail, showNotice } from '@/services/notice-service'
import { revalidateQuery, useQuery } from '@/services/query-client'

import {
  ATTENTION_QUERY_KEY,
  type LoginTarget,
  controlServerText,
  failureText,
  isChangedFailure,
} from './node-login'
import { NodeLoginDialog } from './node-login-dialog'

type Translate = ReturnType<typeof useTranslation>['t']

const LOGOUT_STATES: NodeState[] = [
  'needs_login',
  'starting',
  'needs_approval',
  'running',
  'stopped',
]

const stateColor = (state: NodeState) => {
  switch (state) {
    case 'running':
      return 'success'
    case 'needs_login':
    case 'needs_approval':
      return 'warning'
    case 'failed':
      return 'error'
    default:
      return 'default'
  }
}

const findRunning = (logins: NodeLogins | undefined, instance: string) => {
  for (const profile of logins?.profiles ?? []) {
    for (const row of profile.nodes) {
      if (row.target?.instance === instance) return row.target
    }
  }
  return undefined
}

/** The settings of a node of another profile that differ from the node it shares with. */
const pendingSettings = (t: Translate, row: NodeRow, target: RunningNode) => {
  const settings: string[] = []
  if (row.config.controlUrl !== target.config.controlUrl) {
    settings.push(
      t('nodeLogin.settings.controlServer', {
        url: controlServerText(t, row.config.controlUrl),
      }),
    )
  }
  if (row.config.ephemeral !== target.config.ephemeral) {
    settings.push(
      t(
        row.config.ephemeral
          ? 'nodeLogin.settings.ephemeral'
          : 'nodeLogin.settings.notEphemeral',
      ),
    )
  }
  if (row.config.configAuthKey !== target.config.configAuthKey) {
    settings.push(
      t(
        row.config.configAuthKey
          ? 'nodeLogin.settings.authKey'
          : 'nodeLogin.settings.noAuthKey',
      ),
    )
  }
  return settings
}

interface RowProps {
  row: NodeRow
  current: boolean
  supported: boolean
  profileName: string
  onLogin: (target: LoginTarget) => void
  onLogout: (target: LoginTarget) => void
}

function NodeRowView({
  row,
  current,
  supported,
  profileName,
  onLogin,
  onLogout,
}: RowProps) {
  const { t } = useTranslation()
  const target = row.target
  const stateLabels: Record<NodeState, string> = {
    not_started: t('nodeLogin.states.notStarted'),
    starting: t('nodeLogin.states.starting'),
    needs_login: t('nodeLogin.states.needsLogin'),
    needs_approval: t('nodeLogin.states.needsApproval'),
    running: t('nodeLogin.states.running'),
    stopped: t('nodeLogin.states.stopped'),
    failed: t('nodeLogin.states.failed'),
    unknown: t('nodeLogin.states.unknown'),
  }

  const details: string[] = []
  const notes: string[] = []
  let error = ''
  if (target) {
    const { status } = target
    if (status.device)
      details.push(`${t('nodeLogin.fields.device')}: ${status.device}`)
    if (status.user)
      details.push(`${t('nodeLogin.fields.user')}: ${status.user}`)
    if (status.addresses.length)
      details.push(
        `${t('nodeLogin.fields.addresses')}: ${status.addresses.join(', ')}`,
      )
    if (status.keyExpiry)
      details.push(
        `${t('nodeLogin.fields.keyExpiry')}: ${dayjs(status.keyExpiry).format('YYYY-MM-DD HH:mm')}`,
      )
    error = status.error

    if (!current) {
      notes.push(t('nodeLogin.notes.shared', { name: target.name }))
      const settings = pendingSettings(t, row, target)
      if (settings.length)
        notes.push(
          t('nodeLogin.notes.pendingSettings', {
            settings: settings.join(', '),
          }),
        )
      if (row.config.controlUrl !== target.config.controlUrl)
        notes.push(t('nodeLogin.notes.controlServerDiffers'))
    }
    if (status.state === 'needs_login' && status.identity)
      notes.push(t('nodeLogin.notes.keyExpired'))
    if (status.state === 'needs_approval')
      notes.push(t('nodeLogin.notes.needsApproval'))
    if (status.state === 'failed') notes.push(t('nodeLogin.notes.failed'))
  } else if (supported && !current) {
    if (row.invalid) notes.push(t('nodeLogin.notes.invalid'))
    else if (row.conflicts.length)
      notes.push(
        t('nodeLogin.notes.conflict', { names: row.conflicts.join(', ') }),
      )
    else notes.push(t('nodeLogin.notes.inactive'))
  }
  const controlUrl = target ? target.config.controlUrl : row.config.controlUrl
  details.push(
    `${t('nodeLogin.fields.controlServer')}: ${controlServerText(t, controlUrl)}`,
  )

  const loginTarget: LoginTarget | undefined = target
    ? {
        node: target,
        source: current ? undefined : { profile: profileName, name: row.name },
      }
    : undefined
  const state = target?.status.state
  const canLogin = state === 'not_started' || state === 'needs_login'
  const canLogout =
    !!target?.status.identity && !!state && LOGOUT_STATES.includes(state)

  return (
    <ListItem divider sx={{ display: 'block', py: 1 }}>
      <Stack direction="row" spacing={1} sx={{ alignItems: 'center' }}>
        <Typography sx={{ fontWeight: 600, wordBreak: 'break-all' }}>
          {row.name}
        </Typography>
        {state && (
          <Chip
            size="small"
            variant="outlined"
            color={stateColor(state)}
            label={stateLabels[state]}
          />
        )}
        <Box sx={{ flex: 1 }} />
        {loginTarget && canLogin && (
          <Button size="small" onClick={() => onLogin(loginTarget)}>
            {t('nodeLogin.actions.login')}
          </Button>
        )}
        {loginTarget && canLogout && (
          <Button
            size="small"
            color="warning"
            onClick={() => onLogout(loginTarget)}
          >
            {t('nodeLogin.actions.logout')}
          </Button>
        )}
      </Stack>
      <Typography
        variant="body2"
        color="text.secondary"
        sx={{ wordBreak: 'break-all' }}
      >
        {details.join(' · ')}
      </Typography>
      {error && (
        <Typography
          variant="body2"
          color="error"
          sx={{ wordBreak: 'break-all' }}
        >
          {t('nodeLogin.fields.error')}: {error}
        </Typography>
      )}
      {notes.map((note) => (
        <Typography key={note} variant="body2" color="text.secondary">
          {note}
        </Typography>
      ))}
    </ListItem>
  )
}

export function NodeLoginViewer({ ref }: { ref?: Ref<DialogRef> }) {
  const { t } = useTranslation()
  const [open, setOpen] = useState(false)
  const [loginTarget, setLoginTarget] = useState<LoginTarget | null>(null)
  const [logoutTarget, setLogoutTarget] = useState<LoginTarget | null>(null)
  const [loggingOut, setLoggingOut] = useState(false)

  useImperativeHandle(ref, () => ({
    open: () => setOpen(true),
    close: () => setOpen(false),
  }))

  const { profiles } = useProfiles()
  const profilesKey = useMemo(
    () =>
      JSON.stringify(
        profiles?.items?.map((item) => [
          item.uid,
          item.name,
          item.file,
          item.updated,
          item.option?.proxies,
        ]),
      ),
    [profiles],
  )
  const { data: configNodes } = useQuery({
    queryKey: ['getProfileLoginNodes', profilesKey],
    queryFn: getProfileLoginNodes,
    enabled: open,
    refetchOnWindowFocus: false,
  })
  const nodesKey = useMemo(() => JSON.stringify(configNodes), [configNodes])
  const {
    data: logins,
    error,
    refetch,
  } = useQuery({
    queryKey: ['getNodeLogins', nodesKey],
    // stamped, so that every fetch gives new objects, which the login dialog takes for news
    // of its node even when nothing changed
    queryFn: async () => ({
      ...(await getNodeLogins(configNodes ?? [])),
      fetchedAt: Date.now(),
    }),
    enabled: open && configNodes !== undefined,
    refetchInterval: 3000,
    refetchOnWindowFocus: false,
    // polling pauses while the last fetch failed, so retries carry on until the core is back
    retry: Infinity,
    retryDelay: 3000,
  })

  const refresh = () => {
    void refetch()
  }

  const close = () => {
    setOpen(false)
    void revalidateQuery(ATTENTION_QUERY_KEY)
  }

  const closeLogout = () => {
    if (!loggingOut) setLogoutTarget(null)
  }

  const logout = async () => {
    if (!logoutTarget) return
    const { node } = logoutTarget
    setLoggingOut(true)
    try {
      const outcome = await logoutNode(node.name, node.instance)
      if (outcome.kind === 'done') {
        showNotice.success(t('nodeLogin.logout.success', { name: node.name }))
      } else if (outcome.code === 'timeout' || isChangedFailure(outcome)) {
        showNotice.warning(failureText(t, outcome))
      } else {
        showNotice.error(failureText(t, outcome))
      }
    } catch (err) {
      showNotice.error(err)
    } finally {
      setLoggingOut(false)
      setLogoutTarget(null)
      refresh()
    }
  }

  const sections = logins?.profiles ?? []

  return (
    <>
      <BaseDialog
        open={open}
        title={t('nodeLogin.title')}
        contentSx={{
          width: { xs: 340, sm: 560 },
          height: 460,
          overflowY: 'auto',
          userSelect: 'text',
        }}
        cancelBtn={t('shared.actions.close')}
        disableOk
        onClose={close}
        onCancel={close}
      >
        {error && (
          <Alert severity="error" sx={{ mb: 1 }}>
            {t('nodeLogin.loadFailed', {
              error: errorDetail(error) || String(error),
            })}
          </Alert>
        )}
        {logins && !logins.supported && (
          <Alert severity="warning" sx={{ mb: 1 }}>
            {t('nodeLogin.unsupported')}
          </Alert>
        )}
        {logins && sections.length === 0 && (
          <BaseEmpty
            extra={
              <Typography sx={{ mt: 2, fontSize: '12px' }}>
                {t('nodeLogin.empty')}
              </Typography>
            }
          />
        )}
        {sections.map((profile) => (
          <List
            key={profile.uid}
            dense
            subheader={
              <ListSubheader
                disableSticky
                sx={{ px: 0, display: 'flex', alignItems: 'center', gap: 1 }}
              >
                {profile.name || profile.uid}
                {profile.current && (
                  <Chip
                    size="small"
                    color="primary"
                    label={t('nodeLogin.currentProfile')}
                  />
                )}
              </ListSubheader>
            }
          >
            {profile.nodes.map((row) => (
              <NodeRowView
                key={row.name}
                row={row}
                current={profile.current}
                supported={logins?.supported ?? false}
                profileName={profile.name || profile.uid}
                onLogin={setLoginTarget}
                onLogout={setLogoutTarget}
              />
            ))}
          </List>
        ))}
      </BaseDialog>

      {loginTarget && (
        <NodeLoginDialog
          key={loginTarget.node.instance}
          target={loginTarget}
          live={findRunning(logins, loginTarget.node.instance)}
          onClose={() => {
            setLoginTarget(null)
            refresh()
          }}
          onChanged={refresh}
        />
      )}

      <BaseDialog
        open={logoutTarget !== null}
        title={t('nodeLogin.logout.title')}
        okBtn={t('nodeLogin.actions.logout')}
        cancelBtn={t('shared.actions.cancel')}
        contentSx={{ width: { xs: 320, sm: 420 } }}
        loading={loggingOut}
        disableCancel={loggingOut}
        onOk={() => void logout()}
        onCancel={closeLogout}
        onClose={closeLogout}
      >
        {logoutTarget && (
          <Stack spacing={1}>
            <Typography>
              {t('nodeLogin.logout.confirm', { name: logoutTarget.node.name })}
            </Typography>
            {logoutTarget.source && (
              <Typography variant="body2" color="text.secondary">
                {t('nodeLogin.logout.sharedTarget', {
                  name: logoutTarget.node.name,
                  profile: logoutTarget.source.profile,
                  node: logoutTarget.source.name,
                })}
              </Typography>
            )}
            <Typography variant="body2" color="text.secondary">
              {t('nodeLogin.logout.effect')}
            </Typography>
            {logoutTarget.node.config.ephemeral && (
              <Typography variant="body2" color="text.secondary">
                {t('nodeLogin.logout.ephemeral')}
              </Typography>
            )}
            {logoutTarget.node.config.configAuthKey && (
              <Typography variant="body2" color="text.secondary">
                {t('nodeLogin.logout.configAuthKey')}
              </Typography>
            )}
          </Stack>
        )}
      </BaseDialog>
    </>
  )
}
