import { useQuery } from '@tanstack/react-query'
import { api } from './api'

export type Bucket = '1m' | '5m' | '15m' | '1h' | '1d'

const FAST = 3_000
const SLOW = 15_000

export const useStatus = () =>
  useQuery({ queryKey: ['status'], queryFn: api.status, refetchInterval: FAST })

export const useSummary = () =>
  useQuery({ queryKey: ['summary'], queryFn: api.summary, refetchInterval: FAST })

export const useOpenOrders = () =>
  useQuery({ queryKey: ['orders', 'open'], queryFn: () => api.orders('open'), refetchInterval: FAST })

export const useFills = (limit = 100) =>
  useQuery({ queryKey: ['fills', limit], queryFn: () => api.fills(limit), refetchInterval: FAST })

export const useSessions = () =>
  useQuery({ queryKey: ['sessions'], queryFn: () => api.sessions(30), refetchInterval: SLOW })

export const usePnl = (bucket: Bucket, since: number) =>
  useQuery({
    queryKey: ['pnl', bucket, since],
    queryFn: () => api.pnl(bucket, since),
    refetchInterval: SLOW,
  })

export const useLogs = (unit: string, enabled: boolean) =>
  useQuery({
    queryKey: ['logs', unit],
    queryFn: () => api.logs(unit, 300),
    refetchInterval: 5_000,
    enabled,
  })
