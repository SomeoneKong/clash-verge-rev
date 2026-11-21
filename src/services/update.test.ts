import { expect, it, vi } from 'vitest'

import { forkCheckUpdate } from '@/services/cmds'

import { checkUpdateSafe } from './update'

vi.mock('@/services/cmds', () => ({ forkCheckUpdate: vi.fn() }))

it('returns the checked release and its fork metadata to the manual dialog', async () => {
  const rawJson = { fork: { test: true } }
  vi.mocked(forkCheckUpdate).mockResolvedValueOnce({
    rid: 1,
    currentVersion: '2.0.0',
    version: '99.0.0',
    body: 'break change',
    rawJson,
  })
  const release = await checkUpdateSafe({
    headers: { 'Cache-Control': 'no-cache' },
    timeout: 5000,
    proxy: 'http://localhost:8080',
  })
  expect(forkCheckUpdate).toHaveBeenCalledWith(
    [['cache-control', 'no-cache']],
    5000,
    'http://localhost:8080',
    null,
  )
  expect(release).toMatchObject({
    available: true,
    version: '99.0.0',
    body: 'break change',
    rawJson,
  })
})
