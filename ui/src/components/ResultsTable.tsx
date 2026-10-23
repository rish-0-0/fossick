// Results grid: TanStack Table for the column/sorting model, TanStack
// Virtual for row virtualization, so 10k-row result sets scroll smoothly.

import { useMemo, useRef, useState } from 'react'
import {
  ColumnDef,
  SortingState,
  flexRender,
  getCoreRowModel,
  getSortedRowModel,
  useReactTable,
} from '@tanstack/react-table'
import { useVirtualizer } from '@tanstack/react-virtual'
import type { Cell } from '../api'

const ROW_HEIGHT = 30

function CellValue({ value }: { value: Cell }) {
  if (value === null) return <span className="cell-null">–</span>
  return <>{String(value)}</>
}

export default function ResultsTable({ fields, rows }: { fields: string[]; rows: Cell[][] }) {
  const [sorting, setSorting] = useState<SortingState>([])

  const columns = useMemo<ColumnDef<Cell[], Cell>[]>(
    () =>
      fields.map((f, i) => ({
        id: `${i}:${f}`,
        header: f,
        accessorFn: (row) => row[i],
        // Mixed string/number columns: numeric when both coerce.
        sortingFn: (a, b) => {
          const x = a.getValue<Cell>(`${i}:${f}`)
          const y = b.getValue<Cell>(`${i}:${f}`)
          if (x === null) return y === null ? 0 : -1
          if (y === null) return 1
          const nx = Number(x)
          const ny = Number(y)
          if (!Number.isNaN(nx) && !Number.isNaN(ny) && String(x) !== '' && String(y) !== '')
            return nx < ny ? -1 : nx > ny ? 1 : 0
          return String(x) < String(y) ? -1 : String(x) > String(y) ? 1 : 0
        },
      })),
    [fields],
  )

  const table = useReactTable({
    data: rows,
    columns,
    state: { sorting },
    onSortingChange: setSorting,
    getCoreRowModel: getCoreRowModel(),
    getSortedRowModel: getSortedRowModel(),
  })

  const tableRows = table.getRowModel().rows
  const parentRef = useRef<HTMLDivElement>(null)
  const virtualizer = useVirtualizer({
    count: tableRows.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
  })
  const virtualRows = virtualizer.getVirtualItems()
  const padTop = virtualRows.length > 0 ? virtualRows[0].start : 0
  const padBottom =
    virtualRows.length > 0 ? virtualizer.getTotalSize() - virtualRows[virtualRows.length - 1].end : 0

  return (
    <div className="table-scroll" ref={parentRef}>
      <table className="results">
        <thead>
          {table.getHeaderGroups().map((hg) => (
            <tr key={hg.id}>
              {hg.headers.map((h) => (
                <th key={h.id} onClick={h.column.getToggleSortingHandler()}>
                  {flexRender(h.column.columnDef.header, h.getContext())}
                  <span className="sort-mark">
                    {{ asc: ' ▲', desc: ' ▼' }[h.column.getIsSorted() as string] ?? ''}
                  </span>
                </th>
              ))}
            </tr>
          ))}
        </thead>
        <tbody>
          {padTop > 0 && (
            <tr>
              <td style={{ height: padTop, padding: 0 }} colSpan={fields.length} />
            </tr>
          )}
          {virtualRows.map((vr) => {
            const row = tableRows[vr.index]
            return (
              <tr key={row.id} style={{ height: ROW_HEIGHT }}>
                {row.getVisibleCells().map((cell, ci) => (
                  <td key={cell.id} className={fields[ci] === '_raw' ? 'cell-raw' : undefined}>
                    <CellValue value={cell.getValue<Cell>()} />
                  </td>
                ))}
              </tr>
            )
          })}
          {padBottom > 0 && (
            <tr>
              <td style={{ height: padBottom, padding: 0 }} colSpan={fields.length} />
            </tr>
          )}
        </tbody>
      </table>
    </div>
  )
}
