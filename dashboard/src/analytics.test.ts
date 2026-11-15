import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'
import {
  classifySession,
  crossVenueSpreads,
  dailyLossBudget,
  markoutSummary,
  sessionEfficiency,
} from './analytics'
import { NowContext } from './clock'
import {
  DailyLossGuardCard,
  MarkoutQualityCard,
  SessionEfficiencyCard,
} from './components/Kpis'
import { HedgeFillFee, HedgeFillPrice, Markets, Sessions } from './components/Tables'
import costSummary from './fixtures/summary_cost.json'
import legacySummary from './fixtures/summary_legacy.json'
import type { HedgeFill, Market, Session, Snapshot, SummaryResp } from './types'
import { hedgeFeeSourceBadge, hedgePriceSourceBadge } from './valuation'

const cost = costSummary as unknown as SummaryResp
const legacy = legacySummary as unknown as SummaryResp

describe('strategy analytics & guard invariants', () => {
  it('keeps realized combined PnL distinct from unmarked combined_pnl and computes $/1M cost', () => {
    const base = cost.session as Session
    const eff = sessionEfficiency(base)
    expect(eff).not.toBeNull()
    if (!eff) return
    expect(eff.archetype.kind).toBe('hedged')
    // Unmarked open hedge leg keeps markedCombinedPnl null (never 0).
    expect(eff.markedCombinedPnl).toBeNull()
    // Realized cashflow across legs remains exact and finite.
    expect(eff.realizedCombinedNet).toBeCloseTo(base.net_pnl + base.hedge_net_pnl, 8)

    // Turnover efficiency check: -$100 realized net on $50M gross turnover => +$2.00 / $1M cost, $500k / $1 loss.
    const turnoverSession: Session = {
      ...base,
      hedge_account: '0x1111111111111111111111111111111111111111',
      volume: 25_000_000,
      hedge_volume: 25_000_000,
      net_pnl: -40,
      hedge_net_pnl: -60,
      position: 0,
      hedge_position: 0,
      combined_pnl: -100,
    }
    const tEff = sessionEfficiency(turnoverSession)!
    expect(tEff.grossVolume).toBe(50_000_000)
    expect(tEff.costUsdPerMillionGross).toBeCloseTo(2.0, 6)
    expect(tEff.combinedRealizedBps).toBeCloseTo(-0.02, 6)
    expect(tEff.volumePerDollarLoss).toBeCloseTo(500_000, 4)
    expect(tEff.isFlat).toBe(true)

    // Profitable session yields negative cost per $1M and null volumePerDollarLoss.
    const profitSession: Session = {
      ...turnoverSession,
      hedge_account: null,
      hedge_volume: 0,
      hedge_net_pnl: 0,
      volume: 1_000_000,
      net_pnl: 50,
      total_pnl: 50,
    }
    const pEff = sessionEfficiency(profitSession)!
    expect(classifySession(profitSession).kind).toBe('single_leg')
    expect(pEff.costUsdPerMillionGross).toBeCloseTo(-50.0, 6)
    expect(pEff.volumePerDollarLoss).toBeNull()
  })

  it('evaluates UTC daily loss guard aligned with alerts.py and keeps legacy snapshots unknown', () => {
    // Legacy snapshot has no daily_loss_* fields -> unknown, never 0.
    const legBudget = dailyLossBudget(legacy.snapshot, 1_760_000_005_000)
    expect(legBudget.state).toBe('unknown')
    expect(legBudget.severity).toBe('unknown')
    expect(legBudget.budgetUsd).toBeNull()
    expect(legBudget.usedFraction).toBeNull()

    const dayStart = 1_763_164_800_000 // exact UTC midnight multiple of 86_400_000
    const nowSameDay = dayStart + 3_600_000
    const baseSnap: Snapshot = {
      ...(cost.snapshot as Snapshot),
      equity: 987,
      daily_loss_day_start_ms: dayStart,
      daily_loss_start_equity: 1000,
      daily_loss_limit_pct: 0.02, // $20 budget
    }
    // $13 loss out of $20 budget = 65% >= 60% warn threshold
    const warnBudget = dailyLossBudget(baseSnap, nowSameDay, false)
    expect(warnBudget.state).toBe('active_utc_day')
    expect(warnBudget.severity).toBe('warn')
    expect(warnBudget.budgetUsd).toBeCloseTo(20, 6)
    expect(warnBudget.drawdownUsd).toBeCloseTo(13, 6)
    expect(warnBudget.remainingUsd).toBeCloseTo(7, 6)
    expect(warnBudget.usedFraction).toBeCloseTo(0.65, 6)

    // $18.5 loss out of $20 budget = 92.5% >= 90% critical threshold
    const critBudget = dailyLossBudget({ ...baseSnap, equity: 981.5 }, nowSameDay, false)
    expect(critBudget.severity).toBe('critical')

    // Next UTC day without rollover or stopped session -> historical_day
    const histBudget = dailyLossBudget(baseSnap, dayStart + 90_000_000, false)
    expect(histBudget.state).toBe('historical_day')
  })

  it('preserves unwarmed or missing markout EWMA as null rather than zero', () => {
    const unwarmed = markoutSummary(legacy.snapshot)
    expect(unwarmed.warm).toBe(false)
    expect(unwarmed.ewmaBps).toBeNull()
    expect(unwarmed.coverage).toBeNull()

    const warmed = markoutSummary({
      ...(cost.snapshot as Snapshot),
      markout_marked: 18,
      markout_expired: 1,
      markout_overwritten: 1,
      markout_missed: 0,
      markout_last_delay_ms: 512,
      markout_ewma_bps: -0.42,
    })
    expect(warmed.warm).toBe(true)
    expect(warmed.ewmaBps).toBeCloseTo(-0.42, 6)
    expect(warmed.totalTracked).toBe(20)
    expect(warmed.coverage).toBeCloseTo(0.9, 6)
  })

  it('computes cross-venue SHM basis only on live quotes and labels hedge fill provenance', () => {
    const markets: Market[] = [
      { venue: 'Lighter', symbol: 'ETH', state: 'live', mid: 2500 },
      { venue: 'Hyperliquid', symbol: 'ETH', state: 'live', mid: 2501.25 },
      { venue: 'Binance', symbol: 'ETH', state: 'stale', mid: 2495 },
      { venue: 'Lighter', symbol: 'BTC', state: 'live', mid: 90_000 },
      { venue: 'Hyperliquid', symbol: 'BTC', state: 'waiting' },
      { venue: 'Binance', symbol: 'BTC', state: 'live', mid: 89_955 },
    ]
    const rows = crossVenueSpreads(markets)
    const eth = rows.find((r) => r.symbol === 'ETH')!
    expect(eth.hlVsLighterBps).toBeCloseTo(5.0, 6)
    expect(eth.binanceVsLighterBps).toBeNull() // stale Binance feed excluded

    const btc = rows.find((r) => r.symbol === 'BTC')!
    expect(btc.hlVsLighterBps).toBeNull()
    expect(btc.binanceVsLighterBps).toBeCloseTo(-5.0, 6)

    const sampleHedge: HedgeFill = {
      session_id: 's1',
      offset: 1,
      ts_ms: 1000,
      venue: 'hyperliquid',
      client_order_id: '0xabc',
      side: 'sell',
      price: 2500,
      price_source: 'limit',
      size: 0.1,
      notional: 250,
      fee: 0.1125,
      realized_delta: 0,
      position_after: -0.1,
      fee_source: 'configured_rate',
    }
    expect(hedgePriceSourceBadge('fill')).toBeNull()
    expect(hedgePriceSourceBadge(sampleHedge.price_source)?.label).toBe('IOC限价估算')
    expect(hedgePriceSourceBadge(undefined)?.label).toBe('价源未知')
    expect(hedgeFeeSourceBadge(sampleHedge.fee_source)?.label).toBe('配置费率')
    expect(hedgeFeeSourceBadge(null)?.label).toBe('费源未知')
  })

  it('renders guard cards, hedge provenance badges, 6-slot SHM markets, and accessible sortable sessions', () => {
    const dayStart = 1_763_164_800_000
    const guardHtml = renderToStaticMarkup(
      createElement(
        NowContext,
        { value: dayStart + 10_000 },
        createElement(DailyLossGuardCard, { summary: legacy }),
        createElement(MarkoutQualityCard, { summary: legacy }),
        createElement(SessionEfficiencyCard, { summary: cost }),
      ),
    )
    expect(guardHtml).toContain('基线未知')
    expect(guardHtml).toContain('预热中 / 未知')
    expect(guardHtml).toContain('双腿对冲')

    const limitFill: HedgeFill = {
      session_id: 's1',
      offset: 1,
      ts_ms: 1000,
      venue: 'hyperliquid',
      client_order_id: '0xabc',
      side: 'sell',
      price: 2500,
      price_source: 'limit',
      size: 0.1,
      notional: 250,
      fee: 0.1125,
      realized_delta: 0,
      position_after: -0.1,
      fee_source: 'configured_rate',
    }
    const priceHtml = renderToStaticMarkup(createElement(HedgeFillPrice, { fill: limitFill }))
    const feeHtml = renderToStaticMarkup(createElement(HedgeFillFee, { fill: limitFill }))
    expect(priceHtml).toContain('IOC限价估算')
    expect(feeHtml).toContain('配置费率')

    const marketsHtml = renderToStaticMarkup(
      createElement(Markets, {
        markets: [
          { venue: 'Lighter', symbol: 'ETH', state: 'live', bid: 2499.9, ask: 2500.1, mid: 2500 },
          { venue: 'Binance', symbol: 'ETH', state: 'live', bid: 2501.0, ask: 2501.5, mid: 2501.25 },
        ],
      }),
    )
    expect(marketsHtml).toContain('跨所实时参考基差')
    expect(marketsHtml).toContain('+5.00 bps')

    const sessionsHtml = renderToStaticMarkup(
      createElement(
        NowContext,
        { value: dayStart + 10_000 },
        createElement(Sessions, {
          sessions: [cost.session as Session],
          error: null,
          selectedSessionId: undefined,
          onSelectSession: () => {},
        }),
      ),
    )
    // Sortable headers use <button type="button"> inside <th scope="col">, with no aria-sort="none".
    expect(sessionsHtml).toContain('<th scope="col" class="sortable-col')
    expect(sessionsHtml).not.toContain('aria-sort="none"')
    expect(sessionsHtml).toContain('查看详情')
  })
})
