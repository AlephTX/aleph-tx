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

        # 4) Scheduled Lighter clock_refresh probes preserve continuity, whereas a real disconnect fails.
        clock_refresh_rows = list(dense_rows)
        clock_refresh_rows.insert(
            1800,
            self._row("lighter", self.t0 + 18_000_005, {}, event="clock_refresh"),
        )
        refresh_path = self.root / "clock_refresh_10h.ndjson"
        refresh_path.write_text(
            "".join(json.dumps(r) + "\n" for r in clock_refresh_rows), encoding="utf-8"
        )
        refresh_report = audit_readiness(refresh_path)
        self.assertTrue(refresh_report["window"]["continuous_ten_hour_unit_satisfied"])
        self.assertEqual(refresh_report["sources"]["lighter"]["clock_refreshes"], 1)
        self.assertEqual(refresh_report["sources"]["lighter"]["disconnects"], 0)

        # 5) Single mid-window disconnect MUST fail continuity even when message cadence is dense.
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

    def test_strategy_scoped_readiness_separates_confirm_vs_usable_book_and_unrelated_feeds(self):
        rows = []
        for step_ms in range(0, 36_360_001, 10_000):
            ts = self.t0 + step_ms
            rows.append(
                self._row(
                    "arcus",
                    ts,
                    {
                        "channel": "l2Orderbook",
                        "type": "channel_data",
                        "id": "NVDA-USD",
                        "contents": {"timestamp": (ts - 80) * 1000, "bids": [["160", "1"]], "asks": [["161", "1"]]},
                    },
                )
            )
            rows.append(
                self._row(
                    "arcus_trades",
                    ts + 1,
                    {"channel": "trades", "id": "NVDA-USD"},
                    event="subscribed" if step_ms == 0 else "message",
                )
            )
            rows.append(
                self._row(
                    "hyperliquid",
                    ts + 2,
                    {"channel": "l2Book", "data": {"coin": "xyz:NVDA", "time": ts - 300}},
                )
            )
            rows.append(
                self._row(
                    "hyperliquid_trades",
                    ts + 3,
                    {"type": "trades", "coin": "xyz:NVDA"},
                    event="subscribed" if step_ms == 0 else "message",
                )
            )
        # Inject one Hyperliquid 3-hour session expiry: confirm after 1050ms, usable l2Book after 1450ms
        dc_at = self.t0 + 10_805_000
        rows.append(self._row("hyperliquid", dc_at, {}, event="disconnect"))
        rows.append(
            self._row(
                "hyperliquid",
                dc_at + 1050,
                {"channel": "subscriptionResponse"},
                event="subscribed",
            )
        )
        rows.append(
            self._row(
                "hyperliquid",
                dc_at + 1450,
                {"channel": "l2Book", "data": {"coin": "xyz:NVDA", "time": dc_at + 1150}},
            )
        )
        rows.sort(key=lambda r: r["received_at_ms"])
        path = self.root / "scoped_10h.ndjson"
        path.write_text("".join(json.dumps(r) + "\n" for r in rows), encoding="utf-8")
        rep = audit_readiness(path)
        self.assertFalse(rep["window"]["continuous_ten_hour_unit_satisfied"])
        self.assertEqual(rep["sources"]["hyperliquid"]["reconnect_confirm_p50_ms"], 1050.0)
        self.assertEqual(rep["sources"]["hyperliquid"]["reconnect_usable_p50_ms"], 1450.0)
        self.assertEqual(
            rep["strategy_readiness"]["arcus_single_venue"]["status"],
            "ready_strict_zero_disconnect",
        )
        self.assertEqual(
            rep["strategy_readiness"]["arcus_hip3_pair"]["status"],
            "ready_with_causal_reconnect_barriers",
        )

    def test_arcus_subscription_ack_does_not_end_usable_data_recovery(self):
        book = {"channel": "l2Orderbook", "id": "SNDK-USD"}
        depth = {"bids": [["160", "1"]], "asks": [["161", "1"]]}
        frames = [
            self._row("arcus", self.t0, {}, event="disconnect"),
            self._row("arcus", self.t0 + 25, dict(book, type="subscribed", contents={})),
            self._row("arcus", self.t0 + 100, dict(book, type="channel_data", contents=depth)),
            self._row(
                "arcus",
                self.t0 + 150,
                dict(book, type="channel_data", contents=dict(depth, timestamp=(self.t0 + 149) * 1000)),
            ),
        ]
        capture = self.root / "arcus_reconnect.ndjson"
        capture.write_text("".join(json.dumps(frame) + "\n" for frame in frames), encoding="utf-8")

        source = audit_readiness(capture)["sources"]["arcus"]
        self.assertEqual(source["reconnect_confirm_p50_ms"], 25.0)
        self.assertEqual(source["reconnect_usable_p50_ms"], 150.0)

    def test_lighter_nonpositive_sequence_does_not_end_usable_data_recovery(self):
        for source in ("lighter", "lighter_robinhood"):
            with self.subTest(source=source):
                def frame(offset, message_type, nonce):
                    return self._row(
                        source,
                        self.t0 + offset,
                        {
                            "type": message_type,
                            "channel": "order_book:139",
                            "order_book": {"nonce": nonce, "bids": [], "asks": []},
                        },
                    )

                rows = [
                    self._row(source, self.t0, {}, event="disconnect"),
                    frame(25, "subscribed/order_book", 0),
                    frame(40, "update/order_book", -1),
                    frame(70, "update/order_book", True),
                    frame(150, "subscribed/order_book", 12),
                ]
                capture = self.root / (source + "-reconnect.ndjson")
                capture.write_text("".join(json.dumps(row) + "\n" for row in rows), encoding="utf-8")

                report = audit_readiness(capture)["sources"][source]
                self.assertEqual(report["reconnect_confirm_p50_ms"], 25.0)
                self.assertEqual(report["reconnect_usable_p50_ms"], 150.0)

    def test_binds_sidecar_bronze_manifest_sha256_and_rejects_size_mismatch(self):
        from archive_public_capture import digest

        capture = self.root / "tokyo-bulk-w03.ndjson"
        capture.write_text(
            json.dumps(
                self._row(
                    "bulk",
                    self.t0 + 10,
                    {"type": "book", "symbol": "BTC-USD", "timestamp": self.t0},
                )
            )
            + "\n",
            encoding="utf-8",
        )
        sidecar = self.root / "tokyo-bulk-w03.ndjson.bronze_manifest.json"
        sidecar.write_text(
            json.dumps({"capture_bytes": capture.stat().st_size, "capture_sha256": "abc"}),
            encoding="utf-8",
        )
        rep = audit_readiness(capture)
        self.assertEqual(rep["manifest_sha256"], digest(sidecar))

        sidecar.write_text(
            json.dumps({"capture_bytes": capture.stat().st_size + 1, "capture_sha256": "abc"}),
            encoding="utf-8",
        )
        with self.assertRaisesRegex(ValueError, "capture_bytes"):
            audit_readiness(capture)


if __name__ == "__main__":
    unittest.main()
