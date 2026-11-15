import gzip
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

from quant_lakehouse import (
    build_duckdb_lakehouse,
    bundle_directory_tar,
    evict_local_capture,
    normalize_capture,
    seal_bronze_capture,
    unpack_bundle_tar,
)


def _read_gz_ndjson(path: Path) -> list[dict]:
    with gzip.open(path, "rt", encoding="utf-8") as handle:
        return [json.loads(line) for line in handle if line.strip()]


class QuantLakehouseTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.t0 = 1_800_000_000_000
        self.origin_ns = self.t0 * 1_000_000 - 50_000_000
        self.run_id = "a1b2c3d4e5f60718293a4b5c6d7e8f90"

    def _timed_row(
        self,
        seq: int,
        source: str,
        event: str,
        offset_ms: int,
        *,
        avail_sub_ns: int = 250_000,
        epoch: int = 1,
        market: str | None = None,
        payload: dict | None = None,
        request_sent_at_ms: int | None = None,
    ) -> dict:
        recv_ms = self.t0 + offset_ms
        avail_ms = recv_ms + 1
        recv_wall_ns = recv_ms * 1_000_000 + 100_000
        avail_wall_ns = avail_ms * 1_000_000 + avail_sub_ns
        row = {
            "source": source,
            "event": event,
            "received_at_ms": recv_ms,
            "available_at_ms": avail_ms,
            "capture_timing": {
                "run_id": self.run_id,
                "sequence": seq,
                "connection_epoch": 0 if source == "collector" else epoch,
                "receipt_wall_ns": recv_wall_ns,
                "receipt_monotonic_ns": recv_wall_ns - self.origin_ns,
                "available_wall_ns": avail_wall_ns,
                "available_monotonic_ns": avail_wall_ns - self.origin_ns,
            },
        }
        if market is not None:
            row["market"] = market
        if payload is not None:
            row["payload"] = payload
        if request_sent_at_ms is not None:
            row["request_sent_at_ms"] = request_sent_at_ms
        return row

    def _write_gz_capture(self, name: str, rows: list[dict]) -> Path:
        out = self.root / name
        with gzip.open(out, "wt", encoding="utf-8", compresslevel=3) as handle:
            for item in rows:
                handle.write(json.dumps(item, separators=(",", ":")) + "\n")
        return out

    def test_seal_and_normalize_priority_venues_preserve_causality_and_fees(self) -> None:
        rows = [
            # 1. Binance USD-M BBO with sub-ms precise timing (avail_sub_ns=250_000 -> ceil +1 ms)
            self._timed_row(
                1,
                "binance",
                "message",
                10,
                market="ETHUSDT",
                payload={
                    "e": "bookTicker",
                    "s": "ETHUSDT",
                    "u": 9001,
                    "E": self.t0 + 8,
                    "T": self.t0 + 7,
                    "b": "2499.90",
                    "B": "12.5",
                    "a": "2500.10",
                    "A": "8.25",
                },
            ),
            # 2. Arcus WS L2 book on NVDA-USD (microsecond wire timestamp)
            self._timed_row(
                2,
                "arcus",
                "message",
                20,
                payload={
                    "type": "subscribed",
                    "channel": "l2Orderbook",
                    "id": "NVDA-USD",
                    "contents": {
                        "timestamp": (self.t0 + 18) * 1_000 + 456,
                        "bids": [["139.95", "50"]],
                        "asks": [["140.05", "40"]],
                    },
                },
            ),
            # 3. Entropy (io:NVDA) L2 book + activeAssetCtx (missing source clock stays None)
            self._timed_row(
                3,
                "hyperliquid",
                "message",
                30,
                market="io:NVDA",
                payload={
                    "channel": "l2Book",
                    "data": {
                        "coin": "io:NVDA",
                        "time": self.t0 + 27,
                        "levels": [[{"px": "139.90", "sz": "25"}], [{"px": "140.00", "sz": "30"}]],
                    },
                },
            ),
            self._timed_row(
                4,
                "hyperliquid",
                "message",
                35,
                market="io:NVDA",
                payload={
                    "channel": "activeAssetCtx",
                    "data": {
                        "coin": "io:NVDA",
                        "ctx": {"oraclePx": "139.96", "markPx": "139.95", "funding": "0.0000125"},
                    },
                },
            ),
            # 4. TradeXYZ (xyz:NVDA) trade subscription + cached print (filtered) + live print (kept)
            self._timed_row(
                5,
                "hyperliquid_trades",
                "subscribed",
                40,
                market="xyz:NVDA",
                payload={"type": "trades", "coin": "xyz:NVDA"},
            ),
            self._timed_row(
                6,
                "hyperliquid_trades",
                "message",
                45,
                market="xyz:NVDA",
                payload={
                    "channel": "trades",
                    "data": [
                        {"coin": "xyz:NVDA", "side": "B", "px": "140.01", "sz": "2", "tid": 11, "time": self.t0 + 39},
                        {"coin": "xyz:NVDA", "side": "A", "px": "139.98", "sz": "5", "tid": 12, "time": self.t0 + 44},
                    ],
                },
            ),
            # 5. Lighter Robinhood book snapshot -> delta update -> nonce gap
            self._timed_row(
                7,
                "lighter_robinhood",
                "message",
                50,
                request_sent_at_ms=self.t0 + 46,
                payload={
                    "type": "subscribed/order_book",
                    "channel": "order_book:0",
                    "timestamp": self.t0 + 49,
                    "order_book": {
                        "nonce": 100,
                        "begin_nonce": 99,
                        "last_updated_at": (self.t0 + 48) * 1_000,
                        "bids": [{"price": "2499.50", "size": "4.0"}],
                        "asks": [{"price": "2500.50", "size": "3.0"}],
                    },
                },
            ),
            self._timed_row(
                8,
                "lighter_robinhood",
                "message",
                55,
                payload={
                    "type": "update/order_book",
                    "channel": "order_book:0",
                    "timestamp": self.t0 + 54,
                    "order_book": {
                        "nonce": 101,
                        "begin_nonce": 100,
                        "last_updated_at": (self.t0 + 53) * 1_000,
                        "bids": [{"price": "2499.80", "size": "6.0"}],
                        "asks": [],
                    },
                },
            ),
            self._timed_row(
                9,
                "lighter_robinhood",
                "message",
                58,
                payload={
                    "type": "update/order_book",
                    "channel": "order_book:0",
                    "timestamp": self.t0 + 57,
                    "order_book": {
                        "nonce": 105,
                        "begin_nonce": 103,
                        "last_updated_at": (self.t0 + 56) * 1_000,
                        "bids": [{"price": "2499.90", "size": "1.0"}],
                        "asks": [],
                    },
                },
            ),
            # 6. Lighter Robinhood trades: subscription snapshot ignored, zero-price bankruptcy flagged, fees preserved
            self._timed_row(
                10,
                "lighter_robinhood_trades",
                "message",
                60,
                payload={
                    "type": "subscribed/trade",
                    "channel": "trade:0",
                    "fee_fields_version": 1,
                    "trades": [
                        {
                            "trade_id": "500",
                            "market_id": 0,
                            "timestamp": self.t0 + 50,
                            "price": "2500.00",
                            "size": "1.0",
                            "is_maker_ask": True,
                        }
                    ],
                },
            ),
            self._timed_row(
                11,
                "lighter_robinhood_trades",
                "message",
                65,
                payload={
                    "type": "update/trade",
                    "channel": "trade:0",
                    "fee_fields_version": 1,
                    "trades": [
                        {
                            "trade_id": "501",
                            "market_id": 0,
                            "timestamp": self.t0 + 63,
                            "price": "0.00",
                            "size": "0.25",
                            "is_maker_ask": False,
                        },
                        {
                            "trade_id": "502",
                            "market_id": 0,
                            "timestamp": self.t0 + 64,
                            "price": "2500.25",
                            "size": "2.0",
                            "is_maker_ask": True,
                            "maker_fee": 0,
                            "taker_fee": 20,
                            "transaction_time": (self.t0 + 64) * 1_000 + 777,
                            "block_height": 123456,
                        },
                    ],
                },
            ),
            # 7. BULK REST snapshot with nanosecond wire timestamp + late source clock (still ordered by available_at_ms)
            self._timed_row(
                12,
                "bulk_snapshot",
                "message",
                70,
                payload={
                    "market": "SOL-USD",
                    "data": {
                        "symbol": "SOL-USD",
                        "updateType": "snapshot",
                        "timestamp": (self.t0 + 12) * 1_000_000 + 999,
                        "levels": [[{"px": "150.10", "sz": "10"}], [{"px": "150.20", "sz": "12"}]],
                    },
                },
            ),
            self._timed_row(13, "collector", "end", 80),
        ]

        capture_path = self._write_gz_capture("2026-11-15-tokyo-priority.ndjson.gz", rows)
        manifest_path = self.root / "2026-11-15-tokyo-priority.manifest.json"
        bronze = seal_bronze_capture(
            capture_path,
            campaign_id="2026-11-15-tokyo-priority",
            collector_region="tokyo",
            split_role="selection_dev",
            manifest_out=manifest_path,
        )
        self.assertEqual(bronze["records"], 13)
        self.assertEqual(bronze["precise_timing_records"], 13)
        self.assertFalse(bronze["meets_ten_hour_validation_unit"])
        self.assertEqual(bronze["coverage_class"], "diagnostic_short_window")
        self.assertIn("arcus:mainnet:NVDA-USD", bronze["markets_observed"])
        self.assertIn("entropy:hip3-io:io:NVDA", bronze["markets_observed"])
        self.assertIn("trade_xyz:hip3-xyz:xyz:NVDA", bronze["markets_observed"])
        self.assertIn("lighter_robinhood:robinhood:0", bronze["markets_observed"])

        silver_dir = self.root / "silver_staging"
        silver = normalize_capture(
            capture_path,
            silver_dir,
            campaign_id="2026-11-15-tokyo-priority",
            collector_region="tokyo",
            precise_times=True,
        )
        self.assertEqual(silver["tables"]["bbo_quotes"]["rows"], 6)
        self.assertEqual(silver["tables"]["public_trades"]["rows"], 3)
        self.assertEqual(silver["tables"]["funding_and_ctx"]["rows"], 1)

        bbos = _read_gz_ndjson(silver_dir / "bbo_quotes.ndjson.gz")
        # Ceil of available_wall_ns (+250_000 ns) adds +1 ms over truncated available_at_ms
        self.assertEqual(bbos[0]["venue"], "binance")
        self.assertEqual(bbos[0]["available_at_ms"], self.t0 + 12)
        self.assertEqual(bbos[1]["venue"], "arcus")
        self.assertEqual(bbos[1]["source_time_ms"], self.t0 + 18)
        self.assertEqual(bbos[2]["venue"], "entropy")
        self.assertEqual(bbos[2]["domain"], "hip3-io")
        # Reconstructed Lighter Robinhood delta updated best bid to 2499.80 while retaining ask 2500.50
        self.assertEqual(bbos[4]["venue"], "lighter_robinhood")
        self.assertEqual(bbos[4]["bid_px"], "2499.8")
        self.assertEqual(bbos[4]["ask_px"], "2500.5")
        # Late BULK source_time_ms (t0+12) still preserves its causal arrival sequence at the end!
        self.assertEqual(bbos[5]["venue"], "bulk")
        self.assertEqual(bbos[5]["source_time_ms"], self.t0 + 12)
        self.assertGreater(bbos[5]["available_at_ms"], bbos[4]["available_at_ms"])

        ctxs = _read_gz_ndjson(silver_dir / "funding_and_ctx.ndjson.gz")
        self.assertIsNone(ctxs[0]["source_time_ms"])
        self.assertEqual(ctxs[0]["venue"], "entropy")
        self.assertEqual(ctxs[0]["funding_rate_bps"], "0.125")

        trades = _read_gz_ndjson(silver_dir / "public_trades.ndjson.gz")
        self.assertEqual([t["trade_id"] for t in trades], ["12", "501", "502"])
        self.assertTrue(trades[1]["is_zero_price_transfer"])
        self.assertFalse(trades[2]["is_zero_price_transfer"])
        self.assertEqual(trades[2]["maker_fee_raw"], 0)
        self.assertEqual(trades[2]["taker_fee_raw"], 20)
        self.assertEqual(trades[2]["transaction_time_us"], (self.t0 + 64) * 1_000 + 777)

        lifecycle = _read_gz_ndjson(silver_dir / "feed_lifecycle.ndjson.gz")
        gaps = [row for row in lifecycle if row["event"] == "sequence_gap"]
        self.assertEqual(len(gaps), 1)
        self.assertEqual(gaps[0]["venue"], "lighter_robinhood")

        parquet_root = self.root / "parquet"
        db_path = self.root / "lakehouse.duckdb"
        duck = build_duckdb_lakehouse(silver_dir, parquet_root, db_path)
        sql_text = Path(duck["sql_path"]).read_text(encoding="utf-8")
        self.assertIn("ORDER BY venue, domain, market, available_at_ms, sequence", sql_text)
        self.assertIn("PARTITION_BY (collector_region, venue, utc_date, campaign_id)", sql_text)
        self.assertIn("COMPRESSION ZSTD", sql_text)
        self.assertIn("DECIMAL(38, 12)", sql_text)

    def test_rejects_partial_and_private_paths_and_verifies_eviction_and_bundles(self) -> None:
        partial = self.root / "capture.ndjson.gz.partial"
        partial.write_bytes(b"incomplete")
        with self.assertRaisesRegex(ValueError, "incomplete"):
            seal_bronze_capture(
                partial,
                campaign_id="2026-11-15-bad",
                collector_region="tokyo",
                split_role="selection_dev",
            )

        private_file = self.root / "2026-11-15-private-orders.ndjson.gz"
        private_file.write_bytes(b"secret")
        with self.assertRaisesRegex(ValueError, "private-indicating"):
            seal_bronze_capture(
                private_file,
                campaign_id="2026-11-15-clean",
                collector_region="tokyo",
                split_role="selection_dev",
            )

        rows = [
            {
                "source": "binance",
                "event": "message",
                "received_at_ms": self.t0,
                "available_at_ms": self.t0 + 1,
                "market": "BTCUSDT",
                "payload": {"e": "bookTicker", "s": "BTCUSDT", "b": "90000", "B": "1", "a": "90001", "A": "1"},
            },
            {
                "source": "collector",
                "event": "end",
                "received_at_ms": self.t0 + 36_000_005,
                "available_at_ms": self.t0 + 36_000_005,
            },
        ]
        capture = self._write_gz_capture("2026-11-15-ten-hour.ndjson.gz", rows)
        manifest_path = self.root / "2026-11-15-ten-hour.manifest.json"
        bronze = seal_bronze_capture(
            capture,
            campaign_id="2026-11-15-ten-hour",
            collector_region="tokyo",
            split_role="holdout_vault",
            manifest_out=manifest_path,
        )
        self.assertTrue(bronze["meets_ten_hour_validation_unit"])

        # Bad remote receipt must fail closed without deleting local capture.
        bad_receipt = self.root / "bad_receipt.json"
        bad_receipt.write_text(
            json.dumps({"remote_uri": "gdrive://1XZPU/file", "sha256": "0" * 64, "bytes": bronze["capture_bytes"]}),
            encoding="utf-8",
        )
        with self.assertRaisesRegex(ValueError, "refusing eviction"):
            evict_local_capture(capture, manifest_path=manifest_path, remote_receipt_path=bad_receipt)
        self.assertTrue(capture.exists())

        # Valid remote receipt evicts local blob and leaves tombstone + manifest.
        good_receipt = self.root / "good_receipt.json"
        good_receipt.write_text(
            json.dumps(
                {
                    "remote_uri": "gdrive://1XZPU/2026-11-15-ten-hour.ndjson.gz",
                    "sha256": bronze["capture_sha256"],
                    "bytes": bronze["capture_bytes"],
                }
            ),
            encoding="utf-8",
        )
        tombstone = evict_local_capture(capture, manifest_path=manifest_path, remote_receipt_path=good_receipt)
        self.assertFalse(capture.exists())
        self.assertTrue(manifest_path.exists())
        self.assertTrue((self.root / f"{capture.name}.evicted.json").exists())
        self.assertEqual(tombstone["capture_sha256"], bronze["capture_sha256"])

        # Deterministic TAR bundle round-trip + traversal defense.
        src_dir = self.root / "sample_parquet"
        (src_dir / "collector_region=tokyo/venue=binance").mkdir(parents=True)
        (src_dir / "collector_region=tokyo/venue=binance/part0.parquet").write_bytes(b"PAR1-mock-bytes-PAR1")
        bundle_a = self.root / "bundle_a.tar"
        bundle_b = self.root / "bundle_b.tar"
        man_a = bundle_directory_tar(src_dir, bundle_a, campaign_id="2026-11-15-ten-hour")
        man_b = bundle_directory_tar(src_dir, bundle_b, campaign_id="2026-11-15-ten-hour")
        self.assertEqual(man_a["bundle_sha256"], man_b["bundle_sha256"])

        unpacked = self.root / "unpacked"
        res = unpack_bundle_tar(bundle_a, bundle_a.with_suffix(".manifest.json"), unpacked)
        self.assertEqual(res["files_verified"], 1)
        self.assertEqual(
            (unpacked / "collector_region=tokyo/venue=binance/part0.parquet").read_bytes(),
            b"PAR1-mock-bytes-PAR1",
        )

        # Path traversal tar must be rejected before writing outside target_dir.
        evil_tar = self.root / "evil.tar"
        with tarfile.open(evil_tar, "w") as tf:
            info = tarfile.TarInfo(name="../escape.txt")
            info.size = 4
            tf.addfile(info, fileobj=__import__("io").BytesIO(b"nope"))
        from quant_lakehouse import digest_unchanged

        evil_sha, evil_bytes = digest_unchanged(evil_tar)
        evil_man = self.root / "evil.manifest.json"
        evil_man.write_text(
            json.dumps(
                {
                    "campaign_id": "2026-11-15-ten-hour",
                    "bundle_sha256": evil_sha,
                    "bundle_bytes": evil_bytes,
                    "files": [{"path": "../escape.txt", "sha256": "0" * 64, "bytes": 4}],
                }
            ),
            encoding="utf-8",
        )
        with self.assertRaisesRegex(ValueError, "traversal"):
            unpack_bundle_tar(evil_tar, evil_man, self.root / "safe_target")


if __name__ == "__main__":
    unittest.main()
