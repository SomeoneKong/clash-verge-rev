// The view of the core's probe strategy, made of generic elements whose texts the core writes;
// the GUI renders them without knowing what they mean.

export type ProbeText = Partial<Record<string, string>>

export type ProbeLevel = 'ok' | 'info' | 'warn' | 'error' | 'muted'

export interface ProbeElement {
  type: string
  id?: string
  level?: string
  text?: ProbeText
  label?: ProbeText
  tip?: ProbeText
  value?: number
  unit?: string
  at?: string
}

export interface ProbeSection {
  title: ProbeText
  items: ProbeElement[]
}

interface ProbeRole {
  id: string
  text: ProbeText
  level?: string
  tip?: ProbeText
}

export interface ProbeViewMember {
  name: string
  index: number
  provider: string
  object: number
  position: number
  role: ProbeRole
  headline: ProbeElement[]
  sections: ProbeSection[]
}

export interface ProbeViewGroup {
  name: string
  headline: ProbeElement[]
  sections: ProbeSection[]
  members: ProbeViewMember[]
}

export interface ProbeViewData {
  view: number
  instance: string
  generatedAt: string
  banners: ProbeElement[]
  groups: ProbeViewGroup[]
}

/** The major version of the view this GUI knows. */
export const PROBE_VIEW_VERSION = 1

interface ProbeDisplay {
  v?: number
  category?: string
  categoryText?: ProbeText
  level?: string
  scope?: { kind?: string; groups?: string[] }
  members?: { name: string; provider?: string; object?: number }[]
  text?: ProbeText
}

export interface ProbeHistoryEvent {
  time: string
  instance: string
  seq: number
  display: ProbeDisplay
}

export interface ProbeHistoryGap {
  from: number
  to: number
  reason: string
  afterTime: string | null
  beforeTime: string | null
}

interface ProbeInstanceCoverage {
  instance: string
  startedAt: string
  savedTo: number
  savedTime: string | null
  observedAt: string | null
  observedLatest: number | null
  pending: number | null
  gaps: ProbeHistoryGap[]
  coverageUnknown: boolean
  tailUnknown: boolean
  ended: boolean
  display: boolean | null
}

export interface ProbeHistoryPage {
  events: ProbeHistoryEvent[]
  truncated: boolean
  groups: string[]
  members: string[]
  categories: string[]
  instances: ProbeInstanceCoverage[]
  retainedSince: string | null
}

/** The text in the language of the GUI: the core writes Chinese and English. */
export const pickText = (text: ProbeText | undefined, language: string) => {
  if (!text) return ''
  return text[language] ?? text.en ?? Object.values(text).find(Boolean) ?? ''
}

const LEVELS: readonly string[] = ['ok', 'info', 'warn', 'error', 'muted']

/** The color of a chip of a level. */
export const CHIP_COLOR: Record<
  ProbeLevel,
  'success' | 'info' | 'warning' | 'error' | 'default'
> = {
  ok: 'success',
  info: 'info',
  warn: 'warning',
  error: 'error',
  muted: 'default',
}

export const levelOf = (level: string | undefined): ProbeLevel =>
  level && LEVELS.includes(level) ? (level as ProbeLevel) : 'info'

// The languages of the GUI whose keys are not BCP 47 tags.
const LOCALES: Partial<Record<string, string>> = {
  zh: 'zh-CN',
  zhtw: 'zh-TW',
  jp: 'ja',
}

const localeOf = (language: string) => {
  const tag = LOCALES[language] ?? language
  try {
    return Intl.getCanonicalLocales(tag)[0]
  } catch {
    return undefined
  }
}

/** A time in the language of the GUI. */
export const formatTime = (iso: string, language: string) => {
  const at = new Date(iso)
  if (Number.isNaN(at.getTime())) return iso
  return at.toLocaleString(localeOf(language))
}

const relative = (at: Date, language: string) => {
  const seconds = Math.round((at.getTime() - Date.now()) / 1000)
  const format = new Intl.RelativeTimeFormat(localeOf(language), {
    numeric: 'auto',
  })
  const abs = Math.abs(seconds)
  if (abs < 60) return format.format(seconds, 'second')
  if (abs < 3600) return format.format(Math.round(seconds / 60), 'minute')
  if (abs < 86400) return format.format(Math.round(seconds / 3600), 'hour')
  return format.format(Math.round(seconds / 86400), 'day')
}

export const formatMoment = (iso: string, language: string) => {
  const at = new Date(iso)
  if (Number.isNaN(at.getTime())) return iso
  return `${formatTime(iso, language)} (${relative(at, language)})`
}

/** A key of an element among its siblings: its id, or what it shows. */
export const elementKey = (element: ProbeElement) =>
  element.id ??
  `${element.type}:${pickText(element.label, 'en')}:${pickText(element.text, 'en')}:${element.at ?? ''}`
