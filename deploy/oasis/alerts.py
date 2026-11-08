"""Alert rules over monitor state, plus an optional webhook notifier.

`evaluate` is a pure function of the status payload, telemetry summary and
clock, so every rule is unit-testable without systemd, SHM or the network.
`Alerter` tracks each evaluation and can notify on new, changed-severity,
or resolved alerts when ALEPH_MONITOR_ALERT_WEBHOOK is set.
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


def evaluate(status, summary, now_ms):
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
        state, venue, symbol = market.get("state"), market.get("venue"), market.get("symbol")
        if live_active and state in ("stale", "waiting"):
            alerts.append(_alert(f"quote_{venue}_{symbol}", WARNING,
                                 f"{venue} {symbol} 行情{('过期' if state == 'stale' else '缺失')}"))

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

    start = snapshot.get("daily_loss_start_equity") or 0
    limit = snapshot.get("daily_loss_limit_pct") or 0
    equity = snapshot.get("equity") or 0
    utc_day_start = now_ms - now_ms % 86_400_000
    if (running and snapshot.get("daily_loss_day_start_ms") == utc_day_start
            and start > 0 and equity > 0 and 0 < limit < 1):
        budget = start * limit
        if equity <= start - budget * LOSS_WARN_FRACTION:
            severity = CRITICAL if equity <= start - budget * 0.9 else WARNING
            drawdown = (start - equity) / start
            alerts.append(_alert("drawdown", severity,
                                 f"当日回撤 {drawdown * 100:.2f}%（UTC 日亏熔断 {limit * 100:g}%）"))

    if running and snapshot and session.get("hedge_account"):
        primary = snapshot.get("exchange_position") or 0.0
        hedge = session.get("hedge_position") or 0.0
        net = primary + hedge
        if abs(net) > max(abs(primary) * 0.5, 1e-12):
            alerts.append(_alert("hedge_imbalance", WARNING,
                                 f"两腿净敞口 {net:+.4f}（Lighter {primary:+.4f}）"))

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
            current = {}
            changed = []
            for alert in alerts:
                key = alert["key"]
                previous = self.active.get(key)
                since = previous["since_ms"] if previous else now
                current[key] = {**alert, "since_ms": since}
                if previous is None or previous["severity"] != alert["severity"]:
                    changed.append(alert)
            resolved = [a for key, a in self.active.items() if key not in current]
            self.active = current
            snapshot = sorted(self.active.values(),
                              key=lambda a: (a["severity"] != CRITICAL, a["key"]))
        if self.webhook and (changed or resolved):
            lines = [f"[{a['severity']}] {a['message']}" for a in changed]
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
