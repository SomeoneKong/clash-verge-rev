import { useCallback, useMemo, useSyncExternalStore } from 'react'
import {
  type ClosedConnection,
  type ClosedConnections,
  getClosedConnections,
  getConnections,
  MihomoWebSocket,
} from 'tauri-plugin-mihomo-api'

const MAX_CLOSED_CONNS_NUM = 500
// the closed connections the core keeps, which are kept here alike
const CLOSED_KEEP_MS = 31 * 60_000
const CLOSED_KEEP_MAX = 10_000
const CLOSED_PAGE_SIZE = 1_000
const CLOSED_PAGES_PER_POLL = 5
const CONNECTION_POLL_INTERVAL_MS = 1_000
const CONNECTION_RECONNECT_DELAY_MS = 1_000

type ConnectionMetadata = IConnectionsItem['metadata']
type ConnectionListener = () => void

const metadataValue = (value?: string) => value || ''

/** What the closed connections read from the core cover. */
export interface ClosedFeed {
  /** The core keeps closed connections; null until it answered. */
  supported: boolean | null
  /** Every record the core had when the last poll began is read. */
  caughtUp: boolean
  /** Connections closed at or before this time (ms) may be missing; 0 when none. */
  missingUntil: number
  /** When the core read from started (ms): earlier closes are of the core before it. */
  startedAt: number
}

const initClosedFeed: ClosedFeed = {
  supported: null,
  caughtUp: false,
  missingUntil: 0,
  startedAt: 0,
}

const unsupportedClosedFeed: ClosedFeed = {
  ...initClosedFeed,
  supported: false,
}

const initConnData: ConnectionMonitorData = {
  uploadTotal: 0,
  downloadTotal: 0,
  activeConnections: [],
  closedConnections: [],
  recordedClosed: [],
  closedFeed: initClosedFeed,
  polledAt: 0,
}

interface ConnectionMonitorData {
  uploadTotal: number
  downloadTotal: number
  activeConnections: IConnectionsItem[]
  closedConnections: IConnectionsItem[]
  /** Every closed connection kept, those cleared from the list included. */
  recordedClosed: IConnectionsItem[]
  closedFeed: ClosedFeed
  /** When the snapshot was taken (ms). */
  polledAt: number
}

interface ConnectionSummaryPayload {
  count?: number
}

interface ConnectionSummaryData {
  activeConnectionCount: number
}

const initConnSummaryData: ConnectionSummaryData = {
  activeConnectionCount: 0,
}

let connectionData: ConnectionMonitorData = initConnData
let connectionSummary: ConnectionSummaryData = initConnSummaryData
// when the snapshot that rates are measured from was taken, 0 once polling stops
let lastSnapshotAt = 0

const connectionListeners = new Set<ConnectionListener>()
const summaryListeners = new Set<ConnectionListener>()

const notifyConnectionListeners = () => {
  connectionListeners.forEach((listener) => listener())
}

const notifySummaryListeners = () => {
  summaryListeners.forEach((listener) => listener())
}

const sameMetadata = (left: ConnectionMetadata, right: ConnectionMetadata) =>
  metadataValue(left.network) === metadataValue(right.network) &&
  metadataValue(left.type) === metadataValue(right.type) &&
  metadataValue(left.host) === metadataValue(right.host) &&
  metadataValue(left.sourceIP) === metadataValue(right.sourceIP) &&
  metadataValue(left.sourcePort) === metadataValue(right.sourcePort) &&
  metadataValue(left.destinationPort) ===
    metadataValue(right.destinationPort) &&
  metadataValue(left.destinationIP) === metadataValue(right.destinationIP) &&
  metadataValue(left.remoteDestination) ===
    metadataValue(right.remoteDestination) &&
  metadataValue(left.process) === metadataValue(right.process) &&
  metadataValue(left.processPath) === metadataValue(right.processPath)

const normalizeMetadata = (
  metadata: ConnectionMetadata,
  previous?: ConnectionMetadata,
): ConnectionMetadata => {
  if (previous && sameMetadata(previous, metadata)) return previous

  return {
    network: metadata.network || '',
    type: metadata.type || '',
    host: metadata.host || '',
    sourceIP: metadata.sourceIP || '',
    sourcePort: metadata.sourcePort || '',
    destinationPort: metadata.destinationPort || '',
    destinationIP: metadata.destinationIP || '',
    remoteDestination: metadata.remoteDestination || '',
    process: metadata.process || '',
    processPath: metadata.processPath || '',
  }
}

const sameChains = (left: string[], right: string[]) => {
  if (left.length !== right.length) return false
  for (let i = 0; i < left.length; i++) {
    if (left[i] !== right[i]) return false
  }
  return true
}

const normalizeChains = (chains: string[], previous?: string[]) => {
  if (previous && sameChains(previous, chains)) return previous
  return chains.slice()
}

const normalizeDialerChains = (
  layers: string[][] | undefined,
  previous?: string[][],
) => {
  if (!layers?.length) return undefined
  if (
    previous &&
    previous.length === layers.length &&
    previous.every((chain, i) => sameChains(chain, layers[i]))
  ) {
    return previous
  }
  return layers.map((chain) => chain.slice())
}

const bytesPerSecond = (bytes: number, elapsedMs: number) =>
  elapsedMs > 0 ? Math.round((bytes * 1000) / elapsedMs) : 0

const normalizeConnection = (
  connection: IConnectionsItem,
  previous: IConnectionsItem | undefined,
  elapsedMs: number,
): IConnectionsItem => {
  const metadata = normalizeMetadata(connection.metadata, previous?.metadata)
  const chains = normalizeChains(connection.chains || [], previous?.chains)
  const dialerChains = normalizeDialerChains(
    connection.dialerChains,
    previous?.dialerChains,
  )
  const upload = connection.upload ?? 0
  const download = connection.download ?? 0
  const curUpload = previous
    ? bytesPerSecond(upload - previous.upload, elapsedMs)
    : 0
  const curDownload = previous
    ? bytesPerSecond(download - previous.download, elapsedMs)
    : 0
  const rule = connection.rule || ''
  const rulePayload = connection.rulePayload || ''
  const start = connection.start || ''

  if (
    previous &&
    previous.metadata === metadata &&
    previous.chains === chains &&
    previous.dialerChains === dialerChains &&
    previous.upload === upload &&
    previous.download === download &&
    previous.curUpload === curUpload &&
    previous.curDownload === curDownload &&
    previous.rule === rule &&
    previous.rulePayload === rulePayload &&
    previous.start === start
  ) {
    return previous
  }

  return {
    id: connection.id,
    metadata,
    upload,
    download,
    start,
    chains,
    dialerChains,
    rule,
    rulePayload,
    curUpload,
    curDownload,
  }
}

const mergeConnectionSnapshot = (
  payload: IConnections,
  previous: ConnectionMonitorData,
  elapsedMs: number,
  now: number,
): ConnectionMonitorData => {
  const nextConnections = payload.connections ?? []
  const previousActive = previous.activeConnections ?? []
  // what clearing the list hides is still kept for the recent statistics
  const previousClosed = previous.recordedClosed ?? []
  const previousActiveById = new Map<string, IConnectionsItem>()

  for (let i = 0; i < previousActive.length; i++) {
    const previousConnection = previousActive[i]
    previousActiveById.set(previousConnection.id, previousConnection)
  }

  const activeConnections: IConnectionsItem[] = []
  for (let i = 0; i < nextConnections.length; i++) {
    const connection = nextConnections[i]
    const previousConnection = previousActiveById.get(connection.id)
    if (previousConnection) previousActiveById.delete(connection.id)
    activeConnections.push(
      normalizeConnection(connection, previousConnection, elapsedMs),
    )
  }

  if (previousActiveById.size === 0) {
    return {
      ...previous,
      uploadTotal: payload.uploadTotal ?? 0,
      downloadTotal: payload.downloadTotal ?? 0,
      activeConnections,
      closedConnections: previousClosed,
      recordedClosed: previousClosed,
    }
  }

  const removedConnectionCount = previousActiveById.size
  const dropFromClosed = Math.max(
    0,
    previousClosed.length + removedConnectionCount - MAX_CLOSED_CONNS_NUM,
  )
  const closedConnections =
    dropFromClosed >= previousClosed.length
      ? []
      : previousClosed.slice(dropFromClosed)

  const keepFromRemoved = MAX_CLOSED_CONNS_NUM - closedConnections.length
  let skipRemoved = Math.max(0, removedConnectionCount - keepFromRemoved)
  const closedAt = new Date(now).toISOString()

  for (let i = 0; i < previousActive.length; i++) {
    const connection = previousActive[i]
    if (!previousActiveById.has(connection.id)) continue
    if (skipRemoved > 0) {
      skipRemoved -= 1
      continue
    }
    closedConnections.push({ ...connection, closedAt })
  }

  return {
    ...previous,
    uploadTotal: payload.uploadTotal ?? 0,
    downloadTotal: payload.downloadTotal ?? 0,
    activeConnections,
    closedConnections,
    recordedClosed: closedConnections,
  }
}

// The closed connections the core records, read by sequence number. A record read again,
// as traffic counted after the close renumbers it, replaces the one read before.
interface ClosedState {
  instance: string
  startedAt: number
  seq: number
  caughtUp: boolean
  coreEvictedUntil: number
  localEvictedUntil: number
  records: Map<string, IConnectionsItem>
  // the sequence number each record was read with
  seqs: Map<string, number>
  // the records as numbered when the list was cleared: not listed until read again
  cleared: Map<string, number>
}

const initClosedState = (): ClosedState => ({
  instance: '',
  startedAt: 0,
  seq: 0,
  caughtUp: false,
  coreEvictedUntil: 0,
  localEvictedUntil: 0,
  records: new Map(),
  seqs: new Map(),
  cleared: new Map(),
})

let closedState = initClosedState()
let closedSupported: boolean | null = null
// connections the snapshots found closed before this time (ms) are not listed
let snapshotClearedAt = 0

const closedTime = (item: IConnectionsItem) =>
  item.closedAt ? Date.parse(item.closedAt) : 0

const closedItem = (
  record: ClosedConnection,
  previous: IConnectionsItem | undefined,
): IConnectionsItem => {
  // the plugin serializes its enums, typed as unions, as plain strings
  const connection = record as unknown as IConnectionsItem
  if (
    previous &&
    previous.closedAt === record.closedAt &&
    previous.upload === record.upload &&
    previous.download === record.download
  ) {
    return previous
  }
  return {
    ...normalizeConnection(connection, previous, 0),
    curUpload: 0,
    curDownload: 0,
    closedAt: record.closedAt,
  }
}

const mergeClosedPage = (page: ClosedConnections) => {
  for (const record of page.records) {
    const key = `${page.instance}/${record.id}`
    closedState.records.set(
      key,
      closedItem(record, closedState.records.get(key)),
    )
    closedState.seqs.set(key, record.seq)
    closedState.seq = Math.max(closedState.seq, record.seq)
  }
  closedState.coreEvictedUntil = page.evictedUntil
    ? Date.parse(page.evictedUntil)
    : 0
}

const pruneClosed = (now: number) => {
  const { records, seqs, cleared } = closedState
  const drop = (key: string) => {
    records.delete(key)
    seqs.delete(key)
    cleared.delete(key)
  }
  for (const [key, item] of records) {
    if (now - closedTime(item) > CLOSED_KEEP_MS) drop(key)
  }
  if (records.size <= CLOSED_KEEP_MAX) return
  const oldest = [...records.entries()].sort(
    (a, b) => closedTime(a[1]) - closedTime(b[1]),
  )
  for (const [key, item] of oldest.slice(0, records.size - CLOSED_KEEP_MAX)) {
    closedState.localEvictedUntil = Math.max(
      closedState.localEvictedUntil,
      closedTime(item),
    )
    drop(key)
  }
}

/**
 * Reads the records closed since the last one read, up to the latest the core had at the
 * first page, a few pages per poll. Returns false when the poll is to be dropped: the core
 * changed between the requests. Sets the core's support on the first answer.
 */
const readClosed = async (activeInstance: string | null | undefined) => {
  try {
    return await readClosedPages(activeInstance)
  } finally {
    // whatever failed, what is kept no longer is not shown
    pruneClosed(Date.now())
  }
}

const readClosedPages = async (activeInstance: string | null | undefined) => {
  let page = await getClosedConnections(closedState.seq, CLOSED_PAGE_SIZE)
  if (page === null) {
    // the list goes on from the snapshots; a core keeping records later is read from its first
    closedState = initClosedState()
    closedSupported = false
    return true
  }
  closedSupported = true
  if (activeInstance && page.instance !== activeInstance) return false
  if (page.instance !== closedState.instance) {
    const { records, seqs, cleared, localEvictedUntil } = closedState
    closedState = {
      ...initClosedState(),
      instance: page.instance,
      startedAt: Date.parse(page.startedAt) || 0,
      records,
      seqs,
      cleared,
      localEvictedUntil,
    }
    page = await getClosedConnections(0, CLOSED_PAGE_SIZE)
    if (page === null || page.instance !== closedState.instance) return false
  }
  const target = page.latest
  closedState.caughtUp = false
  for (let pages = 1; ; pages++) {
    mergeClosedPage(page)
    if (closedState.seq >= target || page.records.length === 0) {
      closedState.caughtUp = true
      break
    }
    if (pages >= CLOSED_PAGES_PER_POLL) break
    const next = await getClosedConnections(closedState.seq, CLOSED_PAGE_SIZE)
    if (next === null || next.instance !== closedState.instance) break
    page = next
  }
  return true
}

/** The snapshot with the closed connections the core recorded. */
const withRecordedClosed = (
  data: ConnectionMonitorData,
): ConnectionMonitorData => {
  const { records, seqs, cleared } = closedState
  const closedIds = new Set<string>()
  for (const [key, item] of records) {
    if (key.startsWith(`${closedState.instance}/`)) closedIds.add(item.id)
  }
  const entries = [...records.entries()].sort(
    (a, b) => closedTime(b[1]) - closedTime(a[1]),
  )
  const recordedClosed = entries.map(([, item]) => item)
  const closedConnections = entries
    .filter(([key]) => cleared.get(key) !== seqs.get(key))
    .map(([, item]) => item)
  return {
    ...data,
    // a connection with a record is closed, whatever a snapshot taken before said
    activeConnections: closedIds.size
      ? data.activeConnections.filter((item) => !closedIds.has(item.id))
      : data.activeConnections,
    closedConnections,
    recordedClosed,
    closedFeed: {
      supported: true,
      caughtUp: closedState.caughtUp,
      missingUntil: Math.max(
        closedState.coreEvictedUntil,
        closedState.localEvictedUntil,
      ),
      startedAt: closedState.startedAt,
    },
  }
}

const mergeConnectionSummary = (
  payload: ConnectionSummaryPayload,
): ConnectionSummaryData => ({
  activeConnectionCount: payload.count ?? 0,
})

const applyConnectionSnapshot = (payload: IConnections) => {
  const now = Date.now()
  const elapsedMs = lastSnapshotAt ? now - lastSnapshotAt : 0
  lastSnapshotAt = now
  connectionData = {
    ...mergeConnectionSnapshot(payload, connectionData, elapsedMs, now),
    polledAt: now,
  }
  if (closedSupported) {
    connectionData = withRecordedClosed(connectionData)
  } else {
    connectionData = {
      ...connectionData,
      closedConnections: connectionData.recordedClosed.filter(
        (item) => closedTime(item) > snapshotClearedAt,
      ),
      closedFeed:
        closedSupported === false
          ? unsupportedClosedFeed
          : connectionData.closedFeed,
    }
  }
  notifyConnectionListeners()
}

// A poll dropped as the core changed meanwhile leaves the closed list behind.
const markClosedBehind = () => {
  if (!closedSupported || !connectionData.closedFeed.caughtUp) return
  connectionData = {
    ...connectionData,
    closedFeed: { ...connectionData.closedFeed, caughtUp: false },
  }
  notifyConnectionListeners()
}

// The full snapshot is pulled one request at a time rather than taken from the
// pushed stream: a push keeps coming however slowly the webview takes it in, and
// the large messages pile up in the app process meanwhile, as when the window is
// minimized or busy.
const createConnectionPoller = () => {
  let generation = 0
  // a request of any generation is in flight
  let polling = false
  let pollTimer: ReturnType<typeof setTimeout> | null = null
  const hasSubscribers = () => connectionListeners.size > 0

  const schedulePoll = (current: number) => {
    if (current !== generation || !hasSubscribers() || pollTimer) return
    pollTimer = window.setTimeout(() => {
      pollTimer = null
      void poll(current)
    }, CONNECTION_POLL_INTERVAL_MS)
  }

  const poll = async (current: number) => {
    if (current !== generation || !hasSubscribers()) return
    polling = true
    try {
      const { uploadTotal, downloadTotal, connections, instance } =
        await getConnections()
      let consistent = true
      // a core that reports its instance may have been upgraded to keep closed connections
      if (closedSupported !== false || instance) {
        try {
          consistent = await readClosed(instance)
        } catch (err) {
          // the active connections are still shown, the closed ones as far as read
          closedState.caughtUp = false
          console.warn('[Connections] Failed to fetch closed connections', err)
        }
      }
      if (current === generation && hasSubscribers()) {
        if (consistent) {
          applyConnectionSnapshot({
            uploadTotal,
            downloadTotal,
            // the plugin serializes its enums, typed as unions, as plain strings
            connections: (connections ?? []) as unknown as IConnectionsItem[],
          })
        } else {
          markClosedBehind()
        }
      }
    } catch (err) {
      console.warn('[Connections] Failed to fetch connections', err)
    } finally {
      polling = false
      // subscribed again while this request of a stopped generation was running
      if (current === generation) schedulePoll(current)
      else void poll(generation)
    }
  }

  return {
    start: () => {
      if (polling || pollTimer) return
      void poll(generation)
    },
    stopIfIdle: () => {
      if (hasSubscribers()) return
      // a request still in flight belongs to the old generation and is dropped
      generation += 1
      if (pollTimer) {
        window.clearTimeout(pollTimer)
        pollTimer = null
      }
      lastSnapshotAt = 0
    },
  }
}

interface SocketSupervisor {
  start: () => void
  stopIfIdle: () => void
}

const createSocketSupervisor = (options: {
  listeners: Set<ConnectionListener>
  connectSocket: () => Promise<MihomoWebSocket>
  onText: (data: string) => void
  closeLogLabel: string
}): SocketSupervisor => {
  const { listeners, connectSocket, onText, closeLogLabel } = options
  const hasSubscribers = () => listeners.size > 0
  let socket: MihomoWebSocket | null = null
  let connecting = false
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null

  const clearReconnectTimer = () => {
    if (!reconnectTimer) return
    window.clearTimeout(reconnectTimer)
    reconnectTimer = null
  }

  const closeSocket = async () => {
    const current = socket
    socket = null
    if (!current) return

    try {
      await current.close()
    } catch (err) {
      console.warn(`Failed to close ${closeLogLabel} websocket`, err)
    }
  }

  const scheduleReconnect = () => {
    if (!hasSubscribers()) return
    if (reconnectTimer) return
    reconnectTimer = window.setTimeout(() => {
      reconnectTimer = null
      void connect()
    }, CONNECTION_RECONNECT_DELAY_MS)
  }

  const reconnect = async () => {
    if (!hasSubscribers()) return
    await closeSocket()
    scheduleReconnect()
  }

  const connect = async () => {
    if (socket || connecting) return
    if (!hasSubscribers()) return

    clearReconnectTimer()
    connecting = true

    try {
      const connected = await connectSocket()
      if (!hasSubscribers()) {
        await connected.close()
        return
      }
      socket = connected
      connected.addListener((message) => {
        if (socket !== connected) return
        if (message.type !== 'Text') return
        if (message.data.startsWith('Websocket error')) {
          void reconnect()
          return
        }

        onText(message.data)
      })
    } catch {
      scheduleReconnect()
    } finally {
      connecting = false
      // subscribers may have come back while this attempt was closing its socket
      if (!socket) scheduleReconnect()
    }
  }

  return {
    start: () => {
      void connect()
    },
    stopIfIdle: () => {
      if (hasSubscribers()) return

      clearReconnectTimer()
      void closeSocket()
    },
  }
}

const handleSummaryText = (messageData: string) => {
  let payload: ConnectionSummaryPayload
  try {
    payload = JSON.parse(messageData) as ConnectionSummaryPayload
  } catch (err) {
    console.error(
      '[Connections] Failed to parse connections count payload',
      err,
    )
    return
  }

  connectionSummary = mergeConnectionSummary(payload)
  notifySummaryListeners()
}

const connectionPoller = createConnectionPoller()

const summarySupervisor = createSocketSupervisor({
  listeners: summaryListeners,
  connectSocket: () => MihomoWebSocket.connect_connections_count(),
  onText: handleSummaryText,
  closeLogLabel: 'connections count',
})

const getConnectionSnapshot = () => connectionData
const getConnectionSummarySnapshot = () => connectionSummary

const subscribeConnectionData = (listener: ConnectionListener) => {
  connectionListeners.add(listener)
  connectionPoller.start()
  return () => {
    connectionListeners.delete(listener)
    connectionPoller.stopIfIdle()
  }
}

const subscribeConnectionSummary = (listener: ConnectionListener) => {
  summaryListeners.add(listener)
  summarySupervisor.start()
  return () => {
    summaryListeners.delete(listener)
    summarySupervisor.stopIfIdle()
  }
}

const clearClosedConnectionData = () => {
  // the records stay for the recent statistics; only the list starts again
  closedState.cleared = new Map(closedState.seqs)
  snapshotClearedAt = Date.now()
  if (connectionData.closedConnections.length === 0) return
  connectionData = {
    ...connectionData,
    closedConnections: [],
  }
  notifyConnectionListeners()
}

export const useConnectionData = (options?: { enabled?: boolean }) => {
  const enabled = options?.enabled ?? true
  const subscribe = useCallback(
    (listener: ConnectionListener) =>
      enabled ? subscribeConnectionData(listener) : () => {},
    [enabled],
  )
  const data = useSyncExternalStore(
    subscribe,
    getConnectionSnapshot,
    getConnectionSnapshot,
  )
  const response = useMemo(() => ({ data }), [data])
  const clearClosedConnections = useCallback(() => {
    clearClosedConnectionData()
  }, [])

  return {
    response,
    clearClosedConnections,
  }
}

export const useConnectionSummaryData = (options?: { enabled?: boolean }) => {
  const enabled = options?.enabled ?? true
  const subscribe = useCallback(
    (listener: ConnectionListener) =>
      enabled ? subscribeConnectionSummary(listener) : () => {},
    [enabled],
  )
  const data = useSyncExternalStore(
    subscribe,
    getConnectionSummarySnapshot,
    getConnectionSummarySnapshot,
  )
  const response = useMemo(() => ({ data }), [data])

  return {
    response,
  }
}
