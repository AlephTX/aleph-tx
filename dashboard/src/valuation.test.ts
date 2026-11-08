import { describe, expect, it } from 'vitest'
import { signedUsd } from './format'
import type { Session, Valuation } from './types'
import { qualityBadge, sessionValuation } from './valuation'

const leg = { realized: 'reported', fees: 'reported', unrealized: 'flat', quality: 'reported' } as const

function valuation(overrides: Partial<Valuation> = {}): Valuation {
  return {
    mark: { price: null, source: null, ts_ms: null, scope: 'both legs marked to the strategy reference mid' },
    lighter: { ...leg },
    hedge: { ...leg, price_sources: { fill: 0, limit: 0, unknown: 0 }, fee_sources: { configured_rate: 0, unknown: 0 } },
    combined_quality: 'reported',
    missing: ['funding'],
    excluded: ['funding'],
    scope: 'strategy session trades, not account equity',
    ...overrides,
  }
}

const session = (extra: Partial<Session>) => ({ hedge_account: '0xaa', ...extra }) as Session

describe('valuation', () => {
  it('maps provenance to badges', () => {
    expect(qualityBadge('reported')).toBeNull()
    expect(qualityBadge('flat')).toBeNull()
    expect(qualityBadge('estimated')?.label).toBe('估算')
    expect(qualityBadge('unknown')?.label).toBe('未知')
    expect(qualityBadge('missing')?.label).toBe('不完整')
    expect(qualityBadge(undefined)?.label).toBe('未知')
  })

  it('never renders an unmarked open hedge as zero', () => {
    // Mirrors deploy/oasis/fixtures/cost_session.jsonl after ingest.
    const s = session({
      combined_pnl: null,
      hedge_total_pnl: null,
      valuation: valuation({
        hedge: {
          realized: 'estimated', fees: 'estimated', unrealized: 'missing', quality: 'missing',
          price_sources: { fill: 2, limit: 1, unknown: 0 }, fee_sources: { configured_rate: 3, unknown: 0 },
        },
        combined_quality: 'missing',
        missing: ['mark_price', 'hedge_fill_price', 'hedge_actual_fee', 'funding'],
      }),
    })
    expect(signedUsd(s.combined_pnl)).toBe('—')
    const v = sessionValuation(s)
    expect(v?.combined?.label).toBe('不完整')
    expect(v?.lighter).toBeNull()
    expect(v?.notes).toEqual([
      '无有效标记价，浮动盈亏未知',
      '部分对冲成交按 IOC 限价估算',
      '对冲手续费按配置费率估算',
      '资金费未计入',
      '会话交易口径，不等于账户权益变化',
    ])
  })

  it('notes the shared mark when positions are marked', () => {
    const v = sessionValuation(session({
      valuation: valuation({ mark: { price: 2005, source: 'strategy_mid', ts_ms: 1, scope: '' } }),
    }))
    expect(v?.combined).toBeNull()
    expect(v?.notes).toContain('两腿按同一策略参考价标记，跨所基差未计')
    expect(v?.notes).toContain('资金费未计入')
  })

  it('flags old servers without provenance as unknown', () => {
    const v = sessionValuation(session({ combined_pnl: 3 }))
    expect(v?.combined?.label).toBe('未知')
    expect(v?.notes[0]).toBe('服务端版本较旧，无成本来源信息')
    expect(sessionValuation(null)).toBeNull()
  })
})
