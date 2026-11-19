import gzip
from hashlib import sha256
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

from quant_lakehouse import (
    build_duckdb_lakehouse,
    bundle_directory_tar,
    evict_local_capture,
    normalize_capture,
    seal_bronze_capture,
    seal_bronze_segment_chain,
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

    def test_duckdb_checks_digest_of_zero_row_table_before_writing(self) -> None:
        capture = self._write_gz_capture("public-empty.ndjson.gz", [self._timed_row(1, "collector", "end", 10)])
        stage = self.root / "silver"
        manifest = normalize_capture(capture, stage, campaign_id="example-integrity", collector_region="tokyo")
        table = manifest["tables"]["public_trades"]
        self.assertEqual(table["rows"], 0)
        path = stage / table["file"]
        # Alter the gzip timestamp. The uncompressed data and byte length still match.
        contents = bytearray(path.read_bytes())
        contents[4] ^= 1
        path.write_bytes(contents)

        destination = self.root / "catalog"
        with self.assertRaisesRegex(ValueError, "public_trades failed hash check"):
            build_duckdb_lakehouse(stage, destination / "parquet", destination / "lake.duckdb")
        self.assertFalse(destination.exists())

    def test_arcus_projection_keeps_unaffected_market_sequence_and_strict_default(self) -> None:
        def event(seq, market, trade_timestamp=None):
            payload = {"id": market, "channel": "trades"}
            if trade_timestamp is not None:
                payload["contents"] = [{"marketDisplayName": market, "tradeId": "1",
                                        "timestamp": trade_timestamp, "price": "10", "size": "2", "side": "BUY"}]
            return self._timed_row(seq + 1, "arcus_trades", "subscribed" if trade_timestamp is None else "message",
                                   seq + 10, market=market, payload=payload)

        source_us = self.t0 * 1000
        rows = [event(0, "SPY-USD"), event(1, "QQQ-USD"), event(2, "SPY-USD", source_us),
                event(3, "SPY-USD", source_us + 13), event(4, "QQQ-USD", source_us + 5),
                self._timed_row(6, "arcus", "subscribed", 17, market="SPY-USD"),
                self._timed_row(7, "collector", "end", 20)]
        capture = self._write_gz_capture("public-projection.ndjson.gz", rows)
        args = dict(campaign_id="example-projection", collector_region="tokyo", precise_times=True)
        with self.assertRaisesRegex(ValueError, "conflicting duplicate Arcus trade"):
            normalize_capture(capture, self.root / "strict", **args)
        with self.assertRaisesRegex(ValueError, "conflicting duplicate Arcus trade"):
            normalize_capture(capture, self.root / "other-market",
                              withheld_arcus_trade_markets=frozenset({"QQQ-USD"}), **args)
        dest = self.root / "partial"
        result = normalize_capture(capture, dest, withheld_arcus_trade_markets=frozenset({"SPY-USD"}), **args)
        self.assertEqual(result["artifact_kind"], "silver_partial_market_projection")
        self.assertEqual(result["withheld_arcus_trade_messages"], {"SPY-USD": 3})
        trades = _read_gz_ndjson(dest / "public_trades.ndjson.gz")
        self.assertEqual([(t["market"], t["sequence"], t["transaction_time_us"]) for t in trades],
                         [("QQQ-USD", 4, source_us + 5)])
        lifecycle = _read_gz_ndjson(dest / "feed_lifecycle.ndjson.gz")
        self.assertFalse(any(r["market"] == "SPY-USD" and r["raw_source"] == "arcus_trades"
                             and r["event"] == "subscribed" for r in lifecycle))
        db = self.root / "isolated" / "lake.duckdb"
        parquet = db.parent / "parquet"
        with self.assertRaisesRegex(ValueError, "--allow-partial-projection"):
            build_duckdb_lakehouse(dest, parquet, db)
        self.assertFalse(db.parent.exists())
        receipt = build_duckdb_lakehouse(dest, parquet, db, allow_partial_projection=True)
        self.assertEqual(receipt["withheld_arcus_trade_messages"], {"SPY-USD": 3})
        self.assertEqual(receipt["artifact_kind"], "silver_partial_market_projection")
        if receipt["engine"] == "python_duckdb":
            import duckdb

            with duckdb.connect(str(db), read_only=True) as conn:
                subscriptions = dict(conn.execute(
                    "SELECT market, arcus_trade_subscriptions FROM v_stream_health WHERE venue = 'arcus'"
                ).fetchall())
                self.assertEqual(subscriptions, {"QQQ-USD": 1, "SPY-USD": 0})
        rows[0]["market"] = "QQQ-USD"
        ambiguous = self._write_gz_capture("ambiguous-projection.ndjson.gz", rows)
        with self.assertRaisesRegex(ValueError, "ambiguous Arcus market"):
            normalize_capture(ambiguous, self.root / "ambiguous",
                              withheld_arcus_trade_markets=frozenset({"SPY-USD"}), **args)

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
            # 2. Arcus WS L2 book + public trades on NVDA-USD (microsecond wire timestamps)
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
            self._timed_row(
                3,
                "arcus_trades",
                "subscribed",
                22,
                market="NVDA-USD",
                payload={"channel": "trades", "id": "NVDA-USD"},
            ),
            self._timed_row(
                4,
                "arcus_trades",
                "message",
                25,
                market="NVDA-USD",
                payload={
                    "channel": "trades",
                    "id": "NVDA-USD",
                    "contents": [
                        {
                            "marketDisplayName": "NVDA-USD",
                            "marketId": 28,
                            "side": "BUY",
                            "price": "140.02",
                            "size": "0.5",
                            "tradeId": "901",
                            "timestamp": (self.t0 + 24) * 1_000 + 321,
                            "sequenceNumber": 11976995605,
                        }
                    ],
                },
            ),
            # 3. Entropy (io:NVDA) L2 book + activeAssetCtx (missing source clock stays None)
            self._timed_row(
                5,
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
                6,
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
                7,
                "hyperliquid_trades",
                "subscribed",
                40,
                market="xyz:NVDA",
                payload={"type": "trades", "coin": "xyz:NVDA"},
            ),
            self._timed_row(
                8,
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
                9,
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
                10,
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
                11,
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
            self._timed_row(
                12,
                "lighter_robinhood",
                "message",
                59,
                payload={
                    "type": "update/market_stats",
                    "channel": "market_stats:0",
                    "timestamp": self.t0 + 58,
                    "market_stats": {
                        "symbol": "ETH",
                        "index_price": "2500.10",
                        "mark_price": "2500.15",
                        "current_funding_rate": "0.0012",
                        "funding_timestamp": self.t0,
                    },
                },
            ),
            # 6. Lighter Robinhood trades: subscription snapshot ignored, zero-price bankruptcy flagged, fees preserved
            self._timed_row(
                13,
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
                14,
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
                15,
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
            self._timed_row(16, "collector", "end", 80),
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
        self.assertEqual(bronze["records"], 16)
        self.assertEqual(bronze["precise_timing_records"], 16)
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
        self.assertEqual(silver["tables"]["public_trades"]["rows"], 4)
        self.assertEqual(silver["tables"]["funding_and_ctx"]["rows"], 2)

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
        self.assertEqual(ctxs[1]["venue"], "lighter_robinhood")
        self.assertEqual(ctxs[1]["funding_rate_raw"], "0.0012")
        self.assertEqual(ctxs[1]["funding_rate_bps"], "0.12")

        trades = _read_gz_ndjson(silver_dir / "public_trades.ndjson.gz")
        self.assertEqual([t["trade_id"] for t in trades], ["901", "12", "501", "502"])
        self.assertEqual(trades[0]["venue"], "arcus")
        self.assertEqual(trades[0]["aggressor_side"], "buy")
        self.assertEqual(trades[0]["source_time_ms"], self.t0 + 24)
        self.assertEqual(trades[0]["transaction_time_us"], (self.t0 + 24) * 1_000 + 321)
        self.assertTrue(trades[2]["is_zero_price_transfer"])
        self.assertFalse(trades[3]["is_zero_price_transfer"])
        self.assertEqual(trades[3]["maker_fee_raw"], 0)
        self.assertEqual(trades[3]["taker_fee_raw"], 20)
        self.assertEqual(trades[3]["transaction_time_us"], (self.t0 + 64) * 1_000 + 777)

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

    def test_eviction_retries_after_tombstone_write_with_matching_evidence(self) -> None:
        capture = self.root / "test-capture.ndjson.gz"
        payload = b"synthetic closed capture"
        capture.write_bytes(payload)
        digest = sha256(payload).hexdigest()
        manifest_path = self.root / "manifest.json"
        manifest_path.write_text(
            json.dumps({
                "campaign_id": "test-archive",
                "capture_sha256": digest,
                "capture_bytes": len(payload),
            }),
            encoding="utf-8",
        )
        receipt_path = self.root / "receipt.json"
        receipt = {"remote_uri": "gdrive://test-fixture", "sha256": digest, "bytes": len(payload)}
        receipt_path.write_text(json.dumps(receipt), encoding="utf-8")

        def evict():
            return evict_local_capture(capture, manifest_path=manifest_path, remote_receipt_path=receipt_path)

        unlink = Path.unlink

        def interrupt_capture_unlink(path, *args, **kwargs):
            if path == capture:
                raise OSError("simulated interruption")
            return unlink(path, *args, **kwargs)

        with mock.patch.object(Path, "unlink", interrupt_capture_unlink):
            with self.assertRaisesRegex(OSError, "simulated interruption"):
                evict()
        self.assertTrue(capture.exists())
        tombstone_path = capture.with_name(f"{capture.name}.evicted.json")
        original_tombstone = tombstone_path.read_bytes()

        receipt_path.write_text(json.dumps({**receipt, "remote_uri": "gdrive://other-fixture"}), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "conflicting eviction tombstone"):
            evict()
        self.assertTrue(capture.exists())
        receipt_path.write_text(json.dumps(receipt), encoding="utf-8")
        self.assertEqual(evict(), json.loads(original_tombstone))
        self.assertEqual(tombstone_path.read_bytes(), original_tombstone)
        self.assertFalse(capture.exists())

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

    def test_rolling_segments_seal_chain_and_stateful_book_continuity(self) -> None:
        # Segment 0: opens Lighter Robinhood L2 snapshot (nonce=100) and rotates at +5h.
        seg0_rows = [
            self._timed_row(
                1,
                "collector",
                "segment_start",
                0,
                payload={"segment_index": 0},
            ),
            self._timed_row(
                2,
                "lighter_robinhood",
                "message",
                10,
                request_sent_at_ms=self.t0 + 5,
                payload={
                    "type": "subscribed/order_book",
                    "channel": "order_book:0",
                    "timestamp": self.t0 + 9,
                    "order_book": {
                        "nonce": 100,
                        "begin_nonce": 99,
                        "last_updated_at": (self.t0 + 9) * 1_000,
                        "bids": [{"price": "140.00", "size": "10.0"}],
                        "asks": [{"price": "140.20", "size": "12.0"}],
                    },
                },
            ),
            self._timed_row(
                3,
                "collector",
                "segment_end",
                18_000_000,
                payload={"segment_index": 0, "continued": True},
            ),
        ]
        # Segment 1: continues same run_id at sequence=4, applies delta update (100 -> 101)
        # without re-snapshot, and seals at +10h05m!
        seg1_rows = [
            self._timed_row(
                4,
                "collector",
                "segment_start",
                18_000_001,
                payload={
                    "segment_index": 1,
                    "prev_segment": "2026-11-16-flywheel.seg0000.ndjson.gz",
                },
            ),
            self._timed_row(
                5,
                "lighter_robinhood",
                "message",
                18_000_010,
                payload={
                    "type": "update/order_book",
                    "channel": "order_book:0",
                    "timestamp": self.t0 + 18_000_009,
                    "order_book": {
                        "nonce": 101,
                        "begin_nonce": 100,
                        "last_updated_at": (self.t0 + 18_000_009) * 1_000,
                        "bids": [{"price": "140.10", "size": "8.0"}],
                        "asks": [],
                    },
                },
            ),
            self._timed_row(
                6,
                "collector",
                "segment_end",
                36_300_000,
                payload={"segment_index": 1, "continued": False},
            ),
            self._timed_row(7, "collector", "end", 36_300_001),
        ]

        seg0 = self._write_gz_capture("2026-11-16-flywheel.seg0000.ndjson.gz", seg0_rows)
        seg1 = self._write_gz_capture("2026-11-16-flywheel.seg0001.ndjson.gz", seg1_rows)

        # Individual segments must NEVER certify a 10h validation unit on their own.
        m0 = seal_bronze_capture(
            seg0,
            campaign_id="2026-11-16-flywheel",
            collector_region="tokyo",
            split_role="selection_dev",
        )
        m1 = seal_bronze_capture(
            seg1,
            campaign_id="2026-11-16-flywheel",
            collector_region="tokyo",
            split_role="selection_dev",
        )
        self.assertEqual(m0["artifact_kind"], "bronze_public_segment")
        self.assertEqual(m0["coverage_class"], "rolling_segment")
        self.assertFalse(m0["meets_ten_hour_validation_unit"])
        self.assertFalse(m1["meets_ten_hour_validation_unit"])

        # Mid-chain non-terminal segment cannot be normalized standalone.
        with self.assertRaisesRegex(ValueError, "terminal collector/end"):
            normalize_capture(
                seg0,
                self.root / "bad_single_seg_silver",
                campaign_id="2026-11-16-flywheel",
                collector_region="tokyo",
                precise_times=True,
            )

        # Contiguous unbroken chain certifies 10h validation unit and preserves delta book state.
        chain_manifest_path = self.root / "2026-11-16-flywheel.chain_manifest.json"
        chain = seal_bronze_segment_chain(
            [seg0, seg1],
            campaign_id="2026-11-16-flywheel",
            collector_region="tokyo",
            split_role="selection_dev",
            manifest_out=chain_manifest_path,
        )
        self.assertTrue(chain["meets_ten_hour_validation_unit"])
        self.assertEqual(chain["coverage_class"], "ten_hour_candidate")
        self.assertEqual(chain["first_sequence"], 1)
        self.assertEqual(chain["last_sequence"], 7)

        chain_silver = self.root / "chain_silver"
        norm = normalize_capture(
            chain_manifest_path,
            chain_silver,
            campaign_id="2026-11-16-flywheel",
            collector_region="tokyo",
            precise_times=True,
        )
        self.assertEqual(norm["tables"]["bbo_quotes"]["rows"], 2)
        bbos = _read_gz_ndjson(chain_silver / "bbo_quotes.ndjson.gz")
        # Segment 1's delta (nonce 100->101) retained Segment 0's ask=140.2 while updating bid=140.1!
        self.assertEqual(bbos[1]["bid_px"], "140.1")
        self.assertEqual(bbos[1]["ask_px"], "140.2")

        # Tampered sequence gap or run_id reset across segments must be rejected!
        gap_rows = [json.loads(json.dumps(r)) for r in seg1_rows]
        for idx, r in enumerate(gap_rows):
            r["capture_timing"]["sequence"] = 5 + idx  # skips sequence 4
        seg1_gap = self._write_gz_capture("2026-11-16-gap.seg0001.ndjson.gz", gap_rows)
        with self.assertRaisesRegex(ValueError, "sequence gap"):
            seal_bronze_segment_chain(
                [seg0, seg1_gap],
                campaign_id="2026-11-16-flywheel",
                collector_region="tokyo",
                split_role="selection_dev",
            )


if __name__ == "__main__":
    unittest.main()
