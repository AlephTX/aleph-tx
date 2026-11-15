import { useEffect, useState, type KeyboardEvent } from 'react'
import { classifySession } from './analytics'
import { NowContext, useTicker } from './clock'
import {
  DailyLossGuardCard,
  Headline,
  MarkoutQualityCard,
  Performance,
  Risk,
  SessionEfficiencyCard,
} from './components/Kpis'
import { Logs } from './components/Logs'
import { PnlChart } from './components/PnlChart'
import { StatusBar } from './components/StatusBar'
import {
  Accounts,
  HedgeFills,
  Markets,
  OrdersPanel,
  RecentFills,
  Sessions,
} from './components/Tables'
import { Badge } from './components/ui'
import { dateTime, signedUsd } from './format'
import {
  useFills,
  useHedgeFills,
  useSessions,
  useStatus,
  useSummary,
} from './queries'

type WorkspaceTab = 'overview' | 'execution' | 'sessions' | 'infra'

const TABS: readonly { id: WorkspaceTab; label: string }[] = [
  { id: 'overview', label: '总览与风控护栏' },
  { id: 'execution', label: '委托与双腿成交' },
  { id: 'sessions', label: '会话矩阵与单位成本' },
  { id: 'infra', label: '行情矩阵与系统日志' },
]

export default function App() {
  const now = useTicker()
  const [selectedSessionId, setSelectedSessionId] = useState<string | undefined>(undefined)
  const [activeTab, setActiveTab] = useState<WorkspaceTab>('overview')

  const status = useStatus()
  const sessions = useSessions(100)
  const summary = useSummary(selectedSessionId)
  const session = summary.data?.session ?? null
  const effectiveSessionId = selectedSessionId ?? session?.session_id

  const fills = useFills(200, effectiveSessionId)
  const hedgeFills = useHedgeFills(
    200,
    effectiveSessionId,
    activeTab === 'execution' || Boolean(session?.hedge_account),
  )

  useEffect(() => {
    const headlinePnl = session?.hedge_account ? session.combined_pnl : session?.total_pnl
    const pnl = session ? ` ${signedUsd(headlinePnl)}` : ''
    document.title = `AlephTX${pnl} · Trading Console`
  }, [session])

  const handleSelectFromMatrix = (nextId: string | undefined) => {
    setSelectedSessionId(nextId)
    if (nextId) setActiveTab('overview')
  }

  const handleTabKeyDown = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key !== 'ArrowRight' && e.key !== 'ArrowLeft' && e.key !== 'Home' && e.key !== 'End') {
      return
    }
    e.preventDefault()
    const idx = TABS.findIndex((t) => t.id === activeTab)
    let nextIdx = idx
    if (e.key === 'ArrowRight') nextIdx = (idx + 1) % TABS.length
    else if (e.key === 'ArrowLeft') nextIdx = (idx - 1 + TABS.length) % TABS.length
    else if (e.key === 'Home') nextIdx = 0
    else if (e.key === 'End') nextIdx = TABS.length - 1
    const nextTab = TABS[nextIdx]
    if (nextTab) {
      setActiveTab(nextTab.id)
      document.getElementById(`tab-${nextTab.id}`)?.focus()
    }
  }

  const sessionMeta = classifySession(session)

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
          {status.data?.alerts?.map((alert) => (
            <div
              key={alert.key}
              role="alert"
              className={`card px-4 py-2 text-sm ${
                alert.severity === 'critical'
                  ? 'border-down/40 text-down'
                  : 'border-warn/40 text-warn'
              }`}
            >
              {alert.severity === 'critical' ? '严重' : '警告'}：{alert.message}
            </div>
          ))}
          {summary.error && (
            <div className="card border-down/40 px-4 py-2 text-sm text-down">
              遥测数据不可用：{summary.error.message}
            </div>
          )}

          {/* Session Context & Pinning Bar */}
          <div className="card flex flex-wrap items-center justify-between gap-3 px-4 py-2.5 text-xs">
            <div className="flex flex-wrap items-center gap-2.5">
              <Badge tone={selectedSessionId ? 'warn' : 'ok'} dot={!selectedSessionId}>
                {selectedSessionId ? '已固定会话视图' : '跟随最新实时会话'}
              </Badge>
              {session && (
                <>
                  <Badge tone={sessionMeta.tone}>{sessionMeta.label}</Badge>
                  <span className="num text-text">
                    {session.exchange} {session.symbol} · 账户 #{session.account}
                    {session.hedge_account ? ' · 含对冲腿' : ''}
                  </span>
                  <span className="num text-muted">启动于 {dateTime(session.started_ms)}</span>
                </>
              )}
            </div>
            <div className="flex flex-wrap items-center gap-2">
              <label htmlFor="session-selector" className="text-muted">
                会话切换：
              </label>
              <select
                id="session-selector"
                value={selectedSessionId ?? ''}
                onChange={(e) => setSelectedSessionId(e.target.value || undefined)}
                className="num rounded border border-line bg-bg px-2 py-1 text-xs text-text outline-none focus:border-accent"
              >
                <option value="">最新会话（自动跟随）</option>
                {(sessions.data?.sessions ?? []).map((s) => (
                  <option key={s.session_id} value={s.session_id}>
                    {dateTime(s.started_ms)} · {s.exchange} {s.symbol} #{s.account} (
                    {s.hedge_account ? '双腿对冲' : '单腿'}) {signedUsd(s.net_pnl)}
                  </option>
                ))}
              </select>
              {selectedSessionId && (
                <button
                  type="button"
                  onClick={() => setSelectedSessionId(undefined)}
                  className="rounded bg-accent/15 px-2.5 py-1 font-medium text-accent ring-1 ring-inset ring-accent/30 hover:bg-accent/25"
                >
                  返回实时最新会话
                </button>
              )}
            </div>
          </div>

          <Headline summary={summary.data} />
          <p className="px-1 text-xs text-muted">
            Lighter 盈亏统计 Lighter 成交、手续费和返佣；对冲盈亏统计 Hyperliquid IOC 成交和估算的 taker 手续费，浮动部分按同一参考价估算。两者均不含资金费，且各独立策略会话严禁跨账本混算。
          </p>

          {/* Accessible Single-Row Workspace Tablist */}
          <div
            role="tablist"
            aria-label="交易监控工作区"
            onKeyDown={handleTabKeyDown}
            className="workspace-tablist"
          >
            {TABS.map((t) => (
              <button
                key={t.id}
                id={`tab-${t.id}`}
                type="button"
                role="tab"
                aria-selected={activeTab === t.id}
                tabIndex={activeTab === t.id ? 0 : -1}
                onClick={() => setActiveTab(t.id)}
              >
                {t.label}
              </button>
            ))}
          </div>

          {/* Tab Panel 1: Overview & Risk Guards */}
          <div
            role="tabpanel"
            aria-labelledby="tab-overview"
            hidden={activeTab !== 'overview'}
          >
            <div className="space-y-3 pt-1">
              <div className="grid grid-cols-1 gap-3 lg:grid-cols-3">
                <DailyLossGuardCard summary={summary.data} />
                <MarkoutQualityCard summary={summary.data} />
                <SessionEfficiencyCard summary={summary.data} />
              </div>
              <div className="grid grid-cols-1 gap-3 xl:grid-cols-3">
                <div className="xl:col-span-2">
                  <PnlChart sessionId={selectedSessionId} />
                </div>
                <Performance summary={summary.data} />
              </div>
              <div className="grid grid-cols-1 gap-3 xl:grid-cols-3">
                <div className="xl:col-span-2">
                  <Markets markets={status.data?.markets} />
                </div>
                <Risk summary={summary.data} />
              </div>
            </div>
          </div>

          {/* Tab Panel 2: Orders & Dual-Leg Fills */}
          <div
            role="tabpanel"
            aria-labelledby="tab-execution"
            hidden={activeTab !== 'execution'}
          >
            <div className="space-y-3 pt-1">
              <OrdersPanel sessionId={effectiveSessionId} />
              <div className="grid grid-cols-1 gap-3 xl:grid-cols-2">
                <RecentFills fills={fills.data?.fills} error={fills.error} />
                <HedgeFills
                  fills={hedgeFills.data?.fills}
                  error={hedgeFills.error}
                  hedgeAccount={session?.hedge_account}
                />
              </div>
            </div>
          </div>

          {/* Tab Panel 3: Multi-Session Matrix & Cost Efficiency */}
          <div
            role="tabpanel"
            aria-labelledby="tab-sessions"
            hidden={activeTab !== 'sessions'}
          >
            <div className="space-y-3 pt-1">
              <Sessions
                sessions={sessions.data?.sessions}
                error={sessions.error}
                selectedSessionId={selectedSessionId}
                onSelectSession={handleSelectFromMatrix}
              />
            </div>
          </div>

          {/* Tab Panel 4: SHM Matrix, On-Chain Positions & Service Logs */}
          <div
            role="tabpanel"
            aria-labelledby="tab-infra"
            hidden={activeTab !== 'infra'}
          >
            <div className="space-y-3 pt-1">
              <div className="grid grid-cols-1 gap-3 xl:grid-cols-2">
                <Markets markets={status.data?.markets} />
                <Accounts accounts={status.data?.accounts} />
              </div>
              <Logs defaultOpen />
            </div>
          </div>
        </main>
      </div>
    </NowContext>
  )
}
