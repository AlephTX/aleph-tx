// Turns the server's cost provenance labels into display badges and notes.
import type { BadgeTone } from './components/ui'
import { duration } from './format'
import type { LegValuation, Mark, Quality, Session } from './types'

export interface QualityBadge {
  label: string
  tone: BadgeTone
}

const BADGES: Partial<Record<Quality, QualityBadge>> = {
  estimated: { label: '估算', tone: 'warn' },
  unknown: { label: '未知', tone: 'bad' },
  missing: { label: '不完整', tone: 'bad' },
}

/** No badge means every component is venue-reported (or flat). */
export function qualityBadge(quality: Quality | undefined): QualityBadge | null {
  return quality ? (BADGES[quality] ?? null) : { label: '未知', tone: 'bad' }
}

const FUNDING_NOTE = '资金费未计入'

const MISSING: Record<string, string> = {
  mark_price: '无有效标记价，浮动盈亏未知',
  mark_time: '标记价观测时间未知，仅知快照写入时间',
  hedge_fill_price: '部分对冲成交按 IOC 限价估算',
  hedge_price_source: '旧记录对冲价来源未知',
  hedge_actual_fee: '对冲手续费按配置费率估算',
  hedge_fee_source: '旧记录对冲费来源未知',
  funding: FUNDING_NOTE,
}

export interface SessionValuation {
  lighter: QualityBadge | null
  hedge: QualityBadge | null
  combined: QualityBadge | null
  /** One line on where the mark came from and how old its evidence is. */
  mark: string | null
  notes: string[]
}

const SOURCES: Record<string, string> = {
  external_median: '外部中位价',
  lighter_book: 'Lighter 盘口',
  lighter_depth: 'Lighter 深度',
}

/** Ages come from the server at summary time, so both clocks match. */
export function markLine(m: Mark | undefined): string | null {
  if (!m || m.price === null) return null
  const source = SOURCES[m.source ?? ''] ?? '来源未知'
  switch (m.freshness) {
    case 'unknown':
      return `标记 ${source} · 观测时间未知`
    case 'clock_mismatch':
      return `标记 ${source} · 时钟不一致，新鲜度未知`
    default: {
      const age = m.feed_age_ms === null ? '未知' : `${duration(m.feed_age_ms)}前`
      const tail = m.freshness === 'historical' ? ' · 会话已停止' : ''
      return `标记 ${source} · 行情 ${age}${tail}`
    }
  }
}

const leg = (v: LegValuation | undefined) => qualityBadge(v?.quality)

export function sessionValuation(s: Session | null | undefined): SessionValuation | null {
  if (!s) return null
  const v = s.valuation
  if (!v) {
    // Older servers report 0 for an unmarked position and carry no provenance.
    const old = { label: '未知', tone: 'bad' } as const
    return {
      lighter: old,
      hedge: old,
      combined: old,
      mark: null,
      notes: ['服务端版本较旧，无成本来源信息', FUNDING_NOTE],
    }
  }
  const notes = v.missing.map((key) => MISSING[key] ?? key)
  notes.push('会话交易口径，不等于账户权益变化')
  if (v.mark.price !== null) notes.push('两腿按同一策略参考价标记，跨所基差未计')
  return {
    lighter: leg(v.lighter),
    hedge: leg(v.hedge),
    combined: qualityBadge(v.combined_quality),
    mark: markLine(v.mark),
    notes,
  }
}
