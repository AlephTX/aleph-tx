// Display formatters. All accept null/undefined/NaN and render a dash.

export const DASH = '—'

const ok = (v: number | null | undefined): v is number =>
  typeof v === 'number' && Number.isFinite(v)

export function num(v: number | null | undefined, digits = 2): string {
  if (!ok(v)) return DASH
  return v.toLocaleString('en-US', { minimumFractionDigits: digits, maximumFractionDigits: digits })
}

export function usd(v: number | null | undefined, digits = 2): string {
  if (!ok(v)) return DASH
  return `${v < 0 ? '-' : ''}$${num(Math.abs(v), digits)}`
}

export function signedUsd(v: number | null | undefined, digits = 2): string {
  if (!ok(v)) return DASH
  return v > 0 ? `+${usd(v, digits)}` : usd(v, digits)
}

export function compactUsd(v: number | null | undefined): string {
  if (!ok(v)) return DASH
  const abs = Math.abs(v)
  const sign = v < 0 ? '-' : ''
  if (abs >= 1e9) return `${sign}$${(abs / 1e9).toFixed(2)}B`
  if (abs >= 1e6) return `${sign}$${(abs / 1e6).toFixed(2)}M`
  if (abs >= 1e4) return `${sign}$${(abs / 1e3).toFixed(1)}K`
  return usd(v)
}

export function pct(v: number | null | undefined, digits = 1): string {
  return ok(v) ? `${(v * 100).toFixed(digits)}%` : DASH
}

export function bps(v: number | null | undefined, digits = 2): string {
  return ok(v) ? `${v > 0 ? '+' : ''}${v.toFixed(digits)} bps` : DASH
}

/** Price with precision scaled to magnitude. */
export function price(v: number | null | undefined): string {
  if (!ok(v) || v === 0) return DASH
  const abs = Math.abs(v)
  return num(v, abs >= 1000 ? 2 : abs >= 1 ? 4 : 6)
}

export function size(v: number | null | undefined): string {
  if (!ok(v)) return DASH
  return v.toLocaleString('en-US', { maximumFractionDigits: 6 })
}

export function tone(v: number | null | undefined): 'up' | 'down' | 'flat' {
  if (!ok(v) || Math.abs(v) < 1e-12) return 'flat'
  return v > 0 ? 'up' : 'down'
}

export const toneClass = { up: 'text-up', down: 'text-down', flat: 'text-text' } as const

const pad = (n: number) => String(n).padStart(2, '0')

export function time(ms: number | null | undefined): string {
  if (!ok(ms) || ms <= 0) return DASH
  const d = new Date(ms)
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`
}

export function dateTime(ms: number | null | undefined): string {
  if (!ok(ms) || ms <= 0) return DASH
  const d = new Date(ms)
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${time(ms)}`
}

export function duration(ms: number | null | undefined): string {
  if (!ok(ms) || ms < 0) return DASH
  const s = Math.floor(ms / 1000)
  const d = Math.floor(s / 86400)
  const h = Math.floor((s % 86400) / 3600)
  const m = Math.floor((s % 3600) / 60)
  if (d > 0) return `${d}天${h}时`
  if (h > 0) return `${h}时${m}分`
  if (m > 0) return `${m}分${s % 60}秒`
  return `${s}秒`
}

export function ago(ms: number | null | undefined, now = Date.now()): string {
  if (!ok(ms) || ms <= 0) return DASH
  return `${duration(Math.max(0, now - ms))}前`
}

/** Parse decimal strings returned by the account pollers. */
export function parse(v: string | null | undefined): number | null {
  if (v == null || v === '') return null
  const n = Number(v)
  return Number.isFinite(n) ? n : null
}
