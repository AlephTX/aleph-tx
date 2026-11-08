// Contract tests over real server output: the fixtures are produced by
// deploy/oasis/test_telemetry_store.py (DashboardContractTest) from sessions
// written by the Rust TelemetrySink, so schema drift fails here.
import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'
import { Headline } from './components/Kpis'
import costSummary from './fixtures/summary_cost.json'
import legacySummary from './fixtures/summary_legacy.json'
import markSummary from './fixtures/summary_mark.json'
import skewedSummary from './fixtures/summary_skewed.json'
import type { Session, SummaryResp } from './types'
import { markLine, qualityBadge, sessionValuation } from './valuation'

const cost = costSummary as unknown as SummaryResp
const mark = markSummary as unknown as SummaryResp
const legacy = legacySummary as unknown as SummaryResp
const skewed = skewedSummary as unknown as SummaryResp

const render = (summary: SummaryResp) =>
  renderToStaticMarkup(createElement(Headline, { summary })).replace(/<[^>]+>/g, '|')

/** Text of the stat card whose label is `label`. */
function card(html: string, label: string): string {
  const start = html.indexOf(label)
  expect(start, `card ${label}`).toBeGreaterThanOrEqual(0)
  return html.slice(start, start + 200)
}

describe('valuation', () => {
  it('maps provenance to badges', () => {
    expect(qualityBadge('reported')).toBeNull()
    expect(qualityBadge('flat')).toBeNull()
    expect(qualityBadge('estimated')?.label).toBe('估算')
    expect(qualityBadge('unknown')?.label).toBe('未知')
    expect(qualityBadge('missing')?.label).toBe('不完整')
    expect(qualityBadge(undefined)?.label).toBe('未知')
  })

  it('renders an unmarked open hedge as unknown, never zero', () => {
    const s = cost.session as Session
    expect(s.combined_pnl).toBeNull()
    const html = render(cost)
    // Lighter leg is flat and venue-reported: exact, no badge.
    expect(card(html, 'Lighter 会话盈亏')).toMatch(/^Lighter 会话盈亏\|+\+\$1\.00\|/)
    const combined = card(html, '组合盈亏（含对冲）')
    expect(combined).toMatch(/^组合盈亏（含对冲）\|+不完整\|+—\|/)
    expect(combined).toContain('对冲 —')
    const notes = sessionValuation(s)?.notes ?? []
    expect(notes).toContain('无有效标记价，浮动盈亏未知')
    expect(notes).toContain('部分对冲成交按 IOC 限价估算')
    expect(notes).toContain('对冲手续费按配置费率估算')
    expect(html).toContain('资金费未计入')
    expect(html).toContain('会话交易口径，不等于账户权益变化')
  })

  it('shows evidence age, not snapshot write time, for a re-exported mark', () => {
    const v = (mark.session as Session).valuation
    expect(v?.mark.snapshot_ms).toBe(4000)
    expect(v?.mark.feed_ms).toBe(1400)
    const html = render(mark)
    // Feed age is measured from the 1400 ms book, an hour before summary time.
    expect(html).toContain('标记 外部中位价 · 行情 1时0分前 · 会话已停止')
    expect(card(html, 'Lighter 会话盈亏')).toMatch(/^Lighter 会话盈亏\|+估算\|/)
    expect(html).not.toContain('观测时间未知')
  })

  it('labels legacy marks with unknown observation time', () => {
    const html = render(legacy)
    expect(html).toContain('标记 来源未知 · 观测时间未知')
    expect(html).toContain('标记价观测时间未知，仅知快照写入时间')
    expect(card(html, 'Lighter 会话盈亏')).toMatch(/^Lighter 会话盈亏\|+估算\|/)
  })

  it('shows a sample ahead of the monitor clock as a clock mismatch', () => {
    // Store output for feed 4000 <= now 5000 < sample 6000.
    const m = (skewed.session as Session).valuation?.mark
    expect([m?.freshness, m?.feed_age_ms, m?.sample_age_ms]).toEqual(['clock_mismatch', null, null])
    expect([m?.sampled_ms, m?.feed_ms]).toEqual([6000, 4000])
    const html = render(skewed)
    expect(html).toContain('标记 Lighter 盘口 · 时钟不一致，新鲜度未知')
    expect(html).not.toMatch(/行情 \S+前/)
    // Valuation stays an estimate with its own number; only freshness is unknown.
    expect(card(html, 'Lighter 会话盈亏')).toMatch(/^Lighter 会话盈亏\|+估算\|+\+\$0\.50\|/)
  })

  it('never reports a clock mismatch as fresh', () => {
    const m = (mark.session as Session).valuation?.mark
    expect(m).toBeDefined()
    if (!m) return
    expect(markLine({ ...m, freshness: 'clock_mismatch', feed_age_ms: null })).toBe(
      '标记 外部中位价 · 时钟不一致，新鲜度未知',
    )
    expect(markLine({ ...m, freshness: 'observed', feed_age_ms: null })).toBe('标记 外部中位价 · 行情 未知')
  })

  it('flags old servers without provenance as unknown', () => {
    const s = { ...(mark.session as Session), valuation: undefined }
    const v = sessionValuation(s)
    expect(v?.combined?.label).toBe('未知')
    expect(v?.notes[0]).toBe('服务端版本较旧，无成本来源信息')
    expect(sessionValuation(null)).toBeNull()
  })
})
