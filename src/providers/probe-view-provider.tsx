import {
  type ReactNode,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react'
import { getVersion } from 'tauri-plugin-mihomo-api'

import { useRuntimeConfig } from '@/hooks/use-clash'
import {
  ProbeViewContext,
  type ProbeViewState,
  type ProbeViewStatus,
} from '@/hooks/use-probe-view'
import { subscribeProbeMemoryCleared } from '@/services/bus/probe-memory'
import { getProbeView } from '@/services/cmds'
import delayManager from '@/services/delay'
import { useQuery } from '@/services/query-client'
import type { ProbeViewData } from '@/types/probe-view'

const STRATEGY_MODE_KEY = 'clash-verge-proxy-strategy-mode'
const REFRESH_INTERVAL = 5000
const UNSUPPORTED_RECHECK = 60000
// after a delay test of a group settles, for the group to take its result in
const SETTLED_REFRESH_DELAY = 300

const readEnabled = () => {
  try {
    return localStorage.getItem(STRATEGY_MODE_KEY) === 'true'
  } catch {
    return false
  }
}

export const ProbeViewProvider = ({ children }: { children: ReactNode }) => {
  const [enabledValue, setEnabledValue] = useState(readEnabled)
  const enabled = enabledValue
  const [status, setStatus] = useState<ProbeViewStatus>('loading')
  const [data, setData] = useState<ProbeViewData | null>(null)
  const [fetchedAt, setFetchedAt] = useState<number | null>(null)
  const [stale, setStale] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [history, setHistory] = useState<{
    open: boolean
    group?: string
    token: number
  }>({
    open: false,
    token: 0,
  })
  const openHistory = useCallback(
    (group?: string) =>
      setHistory((current) => ({
        open: true,
        group,
        token: current.token + 1,
      })),
    [],
  )
  const closeHistory = useCallback(
    () => setHistory((current) => ({ ...current, open: false })),
    [],
  )
  // Requests are numbered as sent: an answer to an earlier one that comes late is dropped.
  const sentRef = useRef(0)
  const appliedRef = useRef(0)
  // Only the polling of the current generation schedules the next read.
  const generationRef = useRef(0)
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const statusRef = useRef<ProbeViewStatus>('off')

  const setEnabled = useCallback((next: boolean) => {
    setEnabledValue(next)
    try {
      localStorage.setItem(STRATEGY_MODE_KEY, String(next))
    } catch {
      // the choice lasts this session
    }
  }, [])

  const fetchView = useCallback(async () => {
    const seq = ++sentRef.current
    try {
      const result = await getProbeView()
      if (seq < appliedRef.current) return
      appliedRef.current = seq
      if (result.status === 'ok' && result.view) {
        const view = result.view as unknown as ProbeViewData
        setData(view)
        setFetchedAt(Date.now())
        setStale(false)
        setError(null)
        statusRef.current = view.groups.length > 0 ? 'ok' : 'noGroups'
      } else if (result.status === 'unsupported') {
        setData(null)
        setStale(false)
        setError(null)
        statusRef.current = 'unsupported'
      } else {
        setStale(true)
        setError(result.error ?? null)
        statusRef.current = 'failed'
      }
    } catch (err) {
      if (seq < appliedRef.current) return
      appliedRef.current = seq
      setStale(true)
      setError(String(err))
      statusRef.current = 'failed'
    }
    setStatus(statusRef.current)
  }, [])

  /** Ends the polling: a read still running shows nothing and schedules nothing. */
  const stop = useCallback(() => {
    generationRef.current += 1
    appliedRef.current = ++sentRef.current
    if (timerRef.current) clearTimeout(timerRef.current)
    timerRef.current = null
  }, [])

  /** Reads now and polls from then on, in place of any polling before. */
  const start = useCallback(() => {
    stop()
    const generation = generationRef.current
    const poll = async () => {
      await fetchView()
      if (generation !== generationRef.current) return
      const wait =
        statusRef.current === 'unsupported'
          ? UNSUPPORTED_RECHECK
          : REFRESH_INTERVAL
      timerRef.current = setTimeout(() => void poll(), wait)
    }
    void poll()
  }, [fetchView, stop])

  useEffect(() => {
    if (!enabled) {
      stop()
      statusRef.current = 'off'
      return
    }
    const onVisibility = () => {
      if (document.hidden) stop()
      else start()
    }
    if (!document.hidden) start()
    document.addEventListener('visibilitychange', onVisibility)
    return () => {
      stop()
      document.removeEventListener('visibilitychange', onVisibility)
    }
  }, [enabled, start, stop])

  const refresh = useCallback(() => {
    if (enabled && !document.hidden) start()
  }, [enabled, start])

  useEffect(() => subscribeProbeMemoryCleared(refresh), [refresh])

  // A new config or core may bring another strategy, or support for the view.
  const { data: runtimeConfig } = useRuntimeConfig()
  const { data: version } = useQuery({
    queryKey: ['getVersion'],
    queryFn: getVersion,
  })
  const coreVersion = version?.version
  const signalsRef = useRef({ runtimeConfig, coreVersion })
  useEffect(() => {
    const last = signalsRef.current
    if (
      last.runtimeConfig === runtimeConfig &&
      last.coreVersion === coreVersion
    ) {
      return
    }
    signalsRef.current = { runtimeConfig, coreVersion }
    refresh()
  }, [runtimeConfig, coreVersion, refresh])

  // A delay test of a member reads the strategy again once it settles, rather than at the
  // next poll: the core takes the result into the evidence of the groups.
  const groupNames = JSON.stringify(
    data?.groups.map((group) => group.name) ?? [],
  )
  useEffect(() => {
    const names: string[] = JSON.parse(groupNames)
    if (!enabled || names.length === 0) return
    let timer: ReturnType<typeof setTimeout> | null = null
    const onSettle = () => {
      if (timer) clearTimeout(timer)
      timer = setTimeout(refresh, SETTLED_REFRESH_DELAY)
    }
    const unsubscribes = names.map((name) =>
      delayManager.addGroupListener(name, onSettle),
    )
    return () => {
      if (timer) clearTimeout(timer)
      for (const unsubscribe of unsubscribes) unsubscribe()
    }
  }, [enabled, groupNames, refresh])

  const groups = useMemo(() => {
    if (!enabled || !data) return null
    return new Map(data.groups.map((group) => [group.name, group]))
  }, [enabled, data])

  const value = useMemo<ProbeViewState>(
    () => ({
      enabled,
      setEnabled,
      status: enabled ? status : 'off',
      data: enabled ? data : null,
      fetchedAt,
      stale,
      error,
      groups,
      refresh,
      history,
      openHistory,
      closeHistory,
    }),
    [
      enabled,
      setEnabled,
      status,
      data,
      fetchedAt,
      stale,
      error,
      groups,
      refresh,
      history,
      openHistory,
      closeHistory,
    ],
  )

  return <ProbeViewContext value={value}>{children}</ProbeViewContext>
}
