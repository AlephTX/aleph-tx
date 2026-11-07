import type { Service, StatusResp } from '../types'
import { ago, duration } from '../format'
import { Badge, type BadgeTone } from './ui'
import { StopButton } from './StopButton'
import { useNow } from '../clock'

const LABELS: Record<string, string> = {
  'aleph-tx-feeder': '行情 Feeder',
  'aleph-tx-monitor': '监控',
  'aleph-tx-live': '实盘策略',
}

function serviceTone(s: Service): BadgeTone {
  switch (s.ActiveState) {
    case 'active':
      return 'ok'
    case 'failed':
      return 'bad'
    case 'activating':
    case 'deactivating':
    case 'reloading':
      return 'warn'
    default:
      return 'idle'
  }
}

export function StatusBar({
  status,
  error,
  updatedAt,
  sessionStarted,
  sessionStopped,
}: {
  status: StatusResp | undefined
  error: unknown
  updatedAt: number
  sessionStarted: number | null
  sessionStopped: number | null
}) {
  const now = useNow()
  const live = status?.services['aleph-tx-live']
  const liveActive = live?.ActiveState === 'active'
  const stale = updatedAt > 0 && now - updatedAt > 15_000
  const link: { tone: BadgeTone; text: string } = error
    ? { tone: 'bad', text: '连接中断' }
    : !status
      ? { tone: 'idle', text: '连接中' }
      : stale
        ? { tone: 'warn', text: '数据延迟' }
        : { tone: 'ok', text: '已连接' }

  return (
    <header className="sticky top-0 z-30 border-b border-line bg-bg/85 backdrop-blur">
      <div className="mx-auto flex max-w-[1680px] flex-wrap items-center gap-x-5 gap-y-2 px-4 py-2.5">
        <div className="flex items-center gap-2">
          <span className="grid size-7 place-items-center rounded-md bg-accent/15 font-mono text-base font-bold text-accent">
            α
          </span>
          <div className="leading-tight">
            <div className="text-sm font-semibold">AlephTX</div>
            <div className="text-[11px] text-muted">{status?.host ?? '—'}</div>
          </div>
        </div>
        <Badge tone={link.tone} dot>
          {link.text}
        </Badge>
        <div className="flex flex-wrap items-center gap-2">
          {status &&
            Object.entries(status.services).map(([name, s]) => (
              <span key={name} title={`${s.ActiveState}/${s.SubState ?? ''} restarts=${s.NRestarts ?? 0}`}>
                <Badge tone={serviceTone(s)} dot={s.ActiveState === 'active'}>
                  {LABELS[name] ?? name} · {s.SubState ?? s.ActiveState}
                </Badge>
              </span>
            ))}
        </div>
        <div className="text-xs text-muted">
          {sessionStarted
            ? sessionStopped
              ? `会话已结束 ${ago(sessionStopped, now)}`
              : `会话运行 ${duration(now - sessionStarted)}`
            : '暂无会话'}
        </div>
        <div className="ml-auto flex items-center gap-4">
          <div className="hidden text-right text-[11px] leading-tight text-muted sm:block">
            <div className="num">release {status?.release?.id ?? '—'}</div>
            <div>更新 {updatedAt ? ago(updatedAt, now) : '—'}</div>
          </div>
          {status?.control_enabled && <StopButton liveActive={liveActive} />}
        </div>
      </div>
    </header>
  )
}
