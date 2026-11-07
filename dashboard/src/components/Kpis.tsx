import { useState, type ReactNode } from 'react'
import type { SummaryResp, Window } from '../types'
import { bps, compactUsd, num, pct, price, signedUsd, size, tone, toneClass, usd } from '../format'
import { Card } from './ui'

function Stat({ label, value, sub, valueClass = '' }: { label: string; value: ReactNode; sub?: ReactNode; valueClass?: string }) {
  return (
    <div className="card min-w-0 px-4 py-3">
      <div className="text-[11px] uppercase tracking-wider text-muted">{label}</div>
      <div className={`num mt-1 truncate text-xl font-semibold ${valueClass}`}>{value}</div>
      {sub && <div className="num mt-0.5 truncate text-xs text-muted">{sub}</div>}
    </div>
  )
}

export function Headline({ summary }: { summary: SummaryResp | undefined }) {
  const s = summary?.session
  const snap = summary?.snapshot
  const position = snap?.exchange_position ?? s?.position
  return (
    <div className="grid grid-cols-2 gap-3 md:grid-cols-3 xl:grid-cols-6">
      <Stat
        label="账户权益"
        value={usd(snap?.equity)}
        sub={<>可用 {usd(snap?.available_balance)} · 保证金 {pct(snap?.margin_usage)}</>}
      />
      <Stat
        label="会话总盈亏"
        value={signedUsd(s?.total_pnl)}
        valueClass={toneClass[tone(s?.total_pnl)]}
        sub={<>已实现 {signedUsd(s?.net_pnl)} · 浮动 {signedUsd(s?.unrealized)}</>}
      />
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
        sub={<>均价 {price(s?.avg_cost)} · 标记 {price(snap?.mid)}</>}
      />
      <Stat
        label="会话成交额"
        value={compactUsd(s?.volume)}
        sub={<>{num(s?.fills, 0)} 笔 · 买 {compactUsd(s?.buy_volume)} / 卖 {compactUsd(s?.sell_volume)}</>}
      />
      <Stat
        label="挂单 / 价差"
        value={<>{num(snap?.active_orders ?? s?.open_orders, 0)}<span className="text-sm text-muted"> 单</span></>}
        sub={<>报价价差 {bps(snap?.spread_bps)}</>}
      />
    </div>
  )
}

const WINDOWS = [
  ['today', '今日'],
  ['24h', '24 小时'],
  ['7d', '7 天'],
  ['all', '全部'],
] as const

function WindowRow({ label, value, valueClass = '' }: { label: string; value: ReactNode; valueClass?: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3 py-1">
      <span className="text-xs text-muted">{label}</span>
      <span className={`num text-sm ${valueClass}`}>{value}</span>
    </div>
  )
}

export function Performance({ summary }: { summary: SummaryResp | undefined }) {
  const [key, setKey] = useState<(typeof WINDOWS)[number][0]>('today')
  const w: Window | undefined = summary?.windows[key]
  return (
    <Card
      title="交易绩效"
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
        <WindowRow label="保证金占用" value={pct(snap?.margin_usage)} valueClass={warn((snap?.margin_usage ?? 0) > 0.5)} />
        <WindowRow label="下单 / 拒单" value={`${num(snap?.orders_placed, 0)} / ${num(snap?.orders_rejected, 0)}`} valueClass={warn((snap?.orders_rejected ?? 0) > 0)} />
        <WindowRow label="私有流中断" value={num(s?.stream_lost, 0)} valueClass={warn((s?.stream_lost ?? 0) > 0)} />
        <WindowRow label="遥测丢弃" value={num(snap?.dropped, 0)} valueClass={warn((snap?.dropped ?? 0) > 0)} />
        <WindowRow label="账户 / 网络" value={s ? `#${s.account} · ${s.network ?? '—'}` : '—'} />
      </div>
    </Card>
  )
}
