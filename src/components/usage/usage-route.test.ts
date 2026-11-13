import { describe, expect, it } from 'vitest'

import type { UsageItem } from '@/services/cmds'

import {
  buildTree,
  routeLabel,
  type RouteText,
  type TreeNode,
} from './usage-route'

const text: RouteText = {
  unknownRelay: (dialer) => `${dialer} (unknown relay)`,
  proxyName: (item) =>
    item.overflow ? 'Proxies beyond the limit' : item.label,
}

const item = (
  key: string,
  label: string,
  total: number,
  groups: string[] = [],
  dialers: string[][] = [],
  overflow = false,
): UsageItem => ({
  key,
  label,
  provider: null,
  proxyType: null,
  unknown: false,
  overflow,
  route: { kind: 'exit', groups, dialers },
  total: [total, 0],
})

/** The tree as label: total, children indented. */
const outline = (nodes: TreeNode[], depth = 0): string[] =>
  nodes.flatMap((node) => [
    `${'  '.repeat(depth)}${node.level.label}: ${node.total[0]}`,
    ...outline(node.children, depth + 1),
  ])

describe('buildTree', () => {
  it('keeps the overflow apart from a proxy shown the same', () => {
    const roots = buildTree(
      [
        item('route:a', 'Proxies beyond the limit', 100),
        item('route:o', '*overflow*', 200, [], [], true),
      ],
      text,
    )
    expect(outline(roots)).toEqual([
      'Proxies beyond the limit: 200',
      'Proxies beyond the limit: 100',
    ])
    expect(roots.map((node) => node.item)).toEqual(['route:o', 'route:a'])
  })

  it('adds up the dialer groups a proxy goes through, group by group', () => {
    const roots = buildTree(
      [
        item(
          'a',
          'us',
          100,
          ['MyProxy'],
          [['JP1', 'InnerDialer', 'OuterDialer']],
        ),
        item(
          'b',
          'us',
          200,
          ['MyProxy'],
          [['JP2', 'InnerDialer', 'OuterDialer']],
        ),
      ],
      text,
    )
    expect(outline(roots)).toEqual([
      'MyProxy: 300',
      '  us: 300',
      '    ⇐ OuterDialer: 300',
      '      InnerDialer: 300',
      '        JP2: 200',
      '        JP1: 100',
    ])
  })

  it('counts each route once across layers, unknown relays and no dialer', () => {
    const roots = buildTree(
      [
        item('direct', 'us', 50),
        item(
          'two',
          'us',
          30,
          [],
          [
            ['A', 'G'],
            ['B', 'G'],
          ],
        ),
        item('reused', 'us', 20, [], [['', 'H']]),
        item('node', 'us', 10, [], [['R']]),
      ],
      text,
    )
    expect(outline(roots)).toEqual([
      'us: 110',
      '  ⇐ G: 30',
      '    A: 30',
      '      ⇐ G: 30',
      '        B: 30',
      '  ⇐ H (unknown relay): 20',
      '  ⇐ R: 10',
    ])
    expect(roots[0].own).toEqual([50, 0])
  })
})

describe('routeLabel', () => {
  it('puts a route on one line, layer by layer', () => {
    expect(
      routeLabel(
        item(
          'a',
          'us',
          1,
          ['MyProxy', 'Dns'],
          [
            ['JP', 'Inner', 'Outer'],
            ['', 'H'],
          ],
        ),
        text,
      ),
    ).toBe('Dns › MyProxy › us ⇐ Outer › Inner › JP ⇐ H (unknown relay)')
  })
})
