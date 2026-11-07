import { useMemo, useState } from 'react'
import { useLogs } from '../queries'
import { Card, Empty, ErrorNote } from './ui'

const UNITS = [
  ['aleph-tx-live', '实盘策略'],
  ['aleph-tx-feeder', 'Feeder'],
  ['aleph-tx-monitor', '监控'],
] as const

const LEVEL_CLASS: Record<string, string> = {
  emerg: 'text-down',
  alert: 'text-down',
  crit: 'text-down',
  error: 'text-down',
  warn: 'text-warn',
  notice: 'text-accent',
}

export function Logs() {
  const [open, setOpen] = useState(false)
  const [unit, setUnit] = useState<string>(UNITS[0][0])
  const [filter, setFilter] = useState('')
  const { data, error, isFetching } = useLogs(unit, open)
  const lines = useMemo(() => {
    const needle = filter.trim().toLowerCase()
    const all = data?.logs ?? []
    return (needle ? all.filter((l) => l.message.toLowerCase().includes(needle)) : all).toReversed()
  }, [data, filter])

  return (
    <Card
      title={
        <button type="button" onClick={() => setOpen((v) => !v)} className="hover:text-text">
          {open ? '▾' : '▸'} 服务日志 {isFetching && open ? '·' : ''}
        </button>
      }
      right={
        open && (
          <div className="flex items-center gap-2">
            <input
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder="过滤…"
              className="w-40 rounded border border-line bg-bg px-2 py-0.5 text-xs outline-none focus:border-accent"
            />
            <div className="flex rounded-md bg-bg p-0.5 text-xs">
              {UNITS.map(([u, label]) => (
                <button
                  key={u}
                  type="button"
                  onClick={() => setUnit(u)}
                  className={`rounded px-2 py-0.5 ${u === unit ? 'bg-panel-2 text-text' : 'text-muted hover:text-text'}`}
                >
                  {label}
                </button>
              ))}
            </div>
          </div>
        )
      }
    >
      {open && (
        <>
          <ErrorNote error={error} />
          {lines.length === 0 ? (
            <Empty>{data ? '无日志' : '加载中…'}</Empty>
          ) : (
            <pre className="num max-h-[32rem] overflow-auto px-4 py-2 text-[11.5px] leading-5">
              {lines.map((l, i) => (
                <div key={`${l.ts_us}:${i}`} className={LEVEL_CLASS[l.level] ?? 'text-text/85'}>
                  <span className="text-muted">{new Date(l.ts_us / 1000).toLocaleTimeString('zh-CN', { hour12: false })}</span>{' '}
                  {l.message}
                </div>
              ))}
            </pre>
          )}
        </>
      )}
    </Card>
  )
}
