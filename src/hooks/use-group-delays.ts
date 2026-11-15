import { useCallback, useSyncExternalStore } from 'react'

import delayManager, { type DelaySnapshot } from '@/services/delay'

const NO_DELAYS: DelaySnapshot = { of: () => -1 }

/** Exposes the external delay store to React, updating sort order only after a test settles. */
export const useGroupDelays = (group: string | null): DelaySnapshot => {
  const subscribe = useCallback(
    (onSettle: () => void) =>
      group ? delayManager.addGroupListener(group, onSettle) : () => {},
    [group],
  )
  const read = useCallback(
    () => (group ? delayManager.groupDelays(group) : NO_DELAYS),
    [group],
  )

  return useSyncExternalStore(subscribe, read, read)
}

/** Preserves unchanged group snapshot identities when another group settles. */
export const useGroupsDelays = (
  groups: readonly string[],
): ReadonlyMap<string, DelaySnapshot> => {
  // Subscribe by group membership; callers rebuild the array on every render.
  // Group names may contain any character, so the key is the JSON-encoded list.
  const groupKey = JSON.stringify(groups)

  const subscribe = useCallback(
    (onSettle: () => void) => {
      const names: string[] = JSON.parse(groupKey)
      const unsubscribes = names.map((name) =>
        delayManager.addGroupListener(name, onSettle),
      )
      return () => {
        for (const unsubscribe of unsubscribes) unsubscribe()
      }
    },
    [groupKey],
  )

  const read = useCallback(
    () => delayManager.groupsDelays(groupKey),
    [groupKey],
  )

  return useSyncExternalStore(subscribe, read, read)
}
