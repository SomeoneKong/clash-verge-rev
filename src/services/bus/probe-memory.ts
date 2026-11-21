const listeners = new Set<() => void>()

export function notifyProbeMemoryCleared() {
  for (const listener of listeners) listener()
}

export function subscribeProbeMemoryCleared(listener: () => void) {
  listeners.add(listener)
  return () => {
    listeners.delete(listener)
  }
}
