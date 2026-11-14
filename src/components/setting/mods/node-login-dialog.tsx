import { VisibilityOffRounded, VisibilityRounded } from '@mui/icons-material'
import {
  Alert,
  Box,
  Button,
  Divider,
  IconButton,
  InputAdornment,
  Stack,
  TextField,
  Typography,
} from '@mui/material'
import { writeText } from '@tauri-apps/plugin-clipboard-manager'
import { useEffect, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'

import { BaseDialog } from '@/components/base'
import {
  loginNode,
  type NodeLoginOutcome,
  type RunningNode,
} from '@/services/cmds'
import { errorDetail, showNotice } from '@/services/notice-service'
import { openExternalUrl } from '@/utils/open-external-url'

import {
  type LoginTarget,
  controlServerText,
  failureText,
  isChangedFailure,
  loginSuccessText,
} from './node-login'

// The longest the core waits for a login with a key, and for a login link.
const KEY_LOGIN_SECONDS = 90
const LINK_LOGIN_SECONDS = 60

interface LoginResult {
  by: 'key' | 'link'
  authUrl: string
  note: string
  error: string
}

interface Props {
  target: LoginTarget
  /** The latest status of the target, as the list refreshes it. */
  live?: RunningNode
  onClose: () => void
  onChanged: () => void
}

export function NodeLoginDialog({ target, live, onClose, onChanged }: Props) {
  const { t } = useTranslation()
  const { node, source } = target

  const [authKey, setAuthKey] = useState('')
  const [showKey, setShowKey] = useState(false)
  const [busy, setBusy] = useState<'key' | 'link' | null>(null)
  // What the latest request returned, shown until the list tells of the node anew.
  const [result, setResult] = useState<LoginResult | null>(null)
  // A login goes on after its request, whatever the request returned: once the list tells
  // of the node anew, which it does by a new object, its status is shown instead.
  const [baseline, setBaseline] = useState<RunningNode | undefined>()
  const liveRef = useRef(live)
  useEffect(() => {
    liveRef.current = live
  }, [live])
  // A request that returns after the dialog closed must leave a dialog opened since alone.
  const openRef = useRef(true)
  useEffect(() => {
    openRef.current = true
    return () => {
      openRef.current = false
    }
  }, [])

  const following = result !== null && live !== baseline
  const liveStatus = following ? live?.status : undefined
  useEffect(() => {
    if (liveStatus?.state !== 'running') return
    showNotice.success(loginSuccessText(t, node.name, liveStatus))
    onClose()
  }, [liveStatus, node.name, onClose, t])

  const finish = (by: LoginResult['by'], shown: Partial<LoginResult>) => {
    setResult({ by, authUrl: '', note: '', error: '', ...shown })
    setBaseline(liveRef.current)
  }

  const handleOutcome = (outcome: NodeLoginOutcome, by: LoginResult['by']) => {
    if (outcome.kind === 'done') {
      const { status } = outcome.node
      if (status.state === 'running') {
        showNotice.success(loginSuccessText(t, node.name, status))
        onClose()
      } else if (status.state === 'needs_approval') {
        finish(by, { note: t('nodeLogin.login.approval') })
      } else {
        finish(by, { authUrl: status.authUrl })
      }
    } else if (outcome.code === 'timeout') {
      finish(by, { note: failureText(t, outcome) })
    } else if (isChangedFailure(outcome)) {
      showNotice.warning(failureText(t, outcome))
      onClose()
    } else {
      finish(by, { error: failureText(t, outcome) })
    }
  }

  const login = async (by: LoginResult['by']) => {
    const key = by === 'key' ? authKey.trim() : null
    if (by === 'key' && !key) return
    setBusy(by)
    setResult(null)
    if (by === 'key') setAuthKey('')
    try {
      const outcome = await loginNode(node.name, node.instance, key)
      if (openRef.current) handleOutcome(outcome, by)
    } catch (err) {
      if (openRef.current)
        finish(by, { error: errorDetail(err) || String(err) })
    } finally {
      if (openRef.current) setBusy(null)
      onChanged()
    }
  }

  const shownUrl = following
    ? liveStatus?.state === 'needs_login'
      ? liveStatus.authUrl
      : ''
    : (result?.authUrl ?? '')
  const shownNote = following
    ? liveStatus?.state === 'needs_approval'
      ? t('nodeLogin.login.approval')
      : ''
    : (result?.note ?? '')
  const keyError = !following && result?.by === 'key' ? result.error : ''
  const linkError = !following && result?.by === 'link' ? result.error : ''
  const liveError = liveStatus?.error ?? ''

  const openLink = () => {
    openExternalUrl(shownUrl).catch((err) => showNotice.error(err))
  }

  const copyLink = async () => {
    try {
      await writeText(shownUrl)
      showNotice.success(t('nodeLogin.login.copied'))
    } catch (err) {
      showNotice.error(err)
    }
  }

  const title = source
    ? t('nodeLogin.login.sharedTitle', {
        name: node.name,
        profile: source.profile,
        node: source.name,
      })
    : t('nodeLogin.login.title', { name: node.name })

  return (
    <BaseDialog
      open
      title={title}
      contentSx={{ width: { xs: 320, sm: 480 }, userSelect: 'text' }}
      cancelBtn={t('shared.actions.close')}
      disableOk
      onClose={onClose}
      onCancel={onClose}
    >
      <Stack spacing={2}>
        <Typography variant="body2" color="text.secondary">
          {t('nodeLogin.fields.controlServer')}:{' '}
          {controlServerText(t, node.config.controlUrl)}
        </Typography>

        {shownNote && <Alert severity="info">{shownNote}</Alert>}
        {liveError && (
          <Alert severity="warning" sx={{ wordBreak: 'break-all' }}>
            {t('nodeLogin.fields.error')}: {liveError}
          </Alert>
        )}

        <Box>
          <Typography variant="subtitle2" sx={{ mb: 1 }}>
            {t('nodeLogin.login.keySection')}
          </Typography>
          <Stack direction="row" spacing={1} sx={{ alignItems: 'flex-start' }}>
            <TextField
              size="small"
              fullWidth
              autoComplete="off"
              type={showKey ? 'text' : 'password'}
              label={t('nodeLogin.login.keyLabel')}
              value={authKey}
              disabled={busy !== null}
              error={!!keyError}
              helperText={keyError || t('nodeLogin.login.keyNote')}
              onChange={(e) => setAuthKey(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter') void login('key')
              }}
              slotProps={{
                input: {
                  endAdornment: (
                    <InputAdornment position="end">
                      <IconButton
                        size="small"
                        edge="end"
                        aria-label={t(
                          showKey
                            ? 'nodeLogin.login.hideKey'
                            : 'nodeLogin.login.showKey',
                        )}
                        onClick={() => setShowKey((v) => !v)}
                      >
                        {showKey ? (
                          <VisibilityOffRounded fontSize="small" />
                        ) : (
                          <VisibilityRounded fontSize="small" />
                        )}
                      </IconButton>
                    </InputAdornment>
                  ),
                },
              }}
            />
            <Button
              variant="contained"
              loading={busy === 'key'}
              disabled={busy !== null || !authKey.trim()}
              onClick={() => void login('key')}
              sx={{ flexShrink: 0, height: 40 }}
            >
              {t('nodeLogin.login.submit')}
            </Button>
          </Stack>
          {busy === 'key' && (
            <Typography variant="caption" color="text.secondary">
              {t('nodeLogin.login.waiting', { seconds: KEY_LOGIN_SECONDS })}
            </Typography>
          )}
        </Box>

        <Divider />

        <Box>
          <Typography variant="subtitle2" sx={{ mb: 1 }}>
            {t('nodeLogin.login.linkSection')}
          </Typography>
          <Button
            variant="outlined"
            loading={busy === 'link'}
            disabled={busy !== null}
            onClick={() => void login('link')}
          >
            {t('nodeLogin.login.getLink')}
          </Button>
          {busy === 'link' && (
            <Typography
              variant="caption"
              color="text.secondary"
              sx={{ display: 'block', mt: 0.5 }}
            >
              {t('nodeLogin.login.waiting', { seconds: LINK_LOGIN_SECONDS })}
            </Typography>
          )}
          {linkError && (
            <Typography variant="body2" color="error" sx={{ mt: 1 }}>
              {linkError}
            </Typography>
          )}
          {shownUrl && (
            <Box sx={{ mt: 1 }}>
              <Typography variant="body2" sx={{ wordBreak: 'break-all' }}>
                {shownUrl}
              </Typography>
              <Stack direction="row" spacing={1} sx={{ my: 1 }}>
                <Button size="small" onClick={() => void copyLink()}>
                  {t('nodeLogin.login.copyLink')}
                </Button>
                <Button size="small" onClick={openLink}>
                  {t('nodeLogin.login.openLink')}
                </Button>
              </Stack>
              <Typography variant="caption" color="text.secondary">
                {t('nodeLogin.login.linkNote')}
              </Typography>
            </Box>
          )}
        </Box>
      </Stack>
    </BaseDialog>
  )
}
