import type { AccountSnapshot, Fill, Market, Order, Position, Session } from '../types'
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
  usd,
} from '../format'
import { Badge, Card, ErrorNote, Side, Signed, Table, type Column } from './ui'
import { useNow } from '../clock'
import { feeSourceBadge } from '../valuation'

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
  { key: 'price', label: '价格', align: 'right', render: (o) => price(o.price) },
  { key: 'size', label: '数量', align: 'right', render: (o) => size(o.size) },
  { key: 'filled', label: '已成交', align: 'right', render: (o) => size(o.filled) },
  { key: 'notional', label: '名义', align: 'right', render: (o) => usd(o.price * o.size) },
  { key: 'status', label: '状态', render: (o) => ORDER_STATUS[o.status] ?? o.status },
  { key: 'age', label: '挂单时长', align: 'right', render: (o) => <Elapsed since={o.created_ms} /> },
  { key: 'coid', label: 'COID', align: 'right', render: (o) => <span className="text-muted">{o.client_order_id}</span> },
]

export function OpenOrders({ orders, error }: { orders: Order[] | undefined; error: unknown }) {
  return (
    <Card title={`当前挂单 · ${orders?.length ?? 0}`}>
      <ErrorNote error={error} />
      <Table
        columns={orderCols}
        rows={orders ?? []}
        rowKey={(o) => `${o.session_id}:${o.client_order_id}`}
        empty="无挂单"
        maxHeight="max-h-80"
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

const fillCols: Column<Fill>[] = [
  { key: 't', label: '时间', render: (f) => <span className="num text-muted">{time(f.ts_ms)}</span> },
  { key: 'side', label: '方向', render: (f) => <Side side={f.side} /> },
  { key: 'price', label: '价格', align: 'right', render: (f) => price(f.price) },
  { key: 'size', label: '数量', align: 'right', render: (f) => size(f.size) },
  { key: 'notional', label: '成交额', align: 'right', render: (f) => usd(f.notional) },
  {
    key: 'fee',
    label: '手续费',
    align: 'right',
    render: (f) => <FillFee fill={f} />,
  },
  {
    key: 'rpnl',
    label: '已实现',
    align: 'right',
    render: (f) => <Signed value={f.realized_delta}>{signedUsd(f.realized_delta, 4)}</Signed>,
  },
  { key: 'pos', label: '成交后持仓', align: 'right', render: (f) => size(f.position_after) },
  {
    key: 'cum',
    label: '会话净盈亏',
    align: 'right',
    render: (f) => <Signed value={f.net_pnl_after}>{signedUsd(f.net_pnl_after)}</Signed>,
  },
]

export function RecentFills({ fills, error }: { fills: Fill[] | undefined; error: unknown }) {
  return (
    <Card title="最近成交">
      <ErrorNote error={error} />
      <Table
        columns={fillCols}
        rows={fills ?? []}
        rowKey={(f) => `${f.session_id}:${f.offset}`}
        empty="暂无成交"
        maxHeight="max-h-[28rem]"
      />
    </Card>
  )
}

const marketCols: Column<Market>[] = [
  { key: 'venue', label: '交易所', render: (m) => m.venue },
  { key: 'symbol', label: '品种', render: (m) => m.symbol },
  { key: 'bid', label: '买一', align: 'right', render: (m) => <span className="text-up">{price(m.bid)}</span> },
  { key: 'ask', label: '卖一', align: 'right', render: (m) => <span className="text-down">{price(m.ask)}</span> },
  { key: 'spread', label: '价差', align: 'right', render: (m) => bps(m.spread_bps) },
  {
    key: 'state',
    label: '状态',
    render: (m) => (
      <Badge tone={m.state === 'live' ? 'ok' : m.state === 'stale' ? 'warn' : 'idle'}>
        {m.state === 'live' ? '实时' : m.state === 'stale' ? '过期' : '等待'}
        {m.age_ms != null && ` ${num(m.age_ms / 1000, 1)}s`}
      </Badge>
    ),
  },
]

export function Markets({ markets }: { markets: Market[] | undefined }) {
  return (
    <Card title="行情 (SHM)">
      <Table columns={marketCols} rows={markets ?? []} rowKey={(m) => `${m.venue}:${m.symbol}`} empty="等待行情" />
    </Card>
  )
}

const positionCols: Column<Position & { venue: string }>[] = [
  { key: 'venue', label: '账户', render: (p) => p.venue },
  { key: 'symbol', label: '品种', render: (p) => p.symbol },
  { key: 'size', label: '数量', align: 'right', render: (p) => <Signed value={parse(p.size)}>{p.size}</Signed> },
  { key: 'entry', label: '开仓价', align: 'right', render: (p) => price(parse(p.entry)) },
  {
    key: 'upnl',
    label: '浮动盈亏',
    align: 'right',
    render: (p) => <Signed value={parse(p.unrealized_pnl)}>{signedUsd(parse(p.unrealized_pnl))}</Signed>,
  },
  { key: 'liq', label: '强平价', align: 'right', render: (p) => price(parse(p.liquidation)) },
]

export function Accounts({ accounts }: { accounts: Record<string, AccountSnapshot> | undefined }) {
  const entries = Object.entries(accounts ?? {})
  const rows = entries.flatMap(([venue, a]) => (a.positions ?? []).map((p) => ({ ...p, venue })))
  return (
    <Card
      title="交易所持仓"
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
      <Table columns={positionCols} rows={rows} rowKey={(p) => `${p.venue}:${p.symbol}`} empty="无持仓" />
    </Card>
  )
}

const sessionCols: Column<Session>[] = [
  { key: 'start', label: '开始', render: (s) => <span className="num">{dateTime(s.started_ms)}</span> },
  {
    key: 'dur',
    label: '时长',
    align: 'right',
    render: (s) => duration((s.stopped_ms ?? s.last_ms) - s.started_ms),
  },
  {
    key: 'state',
    label: '状态',
    render: (s) => <SessionState session={s} />,
  },
  { key: 'mkt', label: '市场', render: (s) => `${s.exchange ?? '—'} ${s.symbol ?? ''}` },
  { key: 'net', label: '净盈亏', align: 'right', render: (s) => <Signed value={s.net_pnl}>{signedUsd(s.net_pnl)}</Signed> },
  { key: 'vol', label: '成交额', align: 'right', render: (s) => compactUsd(s.volume) },
  { key: 'fills', label: '笔数', align: 'right', render: (s) => num(s.fills, 0) },
  { key: 'fees', label: '手续费', align: 'right', render: (s) => usd(s.fees, 4) },
  { key: 'pos', label: '末持仓', align: 'right', render: (s) => size(s.position) },
  {
    key: 'issues',
    label: '异常',
    align: 'right',
    render: (s) => (
      <span className={s.stream_lost + s.rejected > 0 ? 'text-warn' : 'text-muted'}>
        断流 {s.stream_lost} · 拒单 {s.rejected}
      </span>
    ),
  },
]

export function Sessions({ sessions, error }: { sessions: Session[] | undefined; error: unknown }) {
  return (
    <Card title="交易会话">
      <ErrorNote error={error} />
      <Table columns={sessionCols} rows={sessions ?? []} rowKey={(s) => s.session_id} empty="暂无会话记录" />
    </Card>
  )
}
