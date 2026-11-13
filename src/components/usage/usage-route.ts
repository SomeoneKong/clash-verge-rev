import type { UsageItem } from '@/services/cmds'

/** How a route is put into words. */
export interface RouteText {
  /** Names a dialer layer whose node is unknown by its dialer proxy. */
  unknownRelay: (dialer: string) => string
  /** The proxy of an item as shown. */
  proxyName: (item: UsageItem) => string
}

/** One level of a route: a group, the proxy, or a group or node of a dialer layer. */
export interface RouteLevel {
  /** Tells the level apart from the others under the same parent; never what is shown. */
  key: string
  label: string
  /** The proxy's provider and type, on the proxy's level. */
  detail: string | null
}

/** The proxy level of an item: the same proxy shares it across its routes. */
const proxyKey = (item: UsageItem) => {
  if (item.overflow) return 'overflow'
  if (item.unknown) return `unknown:${item.key}`
  return `proxy:${item.provider ?? ''}\u0000${item.label}`
}

/**
 * The levels of an item's route, outermost first: the group a rule chose, the groups nested
 * in it, the proxy, then for each dialer layer its groups from the outermost in and the node
 * that relayed. The first level of a layer is marked with ⇐; a layer whose node is unknown
 * is one level.
 */
const routeLevels = (item: UsageItem, text: RouteText): RouteLevel[] => {
  const levels: RouteLevel[] = [...(item.route?.groups ?? [])]
    .reverse()
    .map((group) => ({ key: `group:${group}`, label: group, detail: null }))
  levels.push({
    key: proxyKey(item),
    label: text.proxyName(item),
    detail: [item.provider, item.proxyType].filter(Boolean).join(' · ') || null,
  })
  ;(item.route?.dialers ?? []).forEach(([node, ...layerGroups], layer) => {
    const groups = [...layerGroups].reverse()
    if (!node) {
      levels.push({
        key: `dialer${layer}:unknown:${groups.join('\u0000')}`,
        label: `⇐ ${text.unknownRelay(groups[groups.length - 1] ?? '')}`,
        detail: null,
      })
      return
    }
    groups.forEach((group, index) =>
      levels.push({
        key: `dialer${layer}:group:${group}`,
        label: index === 0 ? `⇐ ${group}` : group,
        detail: null,
      }),
    )
    levels.push({
      key: `dialer${layer}:node:${node}`,
      label: groups.length === 0 ? `⇐ ${node}` : node,
      detail: null,
    })
  })
  return levels
}

/** An item's route on one line: groups › proxy ⇐ dialer groups › node ⇐ … */
export const routeLabel = (item: UsageItem, text: RouteText) => {
  const outward = [...(item.route?.groups ?? [])].reverse()
  const parts = [[...outward, text.proxyName(item)].join(' › ')]
  for (const [node, ...layerGroups] of item.route?.dialers ?? []) {
    const groups = [...layerGroups].reverse()
    parts.push(
      node
        ? [...groups, node].join(' › ')
        : text.unknownRelay(groups[groups.length - 1] ?? ''),
    )
  }
  return parts.join(' ⇐ ')
}

/** A level of the routes with the total of all routes through it. */
export interface TreeNode {
  key: string
  level: RouteLevel
  total: [number, number]
  /** The traffic of the route that ends here, when longer ones go on from here. */
  own: [number, number]
  /** The item of the route that ends here. */
  item: string | null
  children: TreeNode[]
}

const add = (into: [number, number], [up, down]: [number, number]) => {
  into[0] += up
  into[1] += down
}

export const sum = ([up, down]: [number, number]) => up + down

/** The routes of the items as a tree of their levels, the largest first at each level. */
export const buildTree = (items: UsageItem[], text: RouteText) => {
  const roots: TreeNode[] = []
  for (const item of items) {
    let siblings = roots
    let path = ''
    let node: TreeNode | undefined
    for (const level of routeLevels(item, text)) {
      path = `${path}\u0001${level.key}`
      node = siblings.find((child) => child.key === path)
      if (!node) {
        node = {
          key: path,
          level,
          total: [0, 0],
          own: [0, 0],
          item: null,
          children: [],
        }
        siblings.push(node)
      }
      add(node.total, item.total)
      siblings = node.children
    }
    if (node) {
      add(node.own, item.total)
      node.item = item.key
    }
  }
  const sort = (nodes: TreeNode[]) => {
    nodes.sort((a, b) => sum(b.total) - sum(a.total))
    nodes.forEach((node) => sort(node.children))
  }
  sort(roots)
  return roots
}
