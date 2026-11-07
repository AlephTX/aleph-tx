"""Alert rules over monitor state, plus an optional webhook notifier.

`evaluate` is a pure function of the status payload, telemetry summary and
clock, so every rule is unit-testable without systemd, SHM or the network.
`Alerter` re-evaluates periodically and, only when ALEPH_MONITOR_ALERT_WEBHOOK
is set, POSTs a JSON message when an alert is raised or resolved.
"""

import json
import threading
import time
from urllib.request import Request, urlopen

CRITICAL, WARNING = "critical", "warning"
QUOTE_STALE_MS = 15_000
SNAPSHOT_STALE_MS = 60_000
LOSS_WARN_FRACTION = 0.6  # warn at 60% of the configured daily loss budget


def _alert(key, severity, message):
    return {"key": key, "severity": severity, "message": message}


def evaluate(status, summary, now_ms, max_daily_loss_pct=0.05):
    """Active alerts, most severe first. Inputs are the /api payload dicts."""
    alerts = []
    services = (status or {}).get("services", {})
    feeder = services.get("aleph-tx-feeder", {})
    if feeder.get("ActiveState") not in ("active", None):
        alerts.append(_alert("feeder_down", CRITICAL,
                             f"Feeder 服务状态 {feeder.get('ActiveState')}"))
    live = services.get("aleph-tx-live", {})
    live_active = live.get("ActiveState") == "active"
    if live.get("ActiveState") == "failed":
        alerts.append(_alert("live_failed", CRITICAL,
                             f"实盘策略异常退出 ({live.get('Result', 'unknown')})"))

    for market in (status or {}).get("markets", []):
        state, venue = market.get("state"), market.get("venue")
        if live_active and state in ("stale", "waiting"):
            alerts.append(_alert(f"quote_{venue}", WARNING,
                                 f"{venue} {market.get('symbol')} 行情{('过期' if state == 'stale' else '缺失')}"))

    session = (summary or {}).get("session") or {}
    snapshot = (summary or {}).get("snapshot") or {}
    running = live_active and session and not session.get("stopped_ms")
    if running:
        last = session.get("last_ms") or 0
        if now_ms - last > SNAPSHOT_STALE_MS:
            alerts.append(_alert("telemetry_stale", WARNING,
                                 f"遥测已 {(now_ms - last) // 1000}s 未更新"))
    if session.get("stream_lost", 0) > 0:
        alerts.append(_alert("stream_lost", WARNING,
                             f"私有流中断 {session['stream_lost']} 次"))
    if snapshot.get("dropped", 0) > 0:
        alerts.append(_alert("telemetry_dropped", WARNING,
                             f"遥测丢弃 {int(snapshot['dropped'])} 条"))

    start, equity = snapshot.get("session_start_equity") or 0, snapshot.get("equity") or 0
    if running and start > 0 and equity > 0:
        drawdown = (start - equity) / start
        if drawdown >= max_daily_loss_pct * LOSS_WARN_FRACTION:
            severity = CRITICAL if drawdown >= max_daily_loss_pct * 0.9 else WARNING
            alerts.append(_alert("drawdown", severity,
                                 f"会话回撤 {drawdown * 100:.2f}%（熔断 {max_daily_loss_pct * 100:.1f}%）"))

    if session.get("hedge_account"):
        net = (snapshot.get("exchange_position") or 0) + (session.get("hedge_position") or 0)
        gross = abs(snapshot.get("exchange_position") or 0)
        if running and gross > 0 and abs(net) > gross * 0.5:
            alerts.append(_alert("hedge_imbalance", WARNING,
                                 f"两腿净敞口 {net:+.4f}（Lighter {snapshot.get('exchange_position', 0):+.4f}）"))

    order = {CRITICAL: 0, WARNING: 1}
    return sorted(alerts, key=lambda a: (order[a["severity"]], a["key"]))


class Alerter:
    """Tracks raised alerts and notifies a webhook on transitions."""

    def __init__(self, webhook="", post=None, clock=time.time):
        self.webhook = webhook
        self.post = post or self._post
        self.clock = clock
        self.active = {}
        self.lock = threading.Lock()

    def update(self, alerts):
        now = int(self.clock() * 1000)
        with self.lock:
            current = {a["key"]: a for a in alerts}
            raised = [a for key, a in current.items() if key not in self.active]
            resolved = [a for key, a in self.active.items() if key not in current]
            for alert in raised:
                self.active[alert["key"]] = {**alert, "since_ms": now}
            for alert in resolved:
                del self.active[alert["key"]]
            snapshot = sorted(self.active.values(),
                              key=lambda a: (a["severity"] != CRITICAL, a["key"]))
        if self.webhook and (raised or resolved):
            lines = [f"[{a['severity']}] {a['message']}" for a in raised]
            lines += [f"[resolved] {a['message']}" for a in resolved]
            try:
                self.post(self.webhook, {"text": "AlephTX\n" + "\n".join(lines)})
            except Exception as exc:  # notification failure must not stop monitoring
                print(f"alert webhook failed: {type(exc).__name__}", flush=True)
        return snapshot

    def snapshot(self):
        with self.lock:
            return sorted(self.active.values(),
                          key=lambda a: (a["severity"] != CRITICAL, a["key"]))

    @staticmethod
    def _post(url, body):
        request = Request(url, data=json.dumps(body).encode(),
                          headers={"Content-Type": "application/json",
                                   "User-Agent": "AlephTX-monitor/1.0"})
        with urlopen(request, timeout=5) as response:
            response.read(1024)
