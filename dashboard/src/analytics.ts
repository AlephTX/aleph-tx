// Pure strategy telemetry analytics aligned with deploy/oasis/alerts.py and
// deploy/oasis/telemetry_store.py. Unknown telemetry stays null/unknown and is
// never coerced to zero; each session is evaluated independently.

import type { BadgeTone } from './components/ui'
import type { Market, Session, Snapshot } from './types'

const EPS = 1e-12
const DAY_MS = 86_400_000
export const DAILY_LOSS_WARN_FRACTION = 0.6
export const DAILY_LOSS_CRITICAL_FRACTION = 0.9

export type SessionArchetype = 'hedged' | 'single_leg'

export interface ArchetypeMeta {
  kind: SessionArchetype
  label: string
  shortLabel: string
  tone: BadgeTone
}

/** Structural execution mode observed in session telemetry (never guesses
 * unrecorded off-ledger intent and never merges budgets across sessions). */
export function classifySession(s: Pick<Session, 'hedge_account'> | null | undefined): ArchetypeMeta {
  if (s?.hedge_account) {
    return {
      kind: 'hedged',
      label: '双腿对冲 (Maker + IOC Hedge)',
      shortLabel: '双腿对冲',
      tone: 'info',
    }
  }
  return {
    kind: 'single_leg',
    label: '单腿做市 / 量化刷量',
    shortLabel: '单腿做市',
    tone: 'idle',
  }
}

export interface SessionEfficiency {
  archetype: ArchetypeMeta
  lighterVolume: number
  hedgeVolume: number
  grossVolume: number
  totalFills: number
  /** Realized cash PnL net of booked/estimated fees (excludes unrealized & funding). */
  realizedCombinedNet: number
  /** Marked total PnL across both legs; null when an open leg lacks a valid mark. */
  markedCombinedPnl: number | null
  /** Lighter net realized return in bps of Lighter turnover; null when volume == 0. */
  lighterRealizedBps: number | null
  /** Combined realized return in bps of gross 2-leg turnover; null when grossVolume == 0. */
  combinedRealizedBps: number | null
  /** Realized net cost in USD per $1M gross turnover (<0 means net realized profit). */
  costUsdPerMillionGross: number | null
  /** Gross turnover achieved per $1 of realized loss; null when profitable or flat. */
  volumePerDollarLoss: number | null
  /** Signed base-asset net exposure (Lighter + hedge when hedged). */
  netExposure: number
  isFlat: boolean
}

export function sessionEfficiency(s: Session | null | undefined): SessionEfficiency | null {
  if (!s) return null
  const lighterVolume = Math.max(0, s.volume || 0)
  const hedgeVolume = Math.max(0, s.hedge_volume || 0)
  const grossVolume = lighterVolume + hedgeVolume
  const totalFills = (s.fills || 0) + (s.hedge_fills || 0)
  const realizedCombinedNet = s.net_pnl + (s.hedge_account ? s.hedge_net_pnl : 0)
  const markedCombinedPnl = s.hedge_account ? s.combined_pnl : s.total_pnl
  const lighterRealizedBps = lighterVolume > 0 ? (s.net_pnl / lighterVolume) * 10_000 : null
  const combinedRealizedBps = grossVolume > 0 ? (realizedCombinedNet / grossVolume) * 10_000 : null
  const costUsdPerMillionGross =
    grossVolume > 0 ? (-realizedCombinedNet / grossVolume) * 1_000_000 : null
  const volumePerDollarLoss =
    realizedCombinedNet < -EPS && grossVolume > 0
      ? grossVolume / Math.abs(realizedCombinedNet)
      : null
  const netExposure = s.position + (s.hedge_account ? s.hedge_position : 0)
  const isFlat = Math.abs(s.position) <= EPS && Math.abs(s.hedge_position) <= EPS
  return {
    archetype: classifySession(s),
    lighterVolume,
    hedgeVolume,
    grossVolume,
    totalFills,
    realizedCombinedNet,
    markedCombinedPnl,
    lighterRealizedBps,
    combinedRealizedBps,
    costUsdPerMillionGross,
    volumePerDollarLoss,
    netExposure,
    isFlat,
  }
}

export type DailyLossState = 'active_utc_day' | 'historical_day' | 'unknown'
export type GuardSeverity = 'ok' | 'warn' | 'critical' | 'unknown'

export interface DailyLossBudget {
  state: DailyLossState
  severity: GuardSeverity
  utcDayLabel: string | null
  startEquity: number | null
  currentEquity: number | null
  limitPct: number | null
  budgetUsd: number | null
  drawdownUsd: number | null
  drawdownPct: number | null
  remainingUsd: number | null
  usedFraction: number | null
}

function formatUtcDay(dayStartMs: number): string {
  const d = new Date(dayStartMs)
  const yyyy = d.getUTCFullYear()
  const mm = String(d.getUTCMonth() + 1).padStart(2, '0')
  const dd = String(d.getUTCDate()).padStart(2, '0')
  return `${yyyy}-${mm}-${dd} UTC`
}

/** Evaluates the persisted UTC daily loss guard from strategy snapshots,
 * preserving legacy/unreported baselines as unknown (never 0). */
export function dailyLossBudget(
  snap: Snapshot | null | undefined,
  nowMs: number,
  sessionStopped = false,
): DailyLossBudget {
  const dayStart = snap?.daily_loss_day_start_ms
  const start = snap?.daily_loss_start_equity
  const limit = snap?.daily_loss_limit_pct
  const equity = snap?.equity

  const hasValidBaseline =
    typeof dayStart === 'number' &&
    Number.isFinite(dayStart) &&
    dayStart > 0 &&
    typeof start === 'number' &&
    Number.isFinite(start) &&
    start > 0 &&
    typeof limit === 'number' &&
    Number.isFinite(limit) &&
    limit > 0 &&
    limit < 1 &&
    typeof equity === 'number' &&
    Number.isFinite(equity) &&
    equity > 0

  if (!hasValidBaseline) {
    return {
      state: 'unknown',
      severity: 'unknown',
      utcDayLabel: null,
      startEquity: null,
      currentEquity: typeof equity === 'number' && equity > 0 ? equity : null,
      limitPct: null,
      budgetUsd: null,
      drawdownUsd: null,
      drawdownPct: null,
      remainingUsd: null,
      usedFraction: null,
    }
  }

  const currentUtcDay = nowMs - (nowMs % DAY_MS)
  const state: DailyLossState =
    !sessionStopped && dayStart === currentUtcDay ? 'active_utc_day' : 'historical_day'
  const budgetUsd = start * limit
  const rawLoss = start - equity
  const drawdownUsd = Math.max(0, rawLoss)
  const drawdownPct = drawdownUsd / start
  const remainingUsd = equity - (start - budgetUsd)
  const usedFraction = budgetUsd > 0 ? Math.max(0, rawLoss / budgetUsd) : null

  let severity: GuardSeverity = 'ok'
  if (usedFraction !== null) {
    if (usedFraction >= DAILY_LOSS_CRITICAL_FRACTION) severity = 'critical'
    else if (usedFraction >= DAILY_LOSS_WARN_FRACTION) severity = 'warn'
  }

  return {
    state,
    severity,
    utcDayLabel: formatUtcDay(dayStart),
    startEquity: start,
    currentEquity: equity,
    limitPct: limit,
    budgetUsd,
    drawdownUsd,
    drawdownPct,
    remainingUsd,
    usedFraction,
  }
}

export interface MarkoutSummary {
  /** True only when markout_ewma_bps is a finite reported number. */
  warm: boolean
  ewmaBps: number | null
  lastDelayMs: number | null
  marked: number | null
  expired: number | null
  overwritten: number | null
  missed: number | null
  totalTracked: number | null
  coverage: number | null
}

/** Summarizes post-fill markout telemetry without turning null/legacy fields into 0. */
export function markoutSummary(snap: Snapshot | null | undefined): MarkoutSummary {
  if (!snap) {
    return {
      warm: false,
      ewmaBps: null,
      lastDelayMs: null,
      marked: null,
      expired: null,
      overwritten: null,
      missed: null,
      totalTracked: null,
      coverage: null,
    }
  }
  const numOrNull = (v: number | null | undefined): number | null =>
    typeof v === 'number' && Number.isFinite(v) ? v : null

  const ewmaBps = numOrNull(snap.markout_ewma_bps)
  const lastDelayMs = numOrNull(snap.markout_last_delay_ms)
  const marked = numOrNull(snap.markout_marked)
  const expired = numOrNull(snap.markout_expired)
  const overwritten = numOrNull(snap.markout_overwritten)
  const missed = numOrNull(snap.markout_missed)

  const countsPresent = marked !== null && expired !== null && overwritten !== null && missed !== null
  const totalTracked = countsPresent ? marked + expired + overwritten + missed : null
  const coverage = totalTracked !== null && totalTracked > 0 && marked !== null ? marked / totalTracked : null

  return {
    warm: ewmaBps !== null,
    ewmaBps,
    lastDelayMs,
    marked,
    expired,
    overwritten,
    missed,
    totalTracked,
    coverage,
  }
}

export interface CrossVenueSpreadRow {
  symbol: string
  lighterMid: number | null
  hyperliquidMid: number | null
  binanceMid: number | null
  hlVsLighterBps: number | null
  binanceVsLighterBps: number | null
}

/** Computes live cross-venue mid basis (in bps vs Lighter) across the 6 SHM slots
 * when quotes are live; stale or waiting feeds remain null rather than stale math. */
export function crossVenueSpreads(markets: Market[] | undefined): CrossVenueSpreadRow[] {
  if (!markets || markets.length === 0) return []
  const symbols = Array.from(new Set(markets.map((m) => m.symbol)))
  const liveMid = (venue: string, symbol: string): number | null => {
    const m = markets.find((row) => row.venue === venue && row.symbol === symbol)
    if (!m || m.state !== 'live' || typeof m.mid !== 'number' || !Number.isFinite(m.mid) || m.mid <= 0) {
      return null
    }
    return m.mid
  }
  const relBps = (ref: number | null, base: number | null): number | null =>
    ref !== null && base !== null && base > 0 ? ((ref - base) / base) * 10_000 : null

  return symbols.map((symbol) => {
    const lighterMid = liveMid('Lighter', symbol)
    const hyperliquidMid = liveMid('Hyperliquid', symbol)
    const binanceMid = liveMid('Binance', symbol)
    return {
      symbol,
      lighterMid,
      hyperliquidMid,
      binanceMid,
      hlVsLighterBps: relBps(hyperliquidMid, lighterMid),
      binanceVsLighterBps: relBps(binanceMid, lighterMid),
    }
  })
}
