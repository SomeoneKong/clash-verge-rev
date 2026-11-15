import { createContext, use } from 'react'

import type { ProbeViewData, ProbeViewGroup } from '@/types/probe-view'

export type ProbeViewStatus =
  | 'off'
  | 'loading'
  | 'ok'
  | 'noGroups'
  | 'unsupported'
  | 'failed'

export interface ProbeViewState {
  enabled: boolean
  setEnabled: (enabled: boolean) => void
  status: ProbeViewStatus
  data: ProbeViewData | null
  /** When the view shown was read; it is out of date once a read failed after it. */
  fetchedAt: number | null
  stale: boolean
  error: string | null
  groups: Map<string, ProbeViewGroup> | null
  refresh: () => void
  /** The strategy history, open or not, with the group it was opened for. */
  history: { open: boolean; group?: string; token: number }
  openHistory: (group?: string) => void
  closeHistory: () => void
}

export const ProbeViewContext = createContext<ProbeViewState | null>(null)

const OFF: ProbeViewState = {
  enabled: false,
  setEnabled: () => {},
  status: 'off',
  data: null,
  fetchedAt: null,
  stale: false,
  error: null,
  groups: null,
  refresh: () => {},
  history: { open: false, token: 0 },
  openHistory: () => {},
  closeHistory: () => {},
}

export const useProbeView = () => use(ProbeViewContext) ?? OFF
