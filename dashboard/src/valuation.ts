// Turns the server's cost provenance labels into display badges and notes.
import type { BadgeTone } from './components/ui'
import type { LegValuation, Quality, Session } from './types'

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
  notes: string[]
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
      notes: ['服务端版本较旧，无成本来源信息', FUNDING_NOTE],
    }
  }
  const notes = v.missing.map((key) => MISSING[key] ?? key)
  notes.push('会话交易口径，不等于账户权益变化')
  if (v.mark.price !== null) notes.push('两腿按同一策略参考价标记，跨所基差未计')
  return { lighter: leg(v.lighter), hedge: leg(v.hedge), combined: qualityBadge(v.combined_quality), notes }
}
