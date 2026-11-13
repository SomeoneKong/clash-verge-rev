import {
  Box,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableRow,
  Typography,
} from '@mui/material'
import { useTranslation } from 'react-i18next'

import { formatBytes } from './usage-format'

export interface UsageRow {
  key: string
  label: string
  detail: string | null
  color: string
  total: [number, number]
}

export const UsageTable = ({ rows }: { rows: UsageRow[] }) => {
  const { t } = useTranslation()
  const sum = rows.reduce((all, row) => all + row.total[0] + row.total[1], 0)

  return (
    <Table size="small">
      <TableHead>
        <TableRow>
          <TableCell>{t('usage.table.name')}</TableCell>
          <TableCell align="right">{t('usage.table.upload')}</TableCell>
          <TableCell align="right">{t('usage.table.download')}</TableCell>
          <TableCell align="right">{t('usage.table.total')}</TableCell>
          <TableCell align="right">{t('usage.table.share')}</TableCell>
        </TableRow>
      </TableHead>
      <TableBody>
        {rows.map((row) => {
          const total = row.total[0] + row.total[1]
          return (
            <TableRow key={row.key}>
              <TableCell>
                <Box sx={{ display: 'flex', alignItems: 'center', gap: 1 }}>
                  <Box
                    sx={{
                      width: 10,
                      height: 10,
                      borderRadius: '2px',
                      bgcolor: row.color,
                      flexShrink: 0,
                    }}
                  />
                  <Typography variant="body2">{row.label}</Typography>
                  {row.detail && (
                    <Typography variant="caption" color="text.secondary">
                      {row.detail}
                    </Typography>
                  )}
                </Box>
              </TableCell>
              <TableCell align="right">{formatBytes(row.total[0])}</TableCell>
              <TableCell align="right">{formatBytes(row.total[1])}</TableCell>
              <TableCell align="right">{formatBytes(total)}</TableCell>
              <TableCell align="right">
                {sum > 0 ? `${((total / sum) * 100).toFixed(1)}%` : '-'}
              </TableCell>
            </TableRow>
          )
        })}
      </TableBody>
    </Table>
  )
}
