import { getConnections, MihomoWebSocket } from 'tauri-plugin-mihomo-api'
import { afterEach, beforeEach, expect, it, vi } from 'vitest'

type Subscribe = (listener: () => void) => () => void

const store = vi.hoisted(() => {
  // the hook schedules its timers on window, as in the webview
  Object.assign(globalThis, { window: globalThis })
  return {
    subscribes: [] as Subscribe[],
    getSnapshot: (() => undefined) as () => unknown,
  }
})
const subscribes = store.subscribes
vi.mock('react', () => ({
  useCallback: (callback: unknown) => callback,
  useMemo: (factory: () => unknown) => factory(),
  useSyncExternalStore: (subscribe: Subscribe, getSnapshot: () => unknown) => {
    store.subscribes.push(subscribe)
    store.getSnapshot = getSnapshot
    return getSnapshot()
  },
}))
vi.mock('tauri-plugin-mihomo-api', () => ({
  getConnections: vi.fn(),
  MihomoWebSocket: { connect_connections_count: vi.fn() },
}))

const snapshot = (upload: number) => ({
  uploadTotal: upload,
  downloadTotal: 0,
  memory: 0,
  connections: [
    {
      id: 'conn',
      metadata: { host: 'example.com' },
      upload,
      download: 0,
      start: '',
      chains: [],
      rule: '',
      rulePayload: '',
    },
  ],
})

const loadHooks = async () => {
  vi.resetModules()
  subscribes.length = 0
  return import('./use-connection-data')
}

beforeEach(() => {
  vi.useFakeTimers()
})

afterEach(() => {
  vi.useRealTimers()
})

it('pulls one snapshot at a time and stops with the last subscriber', async () => {
  const { useConnectionData } = await loadHooks()
  let finishFirst!: (value: never) => void
  vi.mocked(getConnections)
    .mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finishFirst = resolve
        }) as ReturnType<typeof getConnections>,
    )
    .mockResolvedValue(snapshot(0) as never)

  useConnectionData()
  const unsubscribe = subscribes[0](() => {})
  expect(getConnections).toHaveBeenCalledTimes(1)

  // a slow request is not overlapped by the next one
  await vi.advanceTimersByTimeAsync(5_000)
  expect(getConnections).toHaveBeenCalledTimes(1)

  finishFirst(snapshot(0) as never)
  await vi.advanceTimersByTimeAsync(1_000)
  expect(getConnections).toHaveBeenCalledTimes(2)

  unsubscribe()
  await vi.advanceTimersByTimeAsync(5_000)
  expect(getConnections).toHaveBeenCalledTimes(2)
})

it('waits for a running request when subscribed again', async () => {
  const { useConnectionData } = await loadHooks()
  let finishFirst!: (value: never) => void
  vi.mocked(getConnections)
    .mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          finishFirst = resolve
        }) as ReturnType<typeof getConnections>,
    )
    .mockResolvedValue(snapshot(5) as never)

  useConnectionData()
  subscribes[0](() => {})()
  subscribes[0](() => {})
  expect(getConnections).toHaveBeenCalledTimes(1)

  finishFirst(snapshot(1) as never)
  await vi.advanceTimersByTimeAsync(0)
  // the result of the stopped subscription is dropped, the new one polls right away
  expect(getConnections).toHaveBeenCalledTimes(2)
  const data = store.getSnapshot() as { activeConnections: IConnectionsItem[] }
  expect(data.activeConnections[0]?.upload).toBe(5)
})

it('measures rates per second and restarts them after a pause', async () => {
  const { useConnectionData } = await loadHooks()
  const latest = () =>
    (
      store.getSnapshot() as {
        activeConnections: IConnectionsItem[]
      }
    ).activeConnections[0]?.curUpload
  vi.mocked(getConnections)
    .mockResolvedValueOnce(snapshot(0) as never)
    .mockResolvedValueOnce(snapshot(2_000) as never)
    .mockResolvedValue(snapshot(62_000) as never)

  useConnectionData()
  const unsubscribe = subscribes[0](() => {})
  await vi.advanceTimersByTimeAsync(0)
  expect(latest()).toBe(0)
  await vi.advanceTimersByTimeAsync(1_000)
  expect(latest()).toBe(2_000)

  unsubscribe()
  await vi.advanceTimersByTimeAsync(60_000)
  subscribes[0](() => {})
  await vi.advanceTimersByTimeAsync(0)
  // the bytes of the whole pause are not shown as one second's worth
  expect(latest()).toBe(0)
})

it('reconnects the count stream when subscribed again while closing an attempt', async () => {
  const { useConnectionSummaryData } = await loadHooks()
  let connected!: (socket: MihomoWebSocket) => void
  let closed!: () => void
  const socket = {
    addListener: vi.fn(),
    close: vi.fn(
      () =>
        new Promise<void>((resolve) => {
          closed = resolve
        }),
    ),
  }
  vi.mocked(MihomoWebSocket.connect_connections_count).mockImplementation(
    () =>
      new Promise((resolve) => {
        connected = resolve
      }) as ReturnType<typeof MihomoWebSocket.connect_connections_count>,
  )

  useConnectionSummaryData()
  const unsubscribe = subscribes[0](() => {})
  unsubscribe()
  connected(socket as unknown as MihomoWebSocket)
  await vi.advanceTimersByTimeAsync(0)
  expect(socket.close).toHaveBeenCalled()

  subscribes[0](() => {})
  closed()
  await vi.advanceTimersByTimeAsync(1_000)
  expect(MihomoWebSocket.connect_connections_count).toHaveBeenCalledTimes(2)
})
