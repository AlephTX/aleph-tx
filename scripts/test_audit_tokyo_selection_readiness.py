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


if __name__ == "__main__":
    unittest.main()
