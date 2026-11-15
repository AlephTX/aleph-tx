import json
from pathlib import Path
import tempfile
import unittest

from archive_public_capture import ingest
from audit_tokyo_selection_readiness import audit_readiness


class TokyoSelectionReadinessTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.t0 = 1_800_000_000_000

    def _row(self, source: str, receipt_ms: int, payload: dict, event: str = "message") -> dict:
        return {
            "source": source,
            "event": event,
            "received_at_ms": receipt_ms,
            "available_at_ms": receipt_ms + 1,
            "payload": payload,
        }

    def test_audits_tokyo_core_vs_robinhood_latency_zero_price_and_small_clamp_carry(self):
        rows = [
            # Lighter Core book: 5ms receipt delta -> sub_10ms_colocated_ready
            self._row(
                "lighter",
                self.t0 + 5,
                {
                    "type": "subscribed/order_book",
                    "channel": "order_book:0",
                    "timestamp": self.t0,
                    "order_book": {"bids": [], "asks": []},
                },
            ),
            # Lighter Robinhood book: 145ms receipt delta -> remote_sequencer_over_25ms
            self._row(
                "lighter_robinhood",
                self.t0 + 145,
                {
                    "type": "subscribed/order_book",
                    "channel": "order_book:0",
                    "timestamp": self.t0,
                    "order_book": {"bids": [], "asks": []},
                },
            ),
            # Lighter Core trades: 1 regular trade + 1 zeroPrice LLP liquidation transfer
            self._row(
                "lighter_trades",
                self.t0 + 12,
                {
                    "type": "update/trade",
                    "channel": "trade:0",
                    "trades": [
                        {
                            "trade_id": 101,
                            "market_id": 0,
                            "timestamp": self.t0,
                            "transaction_time": (self.t0 + 6) * 1000,
                            "price": "2000.5",
                            "size": "0.25",
                            "is_maker_ask": True,
                        },
                        {
                            "trade_id": 102,
                            "market_id": 0,
                            "timestamp": self.t0,
                            "transaction_time": (self.t0 + 7) * 1000,
                            "price": "0",
                            "size": "1.50",
                            "is_maker_ask": False,
                        },
                    ],
                },
            ),
            # Lighter ETH funding: 0.02% / hr = +2.0 bps/hr
            self._row(
                "lighter",
                self.t0 + 20,
                {
                    "type": "update/market_stats",
                    "channel": "market_stats:0",
                    "timestamp": self.t0 + 15,
                    "market_stats": {
                        "market_id": 0,
                        "symbol": "ETH",
                        "index_price": "2000",
                        "mark_price": "2000.2",
                        "current_funding_rate": "0.02",
                    },
                },
            ),
            # Hyperliquid ETH funding: 0.00005 / hr = +0.5 bps/hr
            self._row(
                "hyperliquid",
                self.t0 + 25,
                {
                    "channel": "activeAssetCtx",
                    "data": {
                        "coin": "ETH",
                        "ctx": {"oraclePx": "2000", "funding": "0.00005"},
                    },
                },
            ),
            # End marker after 10.1 hours
            {
                "source": "collector",
                "event": "end",
                "received_at_ms": self.t0 + 36_360_000,
                "available_at_ms": self.t0 + 36_360_001,
                "payload": {},
            },
        ]

        rows.sort(key=lambda r: r["available_at_ms"])
        capture = self.root / "capture.ndjson"
        capture.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
        archive = self.root / "archive"
        ingest(capture, archive)

        # Works identically on sealed archive directory and raw NDJSON capture
        for target in (archive, capture):
            report = audit_readiness(target)
            self.assertEqual(report["sources"]["lighter"]["p50_ms"], 5.0)
            self.assertEqual(
                report["sources"]["lighter"]["regime"], "sub_10ms_colocated_ready"
            )
            self.assertEqual(report["sources"]["lighter_robinhood"]["p50_ms"], 145.0)
            self.assertEqual(
                report["sources"]["lighter_robinhood"]["regime"],
                "remote_sequencer_over_25ms",
            )
            self.assertEqual(
                report["domain_comparison"]["robinhood_minus_core_p50_ms"], 140.0
            )
            self.assertEqual(
                report["zero_price_transfers"]["lighter_trades"],
                {"live_trades": 1, "zero_price_liquidations": 1},
            )
            self.assertEqual(len(report["small_clamp_carry_candidates"]), 1)
            cand = report["small_clamp_carry_candidates"][0]
            self.assertEqual(cand["symbol"], "ETH")
            self.assertEqual(cand["direction"], "short_lighter_long_hyperliquid")
            # (2.0 - 0.5) bps / (1/5 + 1/5) = 1.5 / 0.4 = 3.75 bps / margin-hour
            self.assertEqual(cand["gross_carry_bps_per_margin_hr"], "3.7500")
            self.assertFalse(report["window"]["continuous_ten_hour_unit_satisfied"])

    def test_continuous_ten_hour_qualification_rejects_sparse_endpoints_blackouts_and_disconnects(self):
        # 1) Endpoint-only exploit: two messages 10.1h apart with zero interior coverage MUST fail.
        sparse_rows = [
            self._row("lighter", self.t0, {"type": "update/order_book", "timestamp": self.t0 - 5}),
            self._row(
                "lighter",
                self.t0 + 36_360_000,
                {"type": "update/order_book", "timestamp": self.t0 + 36_359_995},
            ),
        ]
        sparse_path = self.root / "sparse_endpoints.ndjson"
        sparse_path.write_text("".join(json.dumps(r) + "\n" for r in sparse_rows), encoding="utf-8")
        sparse_report = audit_readiness(sparse_path)
        self.assertEqual(sparse_report["window"]["duration_hours"], 10.1)
        self.assertFalse(sparse_report["window"]["continuous_ten_hour_unit_satisfied"])
        self.assertIn(
            "covered_10m_buckets_incomplete:1/60",
            sparse_report["window"]["continuity_blockers"],
        )

        # 2) Dense uninterrupted 10.1h stream (every 10s, 0 disconnects) MUST pass.
        dense_rows = [
            self._row(
                "lighter",
                self.t0 + step_ms,
                {"type": "update/order_book", "timestamp": self.t0 + step_ms - 4},
            )
            for step_ms in range(0, 36_360_001, 10_000)
        ]
        dense_path = self.root / "dense_10h.ndjson"
        dense_path.write_text("".join(json.dumps(r) + "\n" for r in dense_rows), encoding="utf-8")
        dense_report = audit_readiness(dense_path)
        self.assertTrue(dense_report["window"]["continuous_ten_hour_unit_satisfied"])
        self.assertEqual(dense_report["window"]["continuity_blockers"], [])
        self.assertEqual(dense_report["window"]["covered_10m_buckets"], 60)
        self.assertEqual(dense_report["window"]["max_overall_gap_ms"], 10_000)

        # 3) Internal 120s blackout (skipping 11 ticks around +5h) MUST fail continuity.
        blackout_rows = [
            r
            for r in dense_rows
            if not (self.t0 + 18_000_000 < r["received_at_ms"] < self.t0 + 18_120_000)
        ]
        blackout_path = self.root / "blackout_10h.ndjson"
        blackout_path.write_text(
            "".join(json.dumps(r) + "\n" for r in blackout_rows), encoding="utf-8"
        )
        blackout_report = audit_readiness(blackout_path)
        self.assertFalse(blackout_report["window"]["continuous_ten_hour_unit_satisfied"])
        self.assertTrue(
            any(
                b.startswith("source_max_gap_ms_exceeded:lighter:120000>")
                for b in blackout_report["window"]["continuity_blockers"]
            )
        )

        # 4) Single mid-window disconnect MUST fail continuity even when message cadence is dense.
        disconnect_rows = list(dense_rows)
        disconnect_rows.insert(
            1800,
            self._row("lighter", self.t0 + 18_000_005, {}, event="disconnect"),
        )
        disconnect_path = self.root / "disconnect_10h.ndjson"
        disconnect_path.write_text(
            "".join(json.dumps(r) + "\n" for r in disconnect_rows), encoding="utf-8"
        )
        disconnect_report = audit_readiness(disconnect_path)
        self.assertFalse(disconnect_report["window"]["continuous_ten_hour_unit_satisfied"])
        self.assertIn(
            "source_disconnects:lighter:1",
            disconnect_report["window"]["continuity_blockers"],
        )


if __name__ == "__main__":
    unittest.main()
