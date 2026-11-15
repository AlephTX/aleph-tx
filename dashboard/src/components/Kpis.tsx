import { useState, type ReactNode } from 'react'
import {
  dailyLossBudget,
  markoutSummary,
  sessionEfficiency,
  type GuardSeverity,
} from '../analytics'
import { useNow } from '../clock'
import { bps, compactUsd, num, pct, price, signedUsd, size, tone, toneClass, usd } from '../format'
import type { SummaryResp, Window } from '../types'
import { sessionValuation, type QualityBadge } from '../valuation'
import { Badge, type BadgeTone, Card } from './ui'

function Stat({
  label,
  value,
  sub,
  valueClass = '',
  badge,
}: {
  label: string
  value: ReactNode
  sub?: ReactNode
  valueClass?: string
  badge?: QualityBadge | null
}) {
  return (
    <div className="card min-w-0 px-4 py-3">
      <div className="flex items-center justify-between gap-2 text-[11px] uppercase tracking-wider text-muted">
        <span className="truncate">{label}</span>
        {badge && <Badge tone={badge.tone}>{badge.label}</Badge>}
      </div>
      <div className={`num mt-1 truncate text-xl font-semibold ${valueClass}`}>{value}</div>
      {sub && <div className="num mt-0.5 truncate text-xs text-muted">{sub}</div>}
    </div>
  )
}

export function Headline({ summary }: { summary: SummaryResp | undefined }) {
  const s = summary?.session
  const snap = summary?.snapshot
  const position = snap?.exchange_position ?? s?.position
  const valuation = sessionValuation(s)
  return (
    <div className="space-y-2">
      <div className="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-6">
        <Stat
          label="账户权益"
          value={usd(snap?.equity)}
          sub={
            <>
              可用 {usd(snap?.available_balance)} · 保证金 {pct(snap?.margin_usage)}
            </>
          }
        />
        <Stat
          label="Lighter 会话盈亏"
          value={signedUsd(s?.total_pnl)}
          valueClass={toneClass[tone(s?.total_pnl)]}
          badge={valuation?.lighter}
          sub={
            <>
              已实现 {signedUsd(s?.net_pnl)} · 浮动 {signedUsd(s?.unrealized)}
            </>
          }
        />
        {s?.hedge_account && (
          <Stat
            label="组合盈亏（含对冲）"
            value={signedUsd(s?.combined_pnl)}
            valueClass={toneClass[tone(s?.combined_pnl)]}
            badge={valuation?.combined}
            sub={
              <>
                对冲 {signedUsd(s?.hedge_total_pnl)} · {num(s?.hedge_fills, 0)} 笔 · 持仓{' '}
                {size(s?.hedge_position)}
              </>
            }
          />
        )}
        <Stat
          label="权益变化"
          value={signedUsd(s?.equity_change)}
          valueClass={toneClass[tone(s?.equity_change)]}
          sub={<>起始 {usd(snap?.session_start_equity)}</>}
        />
        <Stat
          label={`持仓 ${s?.symbol ?? ''}`}
          value={size(position)}
          valueClass={toneClass[tone(position)]}
          sub={
            <>
              均价 {price(s?.avg_cost)} · 标记 {price(s?.valuation?.mark.price ?? snap?.mid)}
            </>
          }
        />
        <Stat
          label="会话成交额"
          value={compactUsd(s?.volume)}
          sub={
            <>
              {num(s?.fills, 0)} 笔 · 买 {compactUsd(s?.buy_volume)} / 卖 {compactUsd(s?.sell_volume)}
            </>
          }
        />
        <Stat
          label="挂单 / 价差"
          value={
            <>
              {num(snap?.active_orders ?? s?.open_orders, 0)}
              <span className="text-sm text-muted"> 单</span>
            </>
          }
          sub={<>报价价差 {bps(snap?.spread_bps)}</>}
        />
      </div>
      {valuation && (
        <div className="text-xs text-muted" data-testid="valuation-notes">
          {valuation.mark && <>{valuation.mark}。</>}
          盈亏口径：{valuation.notes.join(' · ')}
        </div>
      )}
    </div>
  )
}

function severityTone(sev: GuardSeverity): BadgeTone {
  switch (sev) {
    case 'ok':
      return 'ok'
    case 'warn':
      return 'warn'
    case 'critical':
      return 'bad'
    default:
      return 'idle'
  }
}

function WindowRow({
  label,
  value,
  valueClass = '',
}: {
  label: string
  value: ReactNode
  valueClass?: string
}) {
  return (
    <div className="flex items-baseline justify-between gap-3 py-1">
      <span className="text-xs text-muted">{label}</span>
      <span className={`num text-sm ${valueClass}`}>{value}</span>
    </div>
  )
}

export function DailyLossGuardCard({ summary }: { summary: SummaryResp | undefined }) {
  const now = useNow()
  const s = summary?.session
  const budget = dailyLossBudget(summary?.snapshot, now, Boolean(s?.stopped_ms))
  const barPct =
    budget.usedFraction !== null ? Math.min(100, Math.max(0, budget.usedFraction * 100)) : 0
  const barColor =
    budget.severity === 'critical'
      ? 'bg-down'
      : budget.severity === 'warn'
        ? 'bg-warn'
        : 'bg-up'

  const stateBadge: { tone: BadgeTone; text: string } =
    budget.state === 'unknown'
      ? { tone: 'bad', text: '基线未知' }
      : budget.state === 'historical_day'
        ? { tone: 'idle', text: `历史快照 ${budget.utcDayLabel ?? ''}` }
        : budget.severity === 'critical'
          ? { tone: 'bad', text: '逼近熔断 (>=90%)' }
          : budget.severity === 'warn'
            ? { tone: 'warn', text: '预警 (>=60%)' }
            : { tone: severityTone(budget.severity), text: `生效中 ${budget.utcDayLabel ?? ''}` }

  return (
    <Card
      title="UTC 日亏熔断护栏"
      right={
        <Badge tone={stateBadge.tone} dot={budget.state === 'active_utc_day'}>
          {stateBadge.text}
        </Badge>
      }
    >
      <div className="space-y-2 px-4 py-3">
        <div className="flex items-baseline justify-between gap-2">
          <span className="text-xs text-muted">已用风险预算</span>
          <span className="num text-base font-semibold">
            {budget.usedFraction !== null ? pct(budget.usedFraction, 1) : '—'}
          </span>
        </div>
        <div className="h-1.5 w-full overflow-hidden rounded-full bg-bg">
          {budget.usedFraction !== null && (
            <div
              className={`h-full transition-all duration-300 ${barColor}`}
              style={{ width: `${barPct}%` }}
            />
          )}
        </div>
        <div className="grid grid-cols-2 gap-x-5 pt-1">
          <WindowRow label="UTC 起始权益" value={usd(budget.startEquity)} />
          <WindowRow
            label="日亏阈值"
            value={
              budget.limitPct !== null && budget.budgetUsd !== null
                ? `${pct(budget.limitPct, 2)} (${usd(budget.budgetUsd)})`
                : '—'
            }
          />
          <WindowRow
            label="当日回撤"
            value={
              budget.drawdownUsd !== null && budget.drawdownPct !== null
                ? `${usd(budget.drawdownUsd)} (${pct(budget.drawdownPct, 2)})`
                : '—'
            }
            valueClass={(budget.drawdownUsd ?? 0) > 0 ? 'text-down' : ''}
          />
          <WindowRow
            label="剩余缓冲"
            value={usd(budget.remainingUsd)}
            valueClass={(budget.remainingUsd ?? 1) <= 0 ? 'text-down' : 'text-up'}
          />
        </div>
      </div>
    </Card>
  )
}

export function MarkoutQualityCard({ summary }: { summary: SummaryResp | undefined }) {
  const snap = summary?.snapshot
  const m = markoutSummary(snap)
  return (
    <Card
      title="成交后 Markout 与逆向选择"
      right={
        m.warm ? (
          <Badge tone={(m.ewmaBps ?? 0) >= 0 ? 'ok' : (m.ewmaBps ?? 0) > -1.0 ? 'warn' : 'bad'}>
            EWMA 已预热
          </Badge>
        ) : (
          <Badge tone="idle">预热中 / 未知</Badge>
        )
      }
    >
      <div className="grid grid-cols-2 gap-x-5 px-4 py-3">
        <WindowRow
          label="Markout EWMA"
          value={bps(m.ewmaBps)}
          valueClass={toneClass[tone(m.ewmaBps)]}
        />
        <WindowRow
          label="逆向选择分"
          value={num(snap?.adverse_selection, 3)}
          valueClass={(snap?.adverse_selection ?? 0) > 0.6 ? 'text-warn' : ''}
        />
        <WindowRow
          label="样本覆盖率"
          value={m.coverage !== null ? `${pct(m.coverage, 1)} (${m.marked}/${m.totalTracked})` : '—'}
        />
        <WindowRow
          label="末次评估延迟"
          value={m.lastDelayMs !== null ? `${num(m.lastDelayMs, 0)} ms` : '—'}
        />
        <WindowRow
          label="已标记 / 过期"
          value={
            m.marked !== null && m.expired !== null
              ? `${num(m.marked, 0)} / ${num(m.expired, 0)}`
              : '—'
          }
        />
        <WindowRow
          label="覆盖重写 / 缺失"
          value={
            m.overwritten !== null && m.missed !== null
              ? `${num(m.overwritten, 0)} / ${num(m.missed, 0)}`
              : '—'
          }
          valueClass={(m.missed ?? 0) > 0 ? 'text-warn' : ''}
        />
      </div>
    </Card>
  )
}

export function SessionEfficiencyCard({ summary }: { summary: SummaryResp | undefined }) {
  const s = summary?.session
  const eff = sessionEfficiency(s)
  const cost = eff?.costUsdPerMillionGross ?? null
  // Negative cost per $1M means net realized profit!
  const costClass =
    cost === null ? '' : cost <= 0 ? 'text-up' : cost <= 2.0 ? 'text-accent' : 'text-warn'

  return (
    <Card
      title="单会话单位成本与敞口效率"
      right={eff ? <Badge tone={eff.archetype.tone}>{eff.archetype.shortLabel}</Badge> : null}
    >
      <div className="grid grid-cols-2 gap-x-5 px-4 py-3">
        <WindowRow label="双腿总成交额" value={compactUsd(eff?.grossVolume)} />
        <WindowRow
          label="已实现合并净利"
          value={signedUsd(eff?.realizedCombinedNet)}
          valueClass={toneClass[tone(eff?.realizedCombinedNet)]}
        />
        <WindowRow
          label="每 $1M 净损耗成本"
          value={cost !== null ? `${signedUsd(cost, 2)} / $1M` : '—'}
          valueClass={costClass}
        />
        <WindowRow
          label="已实现净收益率"
          value={bps(eff?.combinedRealizedBps, 3)}
          valueClass={toneClass[tone(eff?.combinedRealizedBps)]}
        />
        <WindowRow
          label="每 $1 亏损撬动量"
          value={
            eff?.volumePerDollarLoss != null
              ? compactUsd(eff.volumePerDollarLoss)
              : eff && eff.grossVolume > 0 && eff.realizedCombinedNet >= 0
                ? '净盈利 / 零损耗'
                : '—'
          }
          valueClass={eff && eff.grossVolume > 0 && eff.realizedCombinedNet >= 0 ? 'text-up' : ''}
        />
        <WindowRow
          label="双腿净敞口"
          value={eff ? size(eff.netExposure) : '—'}
          valueClass={eff && !eff.isFlat ? 'text-warn' : ''}
        />
      </div>
    </Card>
  )
}

const WINDOWS = [
  ['today', '今日'],
  ['24h', '24 小时'],
  ['7d', '7 天'],
  ['all', '全部'],
] as const

export function Performance({ summary }: { summary: SummaryResp | undefined }) {
  const [key, setKey] = useState<(typeof WINDOWS)[number][0]>('today')
  const w: Window | undefined = summary?.windows[key]
  return (
    <Card
      title="Lighter 交易绩效"
      right={
        <div className="flex rounded-md bg-bg p-0.5 text-xs">
          {WINDOWS.map(([k, label]) => (
            <button
              key={k}
              type="button"
              onClick={() => setKey(k)}
              className={`rounded px-2 py-0.5 ${k === key ? 'bg-panel-2 text-text' : 'text-muted hover:text-text'}`}
            >
              {label}
            </button>
          ))}
        </div>
      }
    >
      <div className="grid grid-cols-1 gap-x-6 px-4 py-2 sm:grid-cols-2">
        <WindowRow label="净盈亏" value={signedUsd(w?.net_pnl)} valueClass={toneClass[tone(w?.net_pnl)]} />
        <WindowRow label="净收益率" value={bps(w?.net_bps)} valueClass={toneClass[tone(w?.net_bps)]} />
        <WindowRow label="已实现" value={signedUsd(w?.realized)} valueClass={toneClass[tone(w?.realized)]} />
        <WindowRow label="胜率" value={pct(w?.win_rate)} />
        <WindowRow label="手续费" value={usd(w?.fees)} valueClass={(w?.fees ?? 0) < 0 ? 'text-up' : ''} />
        <WindowRow label="Maker 返佣" value={usd(w?.rebates)} valueClass="text-up" />
        <WindowRow label="成交额" value={compactUsd(w?.volume)} />
        <WindowRow label="成交笔数" value={num(w?.fills, 0)} />
        <WindowRow label="买入额" value={compactUsd(w?.buy_volume)} />
        <WindowRow label="卖出额" value={compactUsd(w?.sell_volume)} />
        <WindowRow label="盈利平仓" value={num(w?.winning, 0)} valueClass="text-up" />
        <WindowRow label="亏损平仓" value={num(w?.losing, 0)} valueClass="text-down" />
      </div>
    </Card>
  )
}

export function Risk({ summary }: { summary: SummaryResp | undefined }) {
  const snap = summary?.snapshot
  const s = summary?.session
  const warn = (cond: boolean) => (cond ? 'text-warn' : '')
  return (
    <Card title="风险与执行">
      <div className="grid grid-cols-1 gap-x-6 px-4 py-2 sm:grid-cols-2">
        <WindowRow label="交易所持仓" value={size(snap?.exchange_position)} />
        <WindowRow label="已确认持仓" value={size(snap?.confirmed_position)} />
        <WindowRow label="报价用持仓" value={size(snap?.quote_position)} />
        <WindowRow label="在途敞口" value={size(snap?.pending_exposure)} />
        <WindowRow label="最坏多头" value={size(snap?.worst_case_long)} />
        <WindowRow label="最坏空头" value={size(snap?.worst_case_short)} />
        <WindowRow label="逆向选择分" value={num(snap?.adverse_selection, 3)} />
        <WindowRow
          label="保证金占用"
          value={pct(snap?.margin_usage)}
          valueClass={warn((snap?.margin_usage ?? 0) > 0.5)}
        />
        <WindowRow
          label="下单 / 拒单"
          value={`${num(snap?.orders_placed, 0)} / ${num(snap?.orders_rejected, 0)}`}
          valueClass={warn((snap?.orders_rejected ?? 0) > 0)}
        />
        <WindowRow
          label="私有流中断"
          value={num(s?.stream_lost, 0)}
          valueClass={warn((s?.stream_lost ?? 0) > 0)}
        />
        <WindowRow
          label="遥测丢弃"
          value={num(snap?.dropped, 0)}
          valueClass={warn((snap?.dropped ?? 0) > 0)}
        />
        <WindowRow label="账户 / 网络" value={s ? `#${s.account} · ${s.network ?? '—'}` : '—'} />
      </div>
    </Card>
  )
}
