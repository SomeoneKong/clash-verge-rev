import { ChevronRightRounded, ExpandMoreRounded } from '@mui/icons-material'
import {
  Box,
  IconButton,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableRow,
  Typography,
} from '@mui/material'
import { type ReactNode, useMemo, useState } from 'react'
import { useTranslation } from 'react-i18next'

import type { UsageItem } from '@/services/cmds'

import { formatBytes } from './usage-format'
import { buildTree, type RouteText, sum, type TreeNode } from './usage-route'

/**
 * The usage of every route as a tree: the group a rule chose, the groups nested in it, the
 * proxy, then its dialer layers, each with the total of what is below it. `colors` gives the
 * colors of the routes the chart shows.
 */
export const UsageRouteTree = ({
  items,
  colors,
  text,
}: {
  items: UsageItem[]
  colors: Map<string, string>
  text: RouteText
}) => {
  const { t } = useTranslation()
  const roots = useMemo(() => buildTree(items, text), [items, text])
  const grand = roots.reduce((all, node) => all + sum(node.total), 0)
  // Top levels start expanded, the others collapsed; a click turns one around.
  const [toggled, setToggled] = useState<Set<string>>(() => new Set())
  const toggle = (key: string) =>
    setToggled((current) => {
      const next = new Set(current)
      if (!next.delete(key)) next.add(key)
      return next
    })

  const row = (
    key: string,
    depth: number,
    label: string,
    detail: string | null,
    total: [number, number],
    color: string | undefined,
    expander: ReactNode,
  ) => (
    <TableRow key={key}>
      <TableCell>
        <Box
          sx={{
            display: 'flex',
            alignItems: 'center',
            gap: 1,
            pl: depth * 2.5,
            minWidth: 0,
          }}
        >
          <Box sx={{ width: 28, flexShrink: 0 }}>{expander}</Box>
          <Box
            sx={{
              width: 10,
              height: 10,
              borderRadius: '2px',
              bgcolor: color ?? 'transparent',
              flexShrink: 0,
            }}
          />
          <Typography variant="body2" noWrap title={label}>
            {label}
          </Typography>
          {detail && (
            <Typography variant="caption" color="text.secondary" noWrap>
              {detail}
            </Typography>
          )}
        </Box>
      </TableCell>
      <TableCell align="right">{formatBytes(total[0])}</TableCell>
      <TableCell align="right">{formatBytes(total[1])}</TableCell>
      <TableCell align="right">{formatBytes(sum(total))}</TableCell>
      <TableCell align="right">
        {grand > 0 ? `${((sum(total) / grand) * 100).toFixed(1)}%` : '-'}
      </TableCell>
    </TableRow>
  )

  const rows = (node: TreeNode, depth: number): ReactNode[] => {
    const parent = node.children.length > 0
    const expanded = parent && (depth === 0) !== toggled.has(node.key)
    const expander = parent && (
      <IconButton size="small" onClick={() => toggle(node.key)}>
        {expanded ? (
          <ExpandMoreRounded fontSize="small" />
        ) : (
          <ChevronRightRounded fontSize="small" />
        )}
      </IconButton>
    )
    const color = node.item ? colors.get(node.item) : undefined
    const own = node.item ? node.own : ([0, 0] as [number, number])
    return [
      row(
        node.key,
        depth,
        node.level.label,
        node.level.detail,
        node.total,
        parent ? undefined : color,
        expander,
      ),
      ...(expanded
        ? [
            ...node.children.flatMap((child) => rows(child, depth + 1)),
            ...(sum(own) > 0
              ? [
                  row(
                    `${node.key}\u0001own`,
                    depth + 1,
                    t('usage.labels.noDialer'),
                    null,
                    own,
                    color,
                    null,
                  ),
                ]
              : []),
          ]
        : []),
    ]
  }

  return (
    <Table size="small" sx={{ tableLayout: 'fixed' }}>
      <TableHead>
        <TableRow>
          <TableCell sx={{ width: '52%' }}>{t('usage.table.route')}</TableCell>
          <TableCell align="right">{t('usage.table.upload')}</TableCell>
          <TableCell align="right">{t('usage.table.download')}</TableCell>
          <TableCell align="right">{t('usage.table.total')}</TableCell>
          <TableCell align="right">{t('usage.table.share')}</TableCell>
        </TableRow>
      </TableHead>
      <TableBody>{roots.flatMap((node) => rows(node, 0))}</TableBody>
    </Table>
  )
}
