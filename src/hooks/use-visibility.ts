import { getCurrentWindow } from '@tauri-apps/api/window'
import { useEffect, useState } from 'react'

const isDocumentVisible = () =>
  typeof document === 'undefined' || document.visibilityState === 'visible'

export const useVisibility = () => {
  const [visible, setVisible] = useState(isDocumentVisible)

  useEffect(() => {
    let mounted = true
    let visibilityTimer: ReturnType<typeof setTimeout> | null = null
    // only the latest query applies, an earlier one may answer last
    let queryVersion = 0
    const appWindow = getCurrentWindow()

    // A minimized window stays visible to the OS, and on Windows the webview
    // keeps its document visible too, so minimized is checked on its own.
    const updateVisible = async () => {
      const version = ++queryVersion
      const [windowVisible, windowMinimized] = await Promise.all([
        appWindow.isVisible().catch(() => true),
        appWindow.isMinimized().catch(() => false),
      ])
      if (mounted && version === queryVersion) {
        setVisible(isDocumentVisible() && windowVisible && !windowMinimized)
      }
    }

    const updateVisibleSoon = () => {
      if (visibilityTimer) {
        window.clearTimeout(visibilityTimer)
      }
      visibilityTimer = window.setTimeout(() => {
        visibilityTimer = null
        void updateVisible()
      }, 50)
    }

    const handleVisibleEvent = () => {
      void updateVisible()
    }

    const handlePointerDown = () => {
      queryVersion += 1
      setVisible(true)
    }

    document.addEventListener('focus', handleVisibleEvent)
    document.addEventListener('pointerdown', handlePointerDown)
    document.addEventListener('visibilitychange', handleVisibleEvent)
    window.addEventListener('focus', handleVisibleEvent)

    const unlistenFocusChanged = appWindow.onFocusChanged(updateVisibleSoon)
    // minimizing a window that has already lost focus only resizes it
    const unlistenResized = appWindow.onResized(updateVisibleSoon)
    const unlistenCloseRequested = appWindow.onCloseRequested(() => {
      setVisible(false)
      updateVisibleSoon()
    })
    void updateVisible()

    return () => {
      mounted = false
      if (visibilityTimer) {
        window.clearTimeout(visibilityTimer)
        visibilityTimer = null
      }
      document.removeEventListener('focus', handleVisibleEvent)
      document.removeEventListener('pointerdown', handlePointerDown)
      document.removeEventListener('visibilitychange', handleVisibleEvent)
      window.removeEventListener('focus', handleVisibleEvent)
      void unlistenFocusChanged.then((unlisten) => unlisten())
      void unlistenResized.then((unlisten) => unlisten())
      void unlistenCloseRequested.then((unlisten) => unlisten())
    }
  }, [])

  return visible
}
