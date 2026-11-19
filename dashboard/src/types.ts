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
  alerts?: Alert[]
}

export interface Alert {
  key: string
  severity: 'critical' | 'warning'
  message: string
  since_ms: number
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
  /** null when an open position has no valid mark. */
  unrealized: number | null
  net_pnl: number
  /** null when an open position has no valid mark. */
  total_pnl: number | null
  equity_change: number | null
  open_orders: number
  hedge_position: number
  hedge_avg_cost: number
  hedge_realized: number
  hedge_fees: number
  hedge_volume: number
  hedge_fills: number
  /** null when an open position has no valid mark. */
  hedge_unrealized: number | null
  hedge_net_pnl: number
  /** null when an open position has no valid mark. */
  hedge_total_pnl: number | null
  /** null when an open position has no valid mark. */
  combined_pnl: number | null
  /** Absent on servers older than the cost provenance contract. */
  valuation?: Valuation
}

/** reported: venue-reported · estimated: limit price, configured or declared
 * fee rate, or mark · unknown: legacy record without provenance, or a Lighter
 * fill without a usable fee rate · missing: no mark for an open position ·
 * flat / none: nothing to value. */
export type Quality = 'reported' | 'estimated' | 'unknown' | 'missing' | 'flat' | 'none'

export interface LegValuation {
  realized: Quality
  fees: Quality
  unrealized: Quality
  quality: Quality
}

export interface HedgeValuation extends LegValuation {
  price_sources: { fill: number; limit: number; unknown: number }
  fee_sources: { configured_rate: number; unknown: number }
}

/** How each Lighter fill's fee was derived (see LighterFeeSource). */
export interface LighterValuation extends LegValuation {
  fee_sources?: Record<LighterFeeSource | 'unknown', number>
}

/** rate_derived / explicit_zero: the trade declared a rate for the account's
 * role (an explicit 0 is a known zero) · missing: absent or null, which
 * Lighter also sends for zero, so not a known zero · invalid: not an integer
 * ppm rate · role_unknown: the maker side was guessed. null: older writer. */
export type LighterFeeSource = 'rate_derived' | 'explicit_zero' | 'missing' | 'invalid' | 'role_unknown'

/** Hedge fill price provenance from telemetry_store.py (HEDGE_PRICE_SOURCES). */
export type HedgePriceSource = 'fill' | 'limit'

/** Hedge fill fee provenance from telemetry_store.py (HEDGE_FEE_SOURCES). */
export type HedgeFeeSource = 'configured_rate'

/** observed: feed <= sample <= now holds · historical: same, for the last
 * mark of a stopped session · unknown: a sample or feed time is missing
 * (legacy) · clock_mismatch: the known times break that order, so ages are
 * null · none: no mark. */
export type Freshness = 'observed' | 'historical' | 'unknown' | 'clock_mismatch' | 'none'

/** Epoch ms from three clocks: feed_ms from the feeder's SHM timestamp
 * (mostly venue time), sampled_ms from the strategy host, ages against the
 * monitor host at summary time. Ages are null when a time is missing or the
 * clocks contradict each other; skew that keeps their order is undetectable,
 * so an age is a best estimate, not proof the hosts are synchronized. */
export interface Mark {
  price: number | null
  /** external_median · lighter_{book,depth,touch_micro} · local_{book,depth,touch_micro} · unknown (legacy). */
  source: string | null
  /** When the snapshot record was written; not when the price was observed. */
  snapshot_ms: number | null
  /** When the strategy accepted this price sample. */
  sampled_ms: number | null
  /** Oldest feed timestamp behind the sample (venue time for most venues). */
  feed_ms: number | null
  sample_age_ms: number | null
  feed_age_ms: number | null
  freshness: Freshness
  scope: string
}

export interface Valuation {
  mark: Mark
  lighter: LighterValuation
  hedge: HedgeValuation
  combined_quality: Quality
  missing: string[]
  excluded: string[]
  scope: string
}

export interface Snapshot {
  ts_ms?: number
  equity: number
  session_start_equity: number
  daily_loss_day_start_ms?: number | null
  daily_loss_start_equity?: number | null
  daily_loss_limit_pct?: number | null
  available_balance: number
  margin_usage: number
  exchange_position: number
  quote_position: number
  confirmed_position: number
  pending_exposure: number
  worst_case_long: number
  worst_case_short: number
  mid: number
  mark_source?: string | null
  mark_sampled_ms?: number | null
  mark_feed_ms?: number | null
  spread_bps: number
  adverse_selection: number
  trade_cash_flow: number
  total_fees: number
  fill_count: number
  orders_placed: number
  orders_rejected: number
  active_orders: number
  dropped: number
  /** Post-fill markout diagnostics; null on legacy snapshots or before warm-up. */
  markout_marked?: number | null
  markout_expired?: number | null
  markout_overwritten?: number | null
  markout_missed?: number | null
  markout_last_delay_ms?: number | null
  markout_ewma_bps?: number | null
  volume_quota_remaining?: number | null
  defense_regime?: string | null
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
  /** Absent from older servers; null for older writers. Both are unknown. */
  fee_source?: LighterFeeSource | null
}

export interface HedgeFill {
  session_id: string
  offset: number
  ts_ms: number
  venue: string
  client_order_id: string
  side: 'buy' | 'sell'
  price: number
  /** 'fill' = venue fill report; 'limit' = IOC limit-price fallback; null = legacy unknown. */
  price_source?: HedgePriceSource | null
  size: number
  notional: number
  fee: number
  realized_delta: number
  position_after: number
  /** 'configured_rate' = estimated from configured taker bps; null = legacy unknown. */
  fee_source?: HedgeFeeSource | null
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
