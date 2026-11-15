import { expect, it, vi } from 'vitest'

vi.mock('react', () => ({
  useCallback: (callback: unknown) => callback,
  useSyncExternalStore: (subscribe: unknown, read: unknown) => ({
    subscribe,
    read,
  }),
}))
vi.mock('tauri-plugin-mihomo-api', () => ({
  delayProxyByName: vi.fn(async () => ({ delay: 120 })),
  healthcheckNodeInProvider: vi.fn(async () => ({ delay: 120 })),
}))

import delayManager, { type DelaySnapshot } from '@/services/delay'
import type { InteractableProxyMember } from '@/types/proxy-view'

import { useGroupsDelays } from './use-group-delays'

interface Store {
  subscribe: (onSettle: () => void) => () => void
  read: () => ReadonlyMap<string, DelaySnapshot>
}

const node = (name: string) =>
  ({
    kind: 'node',
    ref: { kind: 'node', name, recordId: `r:${name}` },
    node: {
      recordId: `r:${name}`,
      name,
      history: [],
      source: { kind: 'core', proxyName: name },
    },
  }) as unknown as InteractableProxyMember

it('follows groups whose names contain spaces', async () => {
  const store = useGroupsDelays(['Auto Select', 'Fallback']) as unknown as Store
  const onSettle = vi.fn()
  const stop = store.subscribe(onSettle)

  expect([...store.read().keys()]).toEqual(['Auto Select', 'Fallback'])
  await delayManager.checkDelay(node('a'), 'Auto Select', 5000)
  await new Promise((resolve) => setTimeout(resolve, 0))

  expect(onSettle).toHaveBeenCalledOnce()
  stop()
})
