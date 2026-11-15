import gzip
import json
from pathlib import Path
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "deploy" / "oasis"))

from flywheel_sync import atomic_sqlite_backup
import live_attribution
from live_attribution import (
    attribute_session,
    load_session_telemetry,
    render_markdown_summary,
)
from telemetry_store import TelemetryStore


class LiveAttributionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.t0 = 1_800_000_000_000

    def _bbo_row(
        self,
        *,
        seq: int,
        venue: str,
        domain: str,
        market: str,
        avail_ms: int,
        bid: float,
        ask: float,
    ) -> dict:
        mid = (bid + ask) / 2.0
        spread_bps = ((ask - bid) / mid) * 10_000.0
        return {
            "venue": venue,
            "domain": domain,
            "market": market,
            "sequence": seq,
            "available_at_ms": avail_ms,
            "source_time_ms": avail_ms - 4,
            "bid_px_f64": bid,
            "ask_px_f64": ask,
            "mid_px_f64": mid,
            "spread_bps_f64": spread_bps,
            "tradable": True,
        }

    def test_causal_asof_attribution_lead_lag_markouts_and_fee_provenance(self) -> None:
        telem_dir = self.root / "telemetry"
        telem_dir.mkdir()
        db_path = self.root / "telemetry.sqlite3"
        sess_file = telem_dir / "live-nvda-01.jsonl"

        # Fill 1 (Buy @ 100.00): order created at t0+900, filled at telemetry ts_ms=t0+1020
        # (`resting_lifetime_ms = 120 < 250` -> fast sniped!) with exact `exchange_ts_ns = (t0+1000)*1e6`.
        # At t0+1000:
        #   - local (`lighter_robinhood:robinhood:1`) mid = 100.00
        #   - `arcus:mainnet:NVDA-USD` mid = 99.90 (-10 bps ahead -> informed seller hit our bid!)
        #   - `trade_xyz:hip3-xyz:xyz:NVDA` mid = 99.92 (-8 bps ahead)
        # Post-fill markouts on Arcus:
        #   - +100ms (t0+1100): mid = 99.85 (-15 bps)
        #   - +500ms (t0+1500): mid = 99.70 (-30 bps -> toxic < -2 bps!)
        #   - +5000ms (t0+6000): mid = 99.80 (-20 bps)
        #
        # Fill 2 (Sell @ 100.20): order created at t0+2000, filled at ts_ms=t0+3000 (`resting = 1000ms`),
        # no `exchange_ts_ns` (falls back to `telemetry_ts_ms`), `fee_source = "role_unknown"` -> unrated!
        # At t0+3000:
        #   - local mid = 100.10 (+9.98 bps edge)
        #   - Arcus mid = 100.10 (+9.98 bps edge, 0 bps lead vs local)
        #   - +100ms (t0+3100): Arcus mid = 100.05 (sell markout = +14.97 bps benign!)
        #   - +500ms (t0+3500): Arcus mid = 100.00 (sell markout = +19.96 bps benign!)
        records = [
            {
                "type": "session",
                "ts_ms": self.t0,
                "network": "RobinhoodMainnet",
                "exchange": "lighter",
                "symbol": "NVDA",
                "market_id": 1,
                "account": 42,
            },
            {
                "type": "order",
                "ts_ms": self.t0 + 900,
                "kind": "created",
                "client_order_id": 101,
                "order_index": 1,
                "side": "buy",
                "price": 100.00,
                "size": 1.0,
            },
            {
                "type": "order",
                "ts_ms": self.t0 + 1020,
                "exchange_ts_ns": (self.t0 + 1000) * 1_000_000,
                "kind": "filled",
                "client_order_id": 101,
                "order_index": 1,
                "side": "buy",
                "fill_price": 100.00,
                "fill_size": 1.0,
                "fee": 0.0,
                "fee_source": "explicit_zero",
                "trade_id": 7001,
                "remaining": 0.0,
            },
            {
                "type": "hedge_fill",
                "ts_ms": self.t0 + 1035,
                "venue": "TradeXYZ",
                "client_order_id": "hdg-1",
                "side": "sell",
                "price": 99.91,
                "price_source": "fill",
                "size": 1.0,
                "fee": 0.02,
                "fee_source": "configured_rate",
            },
            {
                "type": "order",
                "ts_ms": self.t0 + 2000,
                "kind": "created",
                "client_order_id": 102,
                "order_index": 2,
                "side": "sell",
                "price": 100.20,
                "size": 1.0,
            },
            {
                "type": "order",
                "ts_ms": self.t0 + 3000,
                "kind": "filled",
                "client_order_id": 102,
                "order_index": 2,
                "side": "sell",
                "fill_price": 100.20,
                "fill_size": 1.0,
                "fee": 0.0,
                "fee_source": "role_unknown",
                "trade_id": 7002,
                "remaining": 0.0,
            },
            {"type": "stopped", "ts_ms": self.t0 + 9000},
        ]
        sess_file.write_text("\n".join(json.dumps(r) for r in records) + "\n", encoding="utf-8")

        store = TelemetryStore(db_path, telem_dir)
        self.addCleanup(store.db.close)
        self.assertEqual(store.ingest(), len(records))

        # Test loading directly from compressed `.sqlite3.gz` hot-backup!
        backup_meta = atomic_sqlite_backup(db_path, self.root / "private_stage", snapshot_tag="unit")
        telemetry = load_session_telemetry(Path(backup_meta["path"]))
        self.assertEqual(telemetry["session"]["session_id"], "live-nvda-01")
        self.assertEqual(len(telemetry["fills"]), 2)
        self.assertEqual(len(telemetry["hedge_fills"]), 1)

        bbo_rows = [
            # Pre-fill quotes at t0+995 (<= t0+1000 alignment of Fill 1)
            self._bbo_row(seq=1, venue="lighter_robinhood", domain="robinhood", market="1", avail_ms=self.t0 + 995, bid=99.95, ask=100.05),
            self._bbo_row(seq=2, venue="arcus", domain="mainnet", market="NVDA-USD", avail_ms=self.t0 + 996, bid=99.85, ask=99.95),
            self._bbo_row(seq=3, venue="trade_xyz", domain="hip3-xyz", market="xyz:NVDA", avail_ms=self.t0 + 998, bid=99.87, ask=99.97),
            # Quote strictly AFTER t0+1000 (at t0+1005) must NOT leak into Fill 1's at-fill ASOF join!
            self._bbo_row(seq=4, venue="arcus", domain="mainnet", market="NVDA-USD", avail_ms=self.t0 + 1005, bid=95.00, ask=95.10),
            # Fill 1 horizons: +100ms (t0+1100), +500ms (t0+1500), +5000ms (t0+6000)
            self._bbo_row(seq=5, venue="arcus", domain="mainnet", market="NVDA-USD", avail_ms=self.t0 + 1102, bid=99.80, ask=99.90),
            self._bbo_row(seq=6, venue="lighter_robinhood", domain="robinhood", market="1", avail_ms=self.t0 + 1105, bid=99.85, ask=99.95),
            self._bbo_row(seq=7, venue="arcus", domain="mainnet", market="NVDA-USD", avail_ms=self.t0 + 1505, bid=99.65, ask=99.75),
            self._bbo_row(seq=8, venue="lighter_robinhood", domain="robinhood", market="1", avail_ms=self.t0 + 1510, bid=99.70, ask=99.80),
            # Fill 2 pre-fill & post-fill quotes around t0+3000
            self._bbo_row(seq=9, venue="lighter_robinhood", domain="robinhood", market="1", avail_ms=self.t0 + 2990, bid=100.05, ask=100.15),
            self._bbo_row(seq=10, venue="arcus", domain="mainnet", market="NVDA-USD", avail_ms=self.t0 + 2995, bid=100.05, ask=100.15),
            self._bbo_row(seq=11, venue="trade_xyz", domain="hip3-xyz", market="xyz:NVDA", avail_ms=self.t0 + 2996, bid=100.05, ask=100.15),
            self._bbo_row(seq=12, venue="arcus", domain="mainnet", market="NVDA-USD", avail_ms=self.t0 + 3100, bid=100.00, ask=100.10),
            self._bbo_row(seq=13, venue="arcus", domain="mainnet", market="NVDA-USD", avail_ms=self.t0 + 3500, bid=99.95, ask=100.05),
            self._bbo_row(seq=14, venue="arcus", domain="mainnet", market="NVDA-USD", avail_ms=self.t0 + 6010, bid=99.75, ask=99.85),
        ]
        bbo_path = self.root / "bbo_quotes.ndjson.gz"
        with gzip.open(bbo_path, "wt", encoding="utf-8") as fh:
            for item in bbo_rows:
                fh.write(json.dumps(item) + "\n")

        py_report = attribute_session(
            telemetry,
            bbo_ndjson_path=bbo_path,
            local_market_key="lighter_robinhood:robinhood:1",
            reference_market_keys=["arcus:mainnet:NVDA-USD", "trade_xyz:hip3-xyz:xyz:NVDA"],
            horizons_ms=(100, 500, 5_000),
            prefer_duckdb=False,
        )
        self.assertEqual(py_report["engine"], "python_causal_bisect")
        self.assertEqual(py_report["fill_count"], 2)
        self.assertEqual(py_report["fast_sniped_fills_lt_250ms"], 1)
        self.assertEqual(py_report["toxic_fills_500ms_lt_minus_2bps"], 1)
        # Because Fill 2 has `role_unknown` fee_source, session-level `all_fees_rated` MUST be False!
        self.assertFalse(py_report["all_fees_rated"])
        self.assertIsNone(py_report["mean_rated_fee_bps"])

        f1, f2 = py_report["fills"]
        self.assertEqual(f1["alignment_clock"], "exchange_ts_ns")
        self.assertEqual(f1["resting_lifetime_ms"], 120)
        self.assertEqual(f1["rated_fee_bps"], 0.0)
        # Causal check: t0+1005 quote (mid=95.05) did NOT leak into t0+1000 Fill 1!
        self.assertAlmostEqual(f1["references"]["arcus:mainnet:NVDA-USD"]["at_fill_mid"], 99.90)
        self.assertAlmostEqual(f1["references"]["arcus:mainnet:NVDA-USD"]["lead_vs_local_bps"], -10.0)
        self.assertAlmostEqual(f1["references"]["arcus:mainnet:NVDA-USD"]["markouts_bps"]["100ms"], -15.0)
        self.assertAlmostEqual(f1["references"]["arcus:mainnet:NVDA-USD"]["markouts_bps"]["500ms"], -30.0)
        self.assertAlmostEqual(f1["references"]["arcus:mainnet:NVDA-USD"]["markouts_bps"]["5000ms"], -20.0)

        self.assertEqual(f2["alignment_clock"], "telemetry_ts_ms")
        self.assertIsNone(f2["rated_fee_bps"])
        self.assertIsNone(f2["net_primary_markouts_after_fee_bps"]["500ms"])
        self.assertAlmostEqual(
            f2["references"]["arcus:mainnet:NVDA-USD"]["markouts_bps"]["500ms"],
            ((100.20 - 100.00) / 100.20) * 10_000.0,
        )

        md = render_markdown_summary(py_report)
        self.assertIn("`arcus:mainnet:NVDA-USD`", md)

        # Whenever DuckDB (module or CLI, e.g. on `aleph`) is installed, verify exact numerical parity!
        has_duck = (
            live_attribution._duckdb_mod is not None
            or shutil.which("duckdb") is not None
            or (Path.home() / ".local" / "bin" / "duckdb").is_file()
        )
        if has_duck:
            duck_report = attribute_session(
                telemetry,
                bbo_ndjson_path=bbo_path,
                local_market_key="lighter_robinhood:robinhood:1",
                reference_market_keys=["arcus:mainnet:NVDA-USD", "trade_xyz:hip3-xyz:xyz:NVDA"],
                horizons_ms=(100, 500, 5_000),
                prefer_duckdb=True,
            )
            self.assertEqual(duck_report["engine"], "duckdb_asof")
            for idx in range(2):
                for rk in ("arcus:mainnet:NVDA-USD", "trade_xyz:hip3-xyz:xyz:NVDA"):
                    d_ref = duck_report["fills"][idx]["references"][rk]
                    p_ref = py_report["fills"][idx]["references"][rk]
                    if p_ref["vs_fill_bps"] is None:
                        self.assertIsNone(d_ref["vs_fill_bps"])
                    else:
                        self.assertAlmostEqual(d_ref["vs_fill_bps"], p_ref["vs_fill_bps"], places=9)
                    for h_key, p_val in p_ref["markouts_bps"].items():
                        d_val = d_ref["markouts_bps"][h_key]
                        if p_val is None:
                            self.assertIsNone(d_val)
                        else:
                            self.assertAlmostEqual(d_val, p_val, places=9)


if __name__ == "__main__":
    unittest.main()
