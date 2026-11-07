// HTTP client for the monitor backend (deploy/oasis/monitor.py).

import type {
  Fill,
  LogLine,
  Order,
  PnLResp,
  Session,
  StatusResp,
  StopResp,
  SummaryResp,
} from './types'

export class ApiError extends Error {
  readonly status: number

  constructor(status: number, message: string) {
    super(message)
    this.status = status
  }
}

async function call<T>(route: string, init?: RequestInit): Promise<T> {
  const resp = await fetch(`/api${route}`, { ...init, headers: { Accept: 'application/json', ...init?.headers } })
  if (resp.ok) return (await resp.json()) as T
  const text = await resp.text()
  let message = `HTTP ${resp.status}`
  try {
    const body = JSON.parse(text) as { error?: unknown }
    if (typeof body.error === 'string') message = body.error
  } catch {
    if (text) message = `${message}: ${text.slice(0, 200)}`
  }
  throw new ApiError(resp.status, message)
}

const qs = (params: Record<string, string | number | undefined>) => {
  const q = new URLSearchParams()
  for (const [k, v] of Object.entries(params)) if (v !== undefined && v !== '') q.set(k, String(v))
  return q.toString()
}

export const api = {
  status: () => call<StatusResp>('/status'),
  summary: () => call<SummaryResp>('/summary'),
  fills: (limit = 200, before?: number, session?: string) =>
    call<{ fills: Fill[] }>(`/fills?${qs({ limit, before, session })}`),
  orders: (status: 'open' | 'all' = 'open', limit = 200, session?: string) =>
    call<{ orders: Order[] }>(`/orders?${qs({ status, limit, session })}`),
  pnl: (bucket: string, since = 0) => call<PnLResp>(`/pnl?${qs({ bucket, since })}`),
  sessions: (limit = 50) => call<{ sessions: Session[] }>(`/sessions?${qs({ limit })}`),
  logs: (unit: string, lines = 200) => call<{ logs: LogLine[] }>(`/logs?${qs({ unit, lines })}`),
  stopLive: (token: string) =>
    call<StopResp>('/live/stop', { method: 'POST', headers: { 'X-Aleph-Control': token } }),
}
