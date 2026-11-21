import type { Update } from '@tauri-apps/plugin-updater'
import { useState } from 'react'
import useSWRInfinite from 'swr/infinite'

import type { HistoryEntry } from '@/services/bindings'
import { forkUpdateHistory, forkUpdateOverview } from '@/services/cmds'
import { useQuery } from '@/services/query-client'
import type { useUpdateState } from '@/services/states'

import { useUpdate } from './use-update'

export const useUpdateOverview = (
  release: Record<string, unknown> | null = null,
  enabled = true,
) =>
  useQuery({
    queryKey: ['fork-update-overview', release],
    queryFn: () => forkUpdateOverview(release),
    enabled,
    refetchInterval: 60_000,
    refetchOnWindowFocus: true,
    revalidateOnMount: true,
  })

export const displayVersion = (value: string) => `v${value.replace(/^v/i, '')}`

export function historyPageKey(
  index: number,
  previous: Pick<HistoryEntry, 'seq'>[] | null,
) {
  if (index === 0) return ['fork-update-history', null] as const
  const cursor = previous?.at(-1)?.seq
  return cursor === undefined
    ? null
    : (['fork-update-history', cursor] as const)
}

export const useUpdateHistory = () =>
  useSWRInfinite<HistoryEntry[]>(
    historyPageKey,
    ([, before]: readonly [string, string | null]) => forkUpdateHistory(before),
    {
      revalidateFirstPage: false,
      revalidateOnFocus: false,
      revalidateOnReconnect: false,
      shouldRetryOnError: false,
    },
  )

export interface UpdateDialogRef {
  open: (release?: Update) => void
  close: () => void
}

export function useDialogRelease(
  open: boolean,
  updateState: ReturnType<typeof useUpdateState>,
) {
  const { updateInfo: cached } = useUpdate()
  const [manual, setManual] = useState<Update>()
  const updateInfo = manual ?? cached
  const overview = useUpdateOverview(
    updateInfo?.rawJson ?? null,
    open && !!updateInfo,
  )
  const fallbackReason =
    updateState?.version === updateInfo?.version
      ? updateState?.fallbackReason
      : undefined
  const next = overview.data?.next
  return {
    updateInfo,
    setManual,
    overview,
    status: { next, fallbackReason },
    installDisabled: !next || next.kind === 'busy' || next.kind === 'unknown',
  }
}
