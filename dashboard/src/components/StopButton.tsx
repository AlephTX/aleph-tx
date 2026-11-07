import { useMutation, useQueryClient } from '@tanstack/react-query'
import { useEffect, useRef, useState } from 'react'
import { api, ApiError } from '../api'

const TOKEN_KEY = 'aleph-control-token'
const CONFIRM_WORD = 'STOP'

/**
 * Emergency stop for aleph-tx-live. Requires typing a confirmation word and the
 * control token. The token is kept in sessionStorage only for this tab.
 */
export function StopButton({ liveActive }: { liveActive: boolean }) {
  const [open, setOpen] = useState(false)
  const dialog = useRef<HTMLDialogElement>(null)

  useEffect(() => {
    const el = dialog.current
    if (!el) return
    if (open && !el.open) el.showModal()
    if (!open && el.open) el.close()
  }, [open])

  return (
    <>
      <button
        type="button"
        disabled={!liveActive}
        onClick={() => setOpen(true)}
        className="rounded-md bg-down px-3 py-1.5 text-xs font-semibold text-white shadow-sm transition hover:bg-down/85 disabled:cursor-not-allowed disabled:bg-panel-2 disabled:text-muted"
        title={liveActive ? '停止实盘策略 (aleph-tx-live)' : '实盘策略未运行'}
      >
        ■ 紧急停止
      </button>
      <dialog
        ref={dialog}
        onClose={() => setOpen(false)}
        className="m-auto w-[min(28rem,92vw)] rounded-lg border border-down/40 bg-panel p-0 text-text backdrop:bg-black/70"
      >
        {open && <StopForm onDone={() => setOpen(false)} />}
      </dialog>
    </>
  )
}

function StopForm({ onDone }: { onDone: () => void }) {
  const qc = useQueryClient()
  const [token, setToken] = useState(() => sessionStorage.getItem(TOKEN_KEY) ?? '')
  const [confirm, setConfirm] = useState('')
  const stop = useMutation({
    mutationFn: () => api.stopLive(token.trim()),
    onSuccess: () => {
      sessionStorage.setItem(TOKEN_KEY, token.trim())
      void qc.invalidateQueries({ queryKey: ['status'] })
    },
    onError: (error) => {
      if (error instanceof ApiError && error.status === 403) sessionStorage.removeItem(TOKEN_KEY)
    },
  })
  const ready = confirm === CONFIRM_WORD && token.trim().length > 0 && !stop.isPending

  if (stop.isSuccess) {
    return (
      <div className="space-y-4 p-5">
        <h3 className="text-base font-semibold text-up">停止指令已发送</h3>
        <p className="text-sm text-muted">
          systemd 正在停止 aleph-tx-live，策略会执行优雅退出（撤单 + 对冲收尾）。请在服务状态和日志中确认。
        </p>
        <div className="flex justify-end">
          <button type="button" onClick={onDone} className="rounded-md bg-panel-2 px-3 py-1.5 text-sm hover:bg-line">
            关闭
          </button>
        </div>
      </div>
    )
  }

  return (
    <form
      method="dialog"
      className="space-y-4 p-5"
      onSubmit={(e) => {
        e.preventDefault()
        if (ready) stop.mutate()
      }}
    >
      <div>
        <h3 className="text-base font-semibold text-down">紧急停止实盘策略</h3>
        <p className="mt-1 text-sm text-muted">
          将执行 <code className="num text-text">systemctl stop aleph-tx-live</code>。服务停止后不会自动重启，需在 VM 上手动启动。
        </p>
      </div>
      <label className="block space-y-1">
        <span className="text-xs text-muted">控制令牌</span>
        <input
          type="password"
          autoComplete="off"
          value={token}
          onChange={(e) => setToken(e.target.value)}
          className="num w-full rounded-md border border-line bg-bg px-3 py-2 text-sm outline-none focus:border-accent"
        />
      </label>
      <label className="block space-y-1">
        <span className="text-xs text-muted">
          输入 <b className="num text-down">{CONFIRM_WORD}</b> 确认
        </span>
        <input
          autoFocus
          value={confirm}
          onChange={(e) => setConfirm(e.target.value)}
          className="num w-full rounded-md border border-line bg-bg px-3 py-2 text-sm outline-none focus:border-down"
        />
      </label>
      {stop.isError && <p className="text-sm text-down">失败：{stop.error.message}</p>}
      <div className="flex justify-end gap-2">
        <button type="button" onClick={onDone} className="rounded-md bg-panel-2 px-3 py-1.5 text-sm hover:bg-line">
          取消
        </button>
        <button
          type="submit"
          disabled={!ready}
          className="rounded-md bg-down px-3 py-1.5 text-sm font-semibold text-white disabled:opacity-40"
        >
          {stop.isPending ? '发送中…' : '确认停止'}
        </button>
      </div>
    </form>
  )
}
