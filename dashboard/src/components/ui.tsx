import type { ReactNode } from 'react'
import { tone, toneClass } from '../format'

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
        <header className="flex items-center justify-between gap-3 border-b border-line px-4 py-2.5">
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

export interface Column<T> {
  key: string
  label: ReactNode
  align?: 'left' | 'right'
  render: (row: T) => ReactNode
}

export function Table<T>({
  columns,
  rows,
  rowKey,
  empty,
  maxHeight = 'max-h-96',
}: {
  columns: Column<T>[]
  rows: T[]
  rowKey: (row: T) => string
  empty: ReactNode
  maxHeight?: string
}) {
  if (rows.length === 0) return <Empty>{empty}</Empty>
  return (
    <div className={`${maxHeight} overflow-auto`}>
      <table className="w-full border-collapse text-[13px]">
        <thead className="sticky top-0 z-10 bg-panel">
          <tr className="text-[11px] uppercase tracking-wider text-muted">
            {columns.map((c) => (
              <th
                key={c.key}
                className={`border-b border-line px-3 py-2 font-medium whitespace-nowrap ${c.align === 'right' ? 'text-right' : 'text-left'}`}
              >
                {c.label}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((row) => (
            <tr key={rowKey(row)} className="border-b border-line/50 hover:bg-panel-2">
              {columns.map((c) => (
                <td
                  key={c.key}
                  className={`px-3 py-1.5 whitespace-nowrap ${c.align === 'right' ? 'num text-right' : ''}`}
                >
                  {c.render(row)}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}
