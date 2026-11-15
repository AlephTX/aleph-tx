import { useMemo, useState } from 'react'
import {
  classifySession,
  crossVenueSpreads,
  sessionEfficiency,
  type SessionArchetype,
} from '../analytics'
import { useNow } from '../clock'
import {
  ago,
  bps,
  compactUsd,
  dateTime,
  duration,
  num,
  parse,
  price,
  signedUsd,
  size,
  time,
  tone,
  toneClass,
  usd,
} from '../format'
import { useOrders } from '../queries'
import type {
  AccountSnapshot,
  Fill,
  HedgeFill,
  Market,
  Order,
  Position,
  Session,
} from '../types'
import { feeSourceBadge, hedgeFeeSourceBadge, hedgePriceSourceBadge } from '../valuation'
import { Badge, Card, ErrorNote, Side, Signed, Table, type Column } from './ui'

type SideFilter = 'all' | 'buy' | 'sell'

const SIDE_FILTERS: readonly [SideFilter, string][] = [
  ['all', '全部'],
  ['buy', '买'],
  ['sell', '卖'],
]

function Segmented<K extends string>({
  options,
  value,
  onChange,
}: {
  options: readonly [K, string][]
  value: K
  onChange: (next: K) => void
}) {
  return (
    <div className="flex rounded-md bg-bg p-0.5 text-xs">
      {options.map(([k, label]) => (
        <button
          key={k}
          type="button"
          onClick={() => onChange(k)}
          className={`rounded px-2 py-0.5 ${k === value ? 'bg-panel-2 text-text' : 'text-muted hover:text-text'}`}
        >
          {label}
        </button>
      ))}
    </div>
  )
}

function Elapsed({ since }: { since: number }) {
  return <>{duration(useNow() - since)}</>
}

function SessionState({ session: s }: { session: Session }) {
  const now = useNow()
  if (s.stopped_ms) return <Badge tone="idle">已停止</Badge>
  // Snapshots arrive every housekeeping cycle; two minutes of silence means trouble.
  if (now - s.last_ms < 120_000)
    return (
      <Badge tone="ok" dot>
        运行中
      </Badge>
    )
  return <Badge tone="warn">无心跳 {ago(s.last_ms, now)}</Badge>
}

const ORDER_STATUS: Record<string, string> = {
  open: '挂单',
  partial: '部分成交',
  filled: '已成交',
  canceled: '已撤',
  rejected: '拒绝',
}

const orderCols: Column<Order>[] = [
  { key: 'side', label: '方向', render: (o) => <Side side={o.side} /> },
  {
    key: 'price',
    label: '价格',
    align: 'right',
    sortValue: (o) => o.price,
    render: (o) => price(o.price),
  },
  {
    key: 'size',
    label: '数量',
    align: 'right',
    sortValue: (o) => o.size,
    render: (o) => size(o.size),
  },
  {
    key: 'filled',
    label: '已成交',
    align: 'right',
    sortValue: (o) => o.filled,
    render: (o) => size(o.filled),
  },
  {
    key: 'notional',
    label: '名义',
    align: 'right',
    sortValue: (o) => o.price * o.size,
    render: (o) => usd(o.price * o.size),
  },
  { key: 'status', label: '状态', render: (o) => ORDER_STATUS[o.status] ?? o.status },
  {
    key: 'age',
    label: '委托时间',
    align: 'right',
    sortValue: (o) => o.created_ms,
    render: (o) =>
      o.status === 'open' || o.status === 'partial' ? (
        <Elapsed since={o.created_ms} />
      ) : (
        <span className="num text-muted">{time(o.updated_ms)}</span>
      ),
  },
  {
    key: 'coid',
    label: 'COID',
    align: 'right',
    sortValue: (o) => o.client_order_id,
    render: (o) => <span className="text-muted">{o.client_order_id}</span>,
  },
]

export function OpenOrders({ orders, error }: { orders: Order[] | undefined; error: unknown }) {
  return (
    <Card title={`当前挂单 · ${orders?.length ?? 0}`}>
      <ErrorNote error={error} />
      <Table
        caption="当前挂单列表"
        columns={orderCols}
        rows={orders ?? []}
        rowKey={(o) => `${o.session_id}:${o.client_order_id}`}
        empty="无挂单"
        maxHeight="max-h-80"
      />
    </Card>
  )
}

export function OrdersPanel({ sessionId }: { sessionId?: string }) {
  const [status, setStatus] = useState<'open' | 'all'>('open')
  const [side, setSide] = useState<SideFilter>('all')
  const { data, error } = useOrders(status, 200, sessionId)
  const rows = useMemo(() => {
    const all = data?.orders ?? []
    return side === 'all' ? all : all.filter((o) => o.side === side)
  }, [data, side])

  return (
    <Card
      title={`${status === 'open' ? '当前挂单' : '会话委托记录'} · ${rows.length}`}
      right={
        <div className="flex flex-wrap items-center gap-2">
          <Segmented
            options={[
              ['open', '仅挂单'],
              ['all', '全部状态'],
            ]}
            value={status}
            onChange={setStatus}
          />
          <Segmented options={SIDE_FILTERS} value={side} onChange={setSide} />
        </div>
      }
    >
      <ErrorNote error={error} />
      <Table
        caption="委托订单表"
        columns={orderCols}
        rows={rows}
        rowKey={(o) => `${o.session_id}:${o.client_order_id}`}
        empty={status === 'open' ? '无在途挂单' : '暂无委托记录'}
        maxHeight="max-h-96"
      />
    </Card>
  )
}

/** The booked fee with its provenance: a missing rate reads $0 like an
 * explicit zero, so only the badge tells them apart. */
export function FillFee({ fill: f }: { fill: Fill }) {
  const badge = feeSourceBadge(f.fee_source)
  return (
    <span className="inline-flex items-center justify-end gap-1.5">
      {badge && <Badge tone={badge.tone}>{badge.label}</Badge>}
      <span className={f.fee < 0 ? 'text-up' : ''}>{usd(f.fee, 4)}</span>
    </span>
  )
}

export function HedgeFillPrice({ fill: f }: { fill: HedgeFill }) {
  const badge = hedgePriceSourceBadge(f.price_source)
  return (
    <span className="inline-flex items-center justify-end gap-1.5">
      {badge && <Badge tone={badge.tone}>{badge.label}</Badge>}
      <span>{price(f.price)}</span>
    </span>
  )
}

export function HedgeFillFee({ fill: f }: { fill: HedgeFill }) {
  const badge = hedgeFeeSourceBadge(f.fee_source)
  return (
    <span className="inline-flex items-center justify-end gap-1.5">
      {badge && <Badge tone={badge.tone}>{badge.label}</Badge>}
      <span>{usd(f.fee, 4)}</span>
    </span>
  )
}

const fillCols: Column<Fill>[] = [
  {
    key: 't',
    label: '时间',
    sortValue: (f) => f.ts_ms,
    render: (f) => <span className="num text-muted">{time(f.ts_ms)}</span>,
  },
  { key: 'side', label: '方向', render: (f) => <Side side={f.side} /> },
  {
    key: 'price',
    label: '价格',
    align: 'right',
    sortValue: (f) => f.price,
    render: (f) => price(f.price),
  },
  {
    key: 'size',
    label: '数量',
    align: 'right',
    sortValue: (f) => f.size,
    render: (f) => size(f.size),
  },
  {
    key: 'notional',
    label: '成交额',
    align: 'right',
    sortValue: (f) => f.notional,
    render: (f) => usd(f.notional),
  },
  {
    key: 'fee',
    label: '手续费',
    align: 'right',
    sortValue: (f) => f.fee,
    render: (f) => <FillFee fill={f} />,
  },
  {
    key: 'rpnl',
    label: '已实现',
    align: 'right',
    sortValue: (f) => f.realized_delta,
    render: (f) => <Signed value={f.realized_delta}>{signedUsd(f.realized_delta, 4)}</Signed>,
  },
  { key: 'pos', label: '成交后持仓', align: 'right', render: (f) => size(f.position_after) },
  {
    key: 'cum',
    label: '会话净盈亏',
    align: 'right',
    sortValue: (f) => f.net_pnl_after,
    render: (f) => <Signed value={f.net_pnl_after}>{signedUsd(f.net_pnl_after)}</Signed>,
  },
]

export function RecentFills({ fills, error }: { fills: Fill[] | undefined; error: unknown }) {
  const [side, setSide] = useState<SideFilter>('all')
  const rows = useMemo(() => {
    const all = fills ?? []
    return side === 'all' ? all : all.filter((f) => f.side === side)
  }, [fills, side])

  return (
    <Card
      title={`Lighter 成交明细 · ${rows.length}`}
      right={<Segmented options={SIDE_FILTERS} value={side} onChange={setSide} />}
    >
      <ErrorNote error={error} />
      <Table
        caption="Lighter 成交明细"
        columns={fillCols}
        rows={rows}
        rowKey={(f) => `${f.session_id}:${f.offset}`}
        empty="暂无成交"
        maxHeight="max-h-[28rem]"
      />
    </Card>
  )
}

const hedgeFillCols: Column<HedgeFill>[] = [
  {
    key: 't',
    label: '时间',
    sortValue: (f) => f.ts_ms,
    render: (f) => <span className="num text-muted">{time(f.ts_ms)}</span>,
  },
  { key: 'venue', label: '对冲腿', render: (f) => f.venue },
  { key: 'side', label: '方向', render: (f) => <Side side={f.side} /> },
  {
    key: 'price',
    label: '成交 / 估算价',
    align: 'right',
    sortValue: (f) => f.price,
    render: (f) => <HedgeFillPrice fill={f} />,
  },
  {
    key: 'size',
    label: '数量',
    align: 'right',
    sortValue: (f) => f.size,
    render: (f) => size(f.size),
  },
  {
    key: 'notional',
    label: '成交额',
    align: 'right',
    sortValue: (f) => f.notional,
    render: (f) => usd(f.notional),
  },
  {
    key: 'fee',
    label: '估算手续费',
    align: 'right',
    sortValue: (f) => f.fee,
    render: (f) => <HedgeFillFee fill={f} />,
  },
  {
    key: 'rpnl',
    label: '已实现变动',
    align: 'right',
    sortValue: (f) => f.realized_delta,
    render: (f) => <Signed value={f.realized_delta}>{signedUsd(f.realized_delta, 4)}</Signed>,
  },
  { key: 'pos', label: '对冲腿持仓', align: 'right', render: (f) => size(f.position_after) },
  {
    key: 'cloid',
    label: 'CLOID',
    render: (f) => (
      <span className="num text-xs text-muted" title={f.client_order_id}>
        {f.client_order_id.length > 14 ? `${f.client_order_id.slice(0, 12)}…` : f.client_order_id}
      </span>
    ),
  },
]

export function HedgeFills({
  fills,
  error,
  hedgeAccount,
}: {
  fills: HedgeFill[] | undefined
  error: unknown
  hedgeAccount?: string | null
}) {
  const [side, setSide] = useState<SideFilter>('all')
  const rows = useMemo(() => {
    const all = fills ?? []
    return side === 'all' ? all : all.filter((f) => f.side === side)
  }, [fills, side])

  const shortAcct =
    hedgeAccount && hedgeAccount.length > 14
      ? `${hedgeAccount.slice(0, 6)}…${hedgeAccount.slice(-4)}`
      : hedgeAccount

  return (
    <Card
      title={`对冲腿成交 (IOC) · ${rows.length}${shortAcct ? ` · ${shortAcct}` : ''}`}
      right={<Segmented options={SIDE_FILTERS} value={side} onChange={setSide} />}
    >
      <ErrorNote error={error} />
      <Table
        caption="对冲腿成交明细"
        columns={hedgeFillCols}
        rows={rows}
        rowKey={(f) => `${f.session_id}:${f.offset}`}
        empty={hedgeAccount ? '该会话暂无对冲成交' : '当前会话未启用跨所对冲腿'}
        maxHeight="max-h-[28rem]"
      />
    </Card>
  )
}

const marketCols: Column<Market>[] = [
  { key: 'venue', label: '交易所', sortValue: (m) => m.venue, render: (m) => m.venue },
  { key: 'symbol', label: '品种', sortValue: (m) => m.symbol, render: (m) => m.symbol },
  {
    key: 'bid',
    label: '买一',
    align: 'right',
    sortValue: (m) => m.bid,
    render: (m) => <span className="text-up">{price(m.bid)}</span>,
  },
  {
    key: 'ask',
    label: '卖一',
    align: 'right',
    sortValue: (m) => m.ask,
    render: (m) => <span className="text-down">{price(m.ask)}</span>,
  },
  {
    key: 'spread',
    label: '价差',
    align: 'right',
    sortValue: (m) => m.spread_bps,
    render: (m) => bps(m.spread_bps),
  },
  {
    key: 'state',
    label: '状态',
    sortValue: (m) => m.age_ms ?? Number.MAX_SAFE_INTEGER,
    render: (m) => (
      <Badge tone={m.state === 'live' ? 'ok' : m.state === 'stale' ? 'warn' : 'idle'}>
        {m.state === 'live' ? '实时' : m.state === 'stale' ? '过期' : '等待'}
        {m.age_ms != null && ` ${num(m.age_ms / 1000, 1)}s`}
      </Badge>
    ),
  },
]

export function Markets({ markets }: { markets: Market[] | undefined }) {
  const basisRows = useMemo(() => crossVenueSpreads(markets), [markets])
  return (
    <Card title="共享内存实时盘口矩阵 (SHM: Lighter / Hyperliquid / Binance)">
      <Table
        caption="SHM 实时盘口矩阵"
        columns={marketCols}
        rows={markets ?? []}
        rowKey={(m) => `${m.venue}:${m.symbol}`}
        empty="等待行情"
      />
      {basisRows.length > 0 && (
        <div className="flex flex-wrap items-center gap-x-6 gap-y-1.5 border-t border-line px-4 py-2 text-xs">
          <span className="font-medium text-muted">跨所实时参考基差 (vs Lighter)：</span>
          {basisRows.map((row) => (
            <span key={row.symbol} className="num inline-flex items-center gap-2">
              <span className="font-semibold text-text">{row.symbol}</span>
              <span className="text-muted">HL</span>
              <span className={toneClass[tone(row.hlVsLighterBps)]}>{bps(row.hlVsLighterBps)}</span>
              <span className="text-muted">· Binance</span>
              <span className={toneClass[tone(row.binanceVsLighterBps)]}>
                {bps(row.binanceVsLighterBps)}
              </span>
            </span>
          ))}
        </div>
      )}
    </Card>
  )
}

const positionCols: Column<Position & { venue: string }>[] = [
  { key: 'venue', label: '账户', render: (p) => p.venue },
  { key: 'symbol', label: '品种', render: (p) => p.symbol },
  {
    key: 'size',
    label: '数量',
    align: 'right',
    sortValue: (p) => parse(p.size),
    render: (p) => <Signed value={parse(p.size)}>{p.size}</Signed>,
  },
  {
    key: 'entry',
    label: '开仓价',
    align: 'right',
    sortValue: (p) => parse(p.entry),
    render: (p) => price(parse(p.entry)),
  },
  {
    key: 'upnl',
    label: '浮动盈亏',
    align: 'right',
    sortValue: (p) => parse(p.unrealized_pnl),
    render: (p) => <Signed value={parse(p.unrealized_pnl)}>{signedUsd(parse(p.unrealized_pnl))}</Signed>,
  },
  {
    key: 'liq',
    label: '强平价',
    align: 'right',
    sortValue: (p) => parse(p.liquidation),
    render: (p) => price(parse(p.liquidation)),
  },
]

export function Accounts({ accounts }: { accounts: Record<string, AccountSnapshot> | undefined }) {
  const entries = Object.entries(accounts ?? {})
  const rows = entries.flatMap(([venue, a]) => (a.positions ?? []).map((p) => ({ ...p, venue })))
  return (
    <Card
      title="交易所链上实时持仓"
      right={
        <div className="flex gap-1.5">
          {entries.map(([venue, a]) => (
            <span key={venue} title={a.error ?? a.as_of ?? ''}>
              <Badge tone={a.state === 'ready' ? 'ok' : a.state === 'error' ? 'bad' : 'idle'}>{venue}</Badge>
            </span>
          ))}
        </div>
      }
    >
      <Table
        caption="交易所链上实时持仓"
        columns={positionCols}
        rows={rows}
        rowKey={(p) => `${p.venue}:${p.symbol}`}
        empty="无持仓"
      />
    </Card>
  )
}

type ArchetypeFilter = 'all' | SessionArchetype
type RunStateFilter = 'all' | 'active' | 'stopped'

export function Sessions({
  sessions,
  error,
  selectedSessionId,
  onSelectSession,
}: {
  sessions: Session[] | undefined
  error: unknown
  selectedSessionId?: string
  onSelectSession?: (sessionId: string | undefined) => void
}) {
  const [archFilter, setArchFilter] = useState<ArchetypeFilter>('all')
  const [stateFilter, setStateFilter] = useState<RunStateFilter>('all')

  const filtered = useMemo(() => {
    return (sessions ?? []).filter((s) => {
      if (archFilter !== 'all' && classifySession(s).kind !== archFilter) return false
      if (stateFilter === 'active' && s.stopped_ms !== null) return false
      if (stateFilter === 'stopped' && s.stopped_ms === null) return false
      return true
    })
  }, [sessions, archFilter, stateFilter])

  const sessionCols = useMemo<Column<Session>[]>(
    () => [
      {
        key: 'start',
        label: '开始时间',
        sortValue: (s) => s.started_ms,
        render: (s) => <span className="num">{dateTime(s.started_ms)}</span>,
      },
      {
        key: 'dur',
        label: '时长',
        align: 'right',
        sortValue: (s) => (s.stopped_ms ?? s.last_ms) - s.started_ms,
        render: (s) => duration((s.stopped_ms ?? s.last_ms) - s.started_ms),
      },
      {
        key: 'state',
        label: '状态',
        render: (s) => <SessionState session={s} />,
      },
      {
        key: 'mode',
        label: '结构模式',
        sortValue: (s) => classifySession(s).shortLabel,
        render: (s) => {
          const meta = classifySession(s)
          return <Badge tone={meta.tone}>{meta.shortLabel}</Badge>
        },
      },
      {
        key: 'mkt',
        label: '市场 / 账户',
        sortValue: (s) => `${s.exchange ?? ''} ${s.symbol ?? ''}`,
        render: (s) => (
          <span>
            {s.exchange ?? '—'} {s.symbol ?? ''}{' '}
            <span className="num text-xs text-muted">#{s.account}</span>
          </span>
        ),
      },
      {
        key: 'realized_net',
        label: '已实现合并净利',
        align: 'right',
        sortValue: (s) => sessionEfficiency(s)?.realizedCombinedNet,
        render: (s) => {
          const v = sessionEfficiency(s)?.realizedCombinedNet ?? null
          return <Signed value={v}>{signedUsd(v)}</Signed>
        },
      },
      {
        key: 'marked_total',
        label: '含浮盈总计',
        align: 'right',
        sortValue: (s) => sessionEfficiency(s)?.markedCombinedPnl,
        render: (s) => {
          const v = sessionEfficiency(s)?.markedCombinedPnl ?? null
          return <Signed value={v}>{signedUsd(v)}</Signed>
        },
      },
      {
        key: 'gross_vol',
        label: '双腿总成交',
        align: 'right',
        sortValue: (s) => sessionEfficiency(s)?.grossVolume,
        render: (s) => compactUsd(sessionEfficiency(s)?.grossVolume),
      },
      {
        key: 'cost_1m',
        label: '每 $1M 净成本',
        align: 'right',
        sortValue: (s) => sessionEfficiency(s)?.costUsdPerMillionGross,
        render: (s) => {
          const c = sessionEfficiency(s)?.costUsdPerMillionGross ?? null
          if (c === null) return <span className="text-muted">—</span>
          const cls = c <= 0 ? 'text-up' : c <= 2.0 ? 'text-accent' : 'text-warn'
          return <span className={`num ${cls}`}>{signedUsd(c, 2)}</span>
        },
      },
      {
        key: 'fills',
        label: '双腿笔数',
        align: 'right',
        sortValue: (s) => (s.fills || 0) + (s.hedge_fills || 0),
        render: (s) =>
          s.hedge_account ? `${num(s.fills, 0)} + ${num(s.hedge_fills, 0)}` : num(s.fills, 0),
      },
      {
        key: 'pos',
        label: '净敞口',
        align: 'right',
        sortValue: (s) => Math.abs(sessionEfficiency(s)?.netExposure ?? 0),
        render: (s) => size(sessionEfficiency(s)?.netExposure),
      },
      {
        key: 'issues',
        label: '异常',
        align: 'right',
        sortValue: (s) => s.stream_lost + s.rejected,
        render: (s) => (
          <span className={s.stream_lost + s.rejected > 0 ? 'text-warn' : 'text-muted'}>
            断流 {s.stream_lost} · 拒单 {s.rejected}
          </span>
        ),
      },
      ...(onSelectSession
        ? [
            {
              key: 'inspect',
              label: '追踪',
              align: 'right' as const,
              render: (s: Session) => {
                const pinned = selectedSessionId === s.session_id
                return (
                  <button
                    type="button"
                    onClick={() => onSelectSession(pinned ? undefined : s.session_id)}
                    className={`rounded px-2 py-0.5 text-xs font-medium transition-colors ${
                      pinned
                        ? 'bg-accent text-bg'
                        : 'bg-panel-2 text-accent ring-1 ring-inset ring-accent/30 hover:bg-accent/15'
                    }`}
                  >
                    {pinned ? '取消固定' : '查看详情'}
                  </button>
                )
              },
            },
          ]
        : []),
    ],
    [onSelectSession, selectedSessionId],
  )

  return (
    <Card
      title={`独立会话矩阵与单位成本审计 · ${filtered.length} / ${sessions?.length ?? 0}`}
      right={
        <div className="flex flex-wrap items-center gap-2">
          <Segmented<ArchetypeFilter>
            options={[
              ['all', '全部结构'],
              ['hedged', '双腿对冲'],
              ['single_leg', '单腿做市'],
            ]}
            value={archFilter}
            onChange={setArchFilter}
          />
          <Segmented<RunStateFilter>
            options={[
              ['all', '全部状态'],
              ['active', '运行中'],
              ['stopped', '已停止'],
            ]}
            value={stateFilter}
            onChange={setStateFilter}
          />
        </div>
      }
    >
      <ErrorNote error={error} />
      <Table
        caption="交易会话矩阵"
        columns={sessionCols}
        rows={filtered}
        rowKey={(s) => s.session_id}
        highlightedRowKey={selectedSessionId ?? null}
        empty="暂无匹配会话记录"
        maxHeight="max-h-[32rem]"
      />
      <div className="border-t border-line px-4 py-2 text-xs text-muted">
        独立核算原则：每个会话单独计算已实现净利与每 $1M 双腿成交净成本（负值代表净盈利），跨会话预算绝不混同，积分价值固定按 $0 计。
      </div>
    </Card>
  )
}
