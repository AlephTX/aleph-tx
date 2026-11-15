import { useQuery } from '@tanstack/react-query'
import { api } from './api'

export type Bucket = '1m' | '5m' | '15m' | '1h' | '1d'

const FAST = 3_000
const SLOW = 15_000

export const useStatus = () =>
  useQuery({ queryKey: ['status'], queryFn: api.status, refetchInterval: FAST })

export const useSummary = (session?: string) =>
  useQuery({
    queryKey: ['summary', session ?? 'latest'],
    queryFn: () => api.summary(session),
    refetchInterval: FAST,
  })

export const useOrders = (status: 'open' | 'all' = 'open', limit = 200, session?: string) =>
  useQuery({
    queryKey: ['orders', status, limit, session ?? 'latest'],
    queryFn: () => api.orders(status, limit, session),
    refetchInterval: FAST,
  })

export const useOpenOrders = (session?: string) => useOrders('open', 200, session)

export const useFills = (limit = 150, session?: string) =>
  useQuery({
    queryKey: ['fills', limit, session ?? 'latest'],
    queryFn: () => api.fills(limit, undefined, session),
    refetchInterval: FAST,
  })

export const useHedgeFills = (limit = 150, session?: string, enabled = true) =>
  useQuery({
    queryKey: ['hedge_fills', limit, session ?? 'latest'],
    queryFn: () => api.hedgeFills(limit, undefined, session),
    refetchInterval: FAST,
    enabled,
  })

export const useSessions = (limit = 100) =>
  useQuery({
    queryKey: ['sessions', limit],
    queryFn: () => api.sessions(limit),
    refetchInterval: SLOW,
  })

export const usePnl = (bucket: Bucket, since: number, session?: string) =>
  useQuery({
    queryKey: ['pnl', bucket, since, session ?? 'all'],
    queryFn: () => api.pnl(bucket, since, session),
    refetchInterval: SLOW,
  })

export const useLogs = (unit: string, enabled: boolean) =>
  useQuery({
    queryKey: ['logs', unit],
    queryFn: () => api.logs(unit, 300),
    refetchInterval: 5_000,
    enabled,
  })
