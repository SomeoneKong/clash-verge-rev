import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { expect, it, vi } from 'vitest'

import { HistoryList } from './update-history-viewer'

const page = vi.hoisted(() => ({
  data: [[{ seq: '4', source: 'actual', event: 'first-page-event' }]],
  error: {} as { code?: string; detail?: string },
}))

vi.mock('@/hooks/use-fork-update', () => ({ useUpdateHistory: () => page }))
vi.mock('@/components/base', () => ({ BaseDialog: 'dialog' }))
vi.mock('./setting-comp', () => ({ SettingItem: 'div' }))
vi.mock('react-i18next', () => ({
  useTranslation: () => ({
    t: (key: string) => key,
    i18n: { language: 'en' },
  }),
}))

it.each(['invalid audit page order', 'could not read the audit log'])(
  'keeps the first page visible and shows the audit error %s',
  (detail) => {
    page.error = { detail }
    const failed = renderToStaticMarkup(createElement(HistoryList))
    expect(failed).toContain('first-page-event')
    expect(failed).toContain(detail)
    expect(failed).not.toContain('clash-verge-updater log')
  },
)

it.each(['broken pipe', 'the update component did not answer in time'])(
  'keeps the first page visible and offers offline history after %s',
  (detail) => {
    page.error = { code: 'UPDATE_COMPONENT_UNAVAILABLE', detail }
    const offline = renderToStaticMarkup(createElement(HistoryList))
    expect(offline).toContain('first-page-event')
    expect(offline).toContain('clash-verge-updater log')
  },
)
