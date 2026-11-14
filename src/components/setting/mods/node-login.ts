import { useTranslation } from 'react-i18next'

import {
  getNodeLogins,
  type NodeLoginOutcome,
  type NodeState,
  type RunningNode,
} from '@/services/cmds'
import { useQuery } from '@/services/query-client'

type Translate = ReturnType<typeof useTranslation>['t']

type LoginFailure = Extract<NodeLoginOutcome, { kind: 'failed' }>

export interface LoginTarget {
  /** The node of the running config that the login or logout acts on. */
  node: RunningNode
  /** The node of another profile that shares its device identity, when opened from one. */
  source?: { profile: string; name: string }
}

export const ATTENTION_QUERY_KEY = ['nodeLoginAttention'] as const
const ATTENTION_STATES: NodeState[] = [
  'needs_login',
  'needs_approval',
  'failed',
]

/** How many nodes of the running config need the user to log them in or fix them. */
export const useNodeLoginAttention = () => {
  const { data } = useQuery({
    queryKey: ATTENTION_QUERY_KEY,
    queryFn: () => getNodeLogins([]),
    refetchOnWindowFocus: false,
    retry: false,
  })
  return (
    data?.profiles
      .find((profile) => profile.current)
      ?.nodes.filter(
        (row) =>
          row.target && ATTENTION_STATES.includes(row.target.status.state),
      ).length ?? 0
  )
}

export const isChangedFailure = (failure: LoginFailure) =>
  failure.code === 'replaced' || failure.code === 'superseded'

export const failureText = (t: Translate, failure: LoginFailure) => {
  switch (failure.code) {
    case 'timeout':
      return t('nodeLogin.errors.timeout')
    case 'replaced':
    case 'superseded':
      return t('nodeLogin.errors.changed')
    case 'logged_in':
      return t('nodeLogin.errors.loggedIn')
    case 'rejected':
      return t('nodeLogin.errors.rejected', { message: failure.message })
    case 'start_failed':
      return t('nodeLogin.errors.startFailed', { message: failure.message })
    default:
      return failure.message || failure.code
  }
}

export const controlServerText = (t: Translate, url: string) =>
  url || t('nodeLogin.officialServer')

export const loginSuccessText = (
  t: Translate,
  name: string,
  status: RunningNode['status'],
) =>
  t('nodeLogin.login.success', {
    name,
    device: status.device,
    addresses: status.addresses.join(', '),
  })
