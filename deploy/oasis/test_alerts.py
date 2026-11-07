"""Unit tests for alert rules; run with `make test-python`."""

import unittest

from alerts import CRITICAL, WARNING, Alerter, evaluate

NOW = 10_000_000


def status(live="active", feeder="active", markets=None, result="success"):
    return {"services": {"aleph-tx-feeder": {"ActiveState": feeder},
                         "aleph-tx-live": {"ActiveState": live, "Result": result}},
            "markets": markets if markets is not None else
            [{"venue": "Lighter", "symbol": "ETH", "state": "live"}]}


def summary(start=1000.0, equity=1000.0, last=NOW, **session):
    return {"session": {"last_ms": last, "stopped_ms": None, "stream_lost": 0, **session},
            "snapshot": {"session_start_equity": start, "equity": equity, "dropped": 0,
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
                         ["quote_Hyperliquid", "telemetry_stale"])
        self.assertEqual(evaluate(status(live="inactive", markets=markets), stale, NOW), [])

    def test_drawdown_warns_then_goes_critical_near_the_breaker(self):
        warn = evaluate(status(), summary(equity=968.0), NOW, max_daily_loss_pct=0.05)
        self.assertEqual([(a["key"], a["severity"]) for a in warn], [("drawdown", WARNING)])
        crit = evaluate(status(), summary(equity=953.0), NOW, max_daily_loss_pct=0.05)
        self.assertEqual([(a["key"], a["severity"]) for a in crit], [("drawdown", CRITICAL)])
        self.assertEqual(evaluate(status(), summary(equity=990.0), NOW), [])

    def test_hedge_imbalance_and_stream_counters(self):
        balanced = summary(hedge_account="0xabc", hedge_position=-0.1, lighter_pos=0.1)
        self.assertEqual(evaluate(status(), balanced, NOW), [])
        naked = summary(hedge_account="0xabc", hedge_position=0.0, lighter_pos=0.1, stream_lost=2)
        self.assertEqual(keys(evaluate(status(), naked, NOW)), ["hedge_imbalance", "stream_lost"])


class AlerterTest(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
