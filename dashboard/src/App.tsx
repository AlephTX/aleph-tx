import { useEffect } from 'react'
import { NowContext, useTicker } from './clock'
import { Accounts, Markets, OpenOrders, RecentFills, Sessions } from './components/Tables'
import { Headline, Performance, Risk } from './components/Kpis'
import { Logs } from './components/Logs'
import { PnlChart } from './components/PnlChart'
import { StatusBar } from './components/StatusBar'
import { useFills, useOpenOrders, useSessions, useStatus, useSummary } from './queries'
import { signedUsd } from './format'

export default function App() {
  const now = useTicker()
  const status = useStatus()
  const summary = useSummary()
  const orders = useOpenOrders()
  const fills = useFills(150)
  const sessions = useSessions()
  const session = summary.data?.session ?? null

  useEffect(() => {
    const pnl = session ? ` ${signedUsd(session.total_pnl)}` : ''
    document.title = `AlephTX${pnl} · Trading Console`
  }, [session])

  return (
    <NowContext value={now}>
    <div className="min-h-full">
      <StatusBar
        status={status.data}
        error={status.error}
        updatedAt={status.dataUpdatedAt}
        sessionStarted={session?.started_ms ?? null}
        sessionStopped={session?.stopped_ms ?? null}
      />
      <main className="mx-auto max-w-[1680px] space-y-3 p-4">
        {summary.error && (
          <div className="card border-down/40 px-4 py-2 text-sm text-down">
            遥测数据不可用：{summary.error.message}
          </div>
        )}
        <Headline summary={summary.data} />
        <p className="px-1 text-xs text-muted">
          成交盈亏仅统计 Lighter 成交、手续费和返佣，不包含 Hyperliquid 对冲盈亏或资金费。
        </p>
        <div className="grid grid-cols-1 gap-3 xl:grid-cols-3">
          <div className="xl:col-span-2">
            <PnlChart />
          </div>
          <Performance summary={summary.data} />
        </div>
        <div className="grid grid-cols-1 gap-3 xl:grid-cols-3">
          <div className="xl:col-span-2">
            <OpenOrders orders={orders.data?.orders} error={orders.error} />
          </div>
          <Risk summary={summary.data} />
        </div>
        <div className="grid grid-cols-1 gap-3 xl:grid-cols-3">
          <div className="xl:col-span-2">
            <RecentFills fills={fills.data?.fills} error={fills.error} />
          </div>
          <div className="space-y-3">
            <Markets markets={status.data?.markets} />
            <Accounts accounts={status.data?.accounts} />
          </div>
        </div>
        <Sessions sessions={sessions.data?.sessions} error={sessions.error} />
        <Logs />
      </main>
    </div>
    </NowContext>
  )
}
