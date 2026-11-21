import { expect, it, vi } from 'vitest'

import { formatTime } from '@/types/probe-view'

import { displayVersion, historyPageKey } from './use-fork-update'

vi.mock('@/services/cmds', () => ({}))
vi.mock('@/services/update', () => ({}))
vi.mock('./use-verge', () => ({}))

it('continues a short audit page from its last delivered sequence until the component returns an empty page', () => {
  expect(historyPageKey(0, null)).toEqual(['fork-update-history', null])
  expect(
    historyPageKey(1, [
      { seq: '9007199254740995' },
      { seq: '9007199254740993' },
    ]),
  ).toEqual(['fork-update-history', '9007199254740993'])
  expect(historyPageKey(2, [{ seq: '1' }])).toEqual([
    'fork-update-history',
    '1',
  ])
  expect(historyPageKey(3, [])).toBeNull()
})

it('formats app and core versions with one prefix and renders Traditional Chinese timestamps', () => {
  expect(displayVersion('2.0.0')).toBe('v2.0.0')
  expect(displayVersion('v1.19.34-custom.9')).toBe('v1.19.34-custom.9')
  const time = '2026-10-01T12:30:00Z'
  expect(formatTime(time, 'zhtw')).toBe(new Date(time).toLocaleString('zh-TW'))
  for (const lang of ['zh', 'zhtw', 'jp']) {
    expect(formatTime(Date.parse(time), lang)).toBe(formatTime(time, lang))
  }
})
