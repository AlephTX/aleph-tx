import { useMemo, useState, type ReactNode } from 'react'
import { tone, toneClass } from '../format'

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' })

export function Card({
  title,
  right,
  children,
  className = '',
}: {
  title?: ReactNode
  right?: ReactNode
  children: ReactNode
  className?: string
}) {
  return (
    <section className={`card flex min-w-0 flex-col ${className}`}>
      {(title || right) && (
        <header className="flex flex-wrap items-center justify-between gap-3 border-b border-line px-4 py-2.5">
          <h2 className="text-xs font-semibold uppercase tracking-wider text-muted">{title}</h2>
          {right}
        </header>
      )}
      <div className="min-h-0 flex-1">{children}</div>
    </section>
  )
}

const badgeTone = {
  ok: 'bg-up/12 text-up ring-up/30',
  bad: 'bg-down/12 text-down ring-down/30',
  warn: 'bg-warn/12 text-warn ring-warn/30',
  idle: 'bg-muted/10 text-muted ring-muted/25',
  info: 'bg-accent/12 text-accent ring-accent/30',
} as const

export type BadgeTone = keyof typeof badgeTone

export function Badge({ tone: t, children, dot }: { tone: BadgeTone; children: ReactNode; dot?: boolean }) {
  return (
    <span
      className={`inline-flex items-center gap-1.5 rounded px-1.5 py-0.5 text-[11px] font-medium ring-1 ring-inset ${badgeTone[t]}`}
    >
      {dot && <span className={`size-1.5 rounded-full bg-current ${t === 'ok' ? 'animate-pulse' : ''}`} />}
      {children}
    </span>
  )
}

/** Colors a signed value green/red. */
export function Signed({ value, children }: { value: number | null | undefined; children: ReactNode }) {
  return <span className={`num ${toneClass[tone(value)]}`}>{children}</span>
}

export function Side({ side }: { side: 'buy' | 'sell' | null }) {
  if (!side) return <span className="text-muted">—</span>
  return <span className={side === 'buy' ? 'text-up' : 'text-down'}>{side === 'buy' ? '买' : '卖'}</span>
}

export function Empty({ children }: { children: ReactNode }) {
  return <div className="px-4 py-8 text-center text-sm text-muted">{children}</div>
}

export function ErrorNote({ error }: { error: unknown }) {
  if (!error) return null
  return (
    <div className="px-4 py-3 text-sm text-down">
      加载失败：{error instanceof Error ? error.message : String(error)}
    </div>
  )
}

export type SortDirection = 'ascending' | 'descending'

export interface Column<T> {
  key: string
  label: string
  align?: 'left' | 'right'
  /** Raw numeric or string value used for accessible sorting; null sorts last. */
  sortValue?: (row: T) => number | string | null | undefined
  render: (row: T) => ReactNode
}

function compareValues(
  a: number | string | null | undefined,
  b: number | string | null | undefined,
  direction: SortDirection,
): number {
  const aMissing = a === null || a === undefined || (typeof a === 'number' && !Number.isFinite(a))
  const bMissing = b === null || b === undefined || (typeof b === 'number' && !Number.isFinite(b))
  if (aMissing && bMissing) return 0
  if (aMissing) return 1
  if (bMissing) return -1
  const factor = direction === 'ascending' ? 1 : -1
  if (typeof a === 'number' && typeof b === 'number') {
    return factor * (a - b)
  }
  return factor * collator.compare(String(a), String(b))
}

export function Table<T>({
  columns,
  rows,
  rowKey,
  empty,
  maxHeight = 'max-h-96',
  caption,
  defaultSort,
  highlightedRowKey,
}: {
  columns: Column<T>[]
  rows: T[]
  rowKey: (row: T) => string
  empty: ReactNode
  maxHeight?: string
  caption?: string
  defaultSort?: { key: string; direction: SortDirection }
  highlightedRowKey?: string | null
}) {
  const [sortState, setSortState] = useState<{ key: string; direction: SortDirection } | null>(
    defaultSort ?? null,
  )
  const [announcement, setAnnouncement] = useState('')

  const sortedRows = useMemo(() => {
    if (!sortState) return rows
    const col = columns.find((c) => c.key === sortState.key)
    if (!col?.sortValue) return rows
    const getter = col.sortValue
    return [...rows].sort((ra, rb) => compareValues(getter(ra), getter(rb), sortState.direction))
  }, [rows, columns, sortState])

  if (rows.length === 0) return <Empty>{empty}</Empty>

  const handleSort = (col: Column<T>) => {
    if (!col.sortValue) return
    const nextDir: SortDirection =
      sortState?.key === col.key
        ? sortState.direction === 'ascending'
          ? 'descending'
          : 'ascending'
        : 'descending'
    setSortState({ key: col.key, direction: nextDir })
    setAnnouncement(
      `已按 ${col.label} ${nextDir === 'ascending' ? '升序' : '降序'} 排列，共 ${rows.length} 行`,
    )
  }

  return (
    <div className="relative">
      <div role="status" className="sr-status">
        {announcement}
      </div>
      <div
        tabIndex={0}
        role="region"
        aria-label={caption ?? '数据表'}
        className={`table-scroll ${maxHeight}`}
      >
        <table className="w-full min-w-max border-collapse text-[13px]">
          {caption && <caption className="sr-status">{caption}</caption>}
          <thead className="sticky top-0 z-10 bg-panel">
            <tr className="text-[11px] uppercase tracking-wider text-muted">
              {columns.map((c) => {
                const isSorted = sortState?.key === c.key
                const alignCls = c.align === 'right' ? 'text-right' : 'text-left'
                if (!c.sortValue) {
                  return (
                    <th
                      key={c.key}
                      scope="col"
                      className={`border-b border-line px-3 py-2 font-medium whitespace-nowrap ${alignCls}`}
                    >
                      {c.label}
                    </th>
                  )
                }
                return (
                  <th
                    key={c.key}
                    scope="col"
                    aria-sort={isSorted ? sortState.direction : undefined}
                    className={`sortable-col border-b border-line font-medium whitespace-nowrap ${alignCls}`}
                  >
                    <button type="button" onClick={() => handleSort(c)}>
                      <span>{c.label}</span>
                      <span aria-hidden="true" className="text-[10px] opacity-75">
                        {isSorted ? (sortState.direction === 'ascending' ? '↑' : '↓') : '↕'}
                      </span>
                    </button>
                  </th>
                )
              })}
            </tr>
          </thead>
          <tbody>
            {sortedRows.map((row) => {
              const key = rowKey(row)
              const active = highlightedRowKey != null && key === highlightedRowKey
              return (
                <tr
                  key={key}
                  className={`border-b border-line/50 transition-colors ${
                    active ? 'bg-accent/10 hover:bg-accent/15' : 'hover:bg-panel-2'
                  }`}
                >
                  {columns.map((c) => (
                    <td
                      key={c.key}
                      className={`px-3 py-1.5 whitespace-nowrap ${c.align === 'right' ? 'num text-right' : ''}`}
                    >
                      {c.render(row)}
                    </td>
                  ))}
                </tr>
              )
            })}
          </tbody>
        </table>
      </div>
    </div>
  )
}
