"""Unit tests for alert rules; run with `make test-python`."""

import json
import tempfile
import unittest
from pathlib import Path

from alerts import CRITICAL, WARNING, Alerter, evaluate
from telemetry_store import TelemetryStore

DAY_MS = 86_400_000
DAY_START = 20_000 * DAY_MS
NOW = DAY_START + 10_000_000


def status(live="active", feeder="active", markets=None, result="success"):
    return {"services": {"aleph-tx-feeder": {"ActiveState": feeder},
                         "aleph-tx-live": {"ActiveState": live, "Result": result}},
            "markets": markets if markets is not None else
            [{"venue": "Lighter", "symbol": "ETH", "state": "live"}]}


def summary(start=1000.0, equity=1000.0, last=NOW, **session):
    return {"session": {"last_ms": last, "stopped_ms": None, "stream_lost": 0, **session},
            "snapshot": {"session_start_equity": start, "equity": equity, "dropped": 0,
                         "daily_loss_day_start_ms": DAY_START,
                         "daily_loss_start_equity": start, "daily_loss_limit_pct": 0.05,
                         "exchange_position": session.pop("lighter_pos", 0.0)}}


def keys(alerts):
    return [a["key"] for a in alerts]


class EvaluateTest(unittest.TestCase):
    def test_healthy_state_has_no_alerts(self):
        self.assertEqual(evaluate(status(), summary(), NOW), [])

    def test_service_failures_are_critical(self):
        alerts = evaluate(status(live="failed", feeder="inactive", result="exit-code"),
                          summary(), NOW)
        self.assertEqual(keys(alerts), ["feeder_down", "live_failed"])
        self.assertTrue(all(a["severity"] == CRITICAL for a in alerts))

    def test_stale_quotes_and_telemetry_only_matter_while_live(self):
        markets = [{"venue": "Hyperliquid", "symbol": "ETH", "state": "stale"}]
        stale = summary(last=NOW - 120_000)
        self.assertEqual(keys(evaluate(status(markets=markets), stale, NOW)),
                         ["quote_Hyperliquid_ETH", "telemetry_stale"])
        self.assertEqual(evaluate(status(live="inactive", markets=markets), stale, NOW), [])

    def test_markets_at_the_same_venue_resolve_independently(self):
        markets = [{"venue": "Hyperliquid", "symbol": "BTC", "state": "waiting"},
                   {"venue": "Hyperliquid", "symbol": "ETH", "state": "stale"}]
        alerter = Alerter()
        self.assertEqual(keys(alerter.update(evaluate(status(markets=markets), summary(), NOW))),
                         ["quote_Hyperliquid_BTC", "quote_Hyperliquid_ETH"])
        markets[0]["state"] = "live"
        self.assertEqual(keys(alerter.update(evaluate(status(markets=markets), summary(), NOW))),
                         ["quote_Hyperliquid_ETH"])

    def test_drawdown_warns_then_goes_critical_near_the_breaker(self):
        warn = evaluate(status(), summary(equity=970.0), NOW)
        self.assertEqual([(a["key"], a["severity"]) for a in warn], [("drawdown", WARNING)])
        crit = evaluate(status(), summary(equity=955.0), NOW)
        self.assertEqual([(a["key"], a["severity"]) for a in crit], [("drawdown", CRITICAL)])
        self.assertEqual(evaluate(status(), summary(equity=990.0), NOW), [])

    def test_legacy_snapshot_cannot_supply_the_daily_baseline(self):
        legacy = summary(equity=930.0)
        for key in ("daily_loss_day_start_ms", "daily_loss_start_equity", "daily_loss_limit_pct"):
            legacy["snapshot"].pop(key)
        self.assertEqual(evaluate(status(), legacy, NOW), [])

    def test_hedge_imbalance_and_stream_counters(self):
        balanced = summary(hedge_account="0xabc", hedge_position=-0.1, lighter_pos=0.1)
        self.assertEqual(evaluate(status(), balanced, NOW), [])
        naked = summary(hedge_account="0xabc", hedge_position=0.0, lighter_pos=0.1, stream_lost=2)
        self.assertEqual(keys(evaluate(status(), naked, NOW)), ["hedge_imbalance", "stream_lost"])
        hedge_only = summary(hedge_account="0xabc", hedge_position=-0.1)
        self.assertEqual(keys(evaluate(status(), hedge_only, NOW)), ["hedge_imbalance"])


class AlerterTest(unittest.TestCase):
    def test_drawdown_escalation_refreshes_message_and_notifies_once(self):
        calls = []
        times = iter([10, 20, 30])
        alerter = Alerter("test-webhook", post=lambda url, body: calls.append(body),
                          clock=lambda: next(times))
        alerter.update(evaluate(status(), summary(equity=968.0), NOW))
        critical = alerter.update(evaluate(status(), summary(equity=953.0), NOW))
        self.assertEqual(critical[0]["severity"], CRITICAL)
        self.assertEqual(critical[0]["since_ms"], 10_000)
        self.assertIn("4.70%", critical[0]["message"])
        self.assertIn("[critical]", calls[-1]["text"])
        alerter.update(evaluate(status(), summary(equity=952.0), NOW))
        self.assertIn("4.80%", alerter.snapshot()[0]["message"])
        self.assertEqual(len(calls), 2)

    def test_notifies_only_on_transitions(self):
        sent = []
        alerter = Alerter("https://hooks.example/x", post=lambda url, body: sent.append(body),
                          clock=lambda: 1.0)
        down = [{"key": "feeder_down", "severity": CRITICAL, "message": "Feeder 服务状态 failed"}]
        self.assertEqual(len(alerter.update(down)), 1)
        alerter.update(down)
        self.assertEqual(len(sent), 1)
        self.assertIn("[critical]", sent[0]["text"])
        self.assertEqual(alerter.update([]), [])
        self.assertEqual(len(sent), 2)
        self.assertIn("[resolved]", sent[1]["text"])

    def test_without_webhook_tracks_state_and_swallows_post_errors(self):
        quiet = Alerter("", post=lambda *_: self.fail("posted without webhook"))
        quiet.update([{"key": "k", "severity": WARNING, "message": "m"}])
        self.assertEqual(keys(quiet.snapshot()), ["k"])

        def boom(*_):
            raise OSError("down")
        noisy = Alerter("https://hooks.example/x", post=boom)
        noisy.update([{"key": "k", "severity": WARNING, "message": "m"}])
        self.assertEqual(keys(noisy.snapshot()), ["k"])


class RestartedDrawdownTest(unittest.TestCase):
    def test_reopened_guard_keeps_the_daily_alert_across_sessions_and_monitor_restarts(self):
        # Rust generated these snapshots by reopening the actual guard state.
        fixture = Path(__file__).parent / "fixtures" / "restarted_daily_loss.jsonl"
        session, startup, later, _stopped = map(json.loads, fixture.read_text().splitlines())
        self.assertEqual(startup["daily_loss_day_start_ms"], DAY_START)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            first_file = root / "first.jsonl"
            restarted_file = root / "restarted_daily_loss.jsonl"

            def write(path, *records):
                with path.open("a") as output:
                    for record in records:
                        output.write(json.dumps(record) + "\n")

            def alerts_at(now):
                store = TelemetryStore(root / "monitor.sqlite3", root)
                try:
                    store.ingest()
                    return evaluate(status(), store.summary(now_ms=now), now)
                finally:
                    store.db.close()

            write(first_file, {**session, "session_id": "first", "ts_ms": NOW - 2_000},
                  {**startup, "session_start_equity": 1000.0, "ts_ms": NOW - 1_000})
            self.assertEqual(alerts_at(NOW)[0]["severity"], WARNING)
            write(first_file, {"type": "stopped", "ts_ms": NOW})

            # At restart equity equals this session's starting equity, but
            # the account has already lost 3.2% of the persisted daily baseline.
            write(restarted_file, {**session, "ts_ms": NOW + 1_000},
                  {**startup, "ts_ms": NOW + 2_000})
            restarted_alert, = alerts_at(NOW + 2_000)
            self.assertEqual(restarted_alert["severity"], WARNING)
            self.assertIn("当日回撤 3.20%", restarted_alert["message"])

            write(restarted_file, {**later, "ts_ms": NOW + 3_000})
            critical, = alerts_at(NOW + 3_000)
            self.assertEqual(critical["severity"], CRITICAL)
            self.assertIn("当日回撤 4.30%", critical["message"])
            self.assertIn("4.7%", critical["message"])

            next_day = DAY_START + DAY_MS
            self.assertNotIn("drawdown", keys(alerts_at(next_day)))
            write(restarted_file, {**later, "daily_loss_day_start_ms": next_day,
                                   "daily_loss_start_equity": later["equity"], "ts_ms": next_day})
            self.assertEqual(alerts_at(next_day), [])


if __name__ == "__main__":
    unittest.main()
