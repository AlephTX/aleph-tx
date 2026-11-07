// Aligned with monitor.py and telemetry_store.py responses.

export interface Service {
  ActiveState: string
  SubState?: string
  MainPID?: string
  NRestarts?: string
  Result?: string
}

export interface Market {
  venue: string
  symbol: string
  state: string
  bid?: number
  ask?: number
  mid?: number
  spread_bps?: number
  age_ms?: number
}

export interface Position {
  symbol: string
  size: string
  entry: string | null
  unrealized_pnl: string
  liquidation: string | null
}

export interface AccountSnapshot {
  state: 'ready' | 'error' | 'waiting' | 'unconfigured'
  positions?: Position[]
  as_of?: string
  error?: string
}

export interface StatusResp {
  host: string
  checked_at: string
  release: { id: string; source_sha256?: string; base_commit?: string } | null
  services: Record<string, Service>
  markets: Market[]
  accounts: Record<string, AccountSnapshot>
  control_enabled: boolean
}

export interface Session {
  session_id: string
  network: string
  exchange: string
  symbol: string
  market_id: number
  account: number
  hedge_account: string | null
  started_ms: number
  last_ms: number
  stopped_ms: number | null
  position: number
  avg_cost: number
  realized: number
  fees: number
  volume: number
  fills: number
  buy_volume: number
  sell_volume: number
  stream_lost: number
  rejected: number
  unrealized: number
  net_pnl: number
  total_pnl: number
  equity_change: number | null
  open_orders: number
}

export interface Snapshot {
  equity: number
  session_start_equity: number
  available_balance: number
  margin_usage: number
  exchange_position: number
  quote_position: number
  confirmed_position: number
  pending_exposure: number
  worst_case_long: number
  worst_case_short: number
  mid: number
  spread_bps: number
  adverse_selection: number
  trade_cash_flow: number
  total_fees: number
  fill_count: number
  orders_placed: number
  orders_rejected: number
  active_orders: number
  dropped: number
}

export interface Window {
  fills: number
  volume: number
  realized: number
  fees: number
  rebates: number
  buy_volume: number
  sell_volume: number
  winning: number
  losing: number
  net_pnl: number
  win_rate: number | null
  net_bps: number | null
}

export interface SummaryResp {
  session: Session | null
  snapshot: Snapshot | null
  windows: { today: Window; '24h': Window; '7d': Window; all: Window }
}

export interface Fill {
  session_id: string
  offset: number
  ts_ms: number
  exchange_ts_ns: number | null
  client_order_id: number
  side: 'buy' | 'sell'
  price: number
  size: number
  notional: number
  fee: number
  trade_id: number
  realized_delta: number
  position_after: number
  avg_cost_after: number
  net_pnl_after: number
}

export interface Order {
  session_id: string
  client_order_id: number
  order_index: number | null
  side: 'buy' | 'sell' | null
  price: number
  size: number
  status: string
  filled: number
  fill_notional: number
  fee: number
  created_ms: number
  updated_ms: number
}

export interface PnLBucket {
  t: number
  net: number
  volume: number
  fills: number
  cumulative: number
}

export interface EquityBucket {
  t: number
  last_ts: number
  equity: number
  position: number
  mid: number
}

export interface PnLResp {
  bucket: string
  pnl: PnLBucket[]
  equity: EquityBucket[]
}

export interface LogLine {
  ts_us: number
  level: string
  message: string
}

export interface StopResp {
  ok: boolean
  requested_at: string
}
