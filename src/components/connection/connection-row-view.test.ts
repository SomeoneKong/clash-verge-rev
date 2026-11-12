import { describe, expect, it } from 'vitest'

import { formatConnectionChains } from './connection-row-view'

describe('formatConnectionChains', () => {
  it('lists the groups before the node', () => {
    expect(formatConnectionChains(['hk', 'group'])).toBe('group / hk')
  })

  it('appends each dialer-proxy layer the node is reached through', () => {
    expect(
      formatConnectionChains(
        ['hk', 'group'],
        [['relay-node', 'relay'], ['jump']],
      ),
    ).toBe('group / hk ← relay / relay-node ← jump')
  })
})
