import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/lib/components/ui/table'
import { cn } from '@/lib/utils/cn'

/** A reference table. The first column names the row and reads in the foreground colour. */
export function SpecTable({
  columns,
  rows,
  className,
}: {
  columns: string[]
  rows: React.ReactNode[][]
  className?: string
}) {
  return (
    <div className={cn('not-prose border-y border-border', className)}>
      <Table className="text-body">
        <TableHeader>
          <TableRow>
            {columns.map((column) => (
              <TableHead key={column} className="align-bottom text-label text-foreground">
                {column}
              </TableHead>
            ))}
          </TableRow>
        </TableHeader>
        <TableBody>
          {rows.map((row, rowIndex) => (
            <TableRow key={rowIndex}>
              {row.map((cell, cellIndex) => (
                <TableCell
                  key={cellIndex}
                  className={cn(
                    'min-w-32 align-top whitespace-normal',
                    cellIndex === 0 ? 'font-medium text-foreground' : 'text-soft-foreground'
                  )}
                >
                  {cell}
                </TableCell>
              ))}
            </TableRow>
          ))}
        </TableBody>
      </Table>
    </div>
  )
}
