import { createContextState } from 'foxact/create-context-state'

const [ThemeModeProvider, useThemeMode, useSetThemeMode] = createContextState<
  'light' | 'dark'
>()

// save update state
const [UpdateStateProvider, useUpdateState, useSetUpdateState] =
  createContextState<{ version: string; fallbackReason?: string } | null>(null)

export {
  ThemeModeProvider,
  useThemeMode,
  useSetThemeMode,
  UpdateStateProvider,
  useUpdateState,
  useSetUpdateState,
}
