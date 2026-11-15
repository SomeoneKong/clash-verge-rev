import { expect, it } from 'vitest'

import { selectGlobalChainNodes, type ProxyViewV1 } from './proxy-view'

const coreNode = (name: string) => ({
  recordId: `r:${name}`,
  name,
  source: { kind: 'core', proxyName: name },
})

it('lists the chain nodes of global mode in the order of the config', () => {
  const names = ['1', '10', '2', 'DIRECT', 'from-provider']
  const view = {
    global: { name: 'GLOBAL', members: [] },
    groups: [],
    records: {
      ...Object.fromEntries(names.map((name) => [`r:${name}`, coreNode(name)])),
      'r:from-provider': {
        recordId: 'r:from-provider',
        name: 'from-provider',
        source: { kind: 'provider', providerName: 'p', proxyName: '2' },
      },
    },
    standalone: names.map((name) => `r:${name}`),
  } as unknown as ProxyViewV1
  const runtimeProxies = [{ name: '2' }, { name: '1' }, { name: '10' }]

  expect(
    selectGlobalChainNodes(view, runtimeProxies).map(({ name }) => name),
  ).toEqual(['2', '1', '10'])
})
