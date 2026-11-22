#!/usr/bin/env python3
"""Offline unit tests for `scripts/audit_venue_specs.py` (`OPT-01`)."""

from __future__ import annotations

import ast
import copy
import hashlib
import io
import json
import os
from pathlib import Path
import ssl
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

try:
    from scripts.audit_venue_specs import (
        ACCOUNT_FEE_STATUS_UNKNOWN,
        NOT_EXPOSED_FEE_EVIDENCE,
        SUPPORTED_VENUES,
        _build_ssl_context,
        audit_venue,
        audit_venues,
        build_embedded_self_test_fixtures,
        compute_response_sha256,
        diff_spec_snapshots,
        main as cli_main,
    )
except ImportError:
    from audit_venue_specs import (
        ACCOUNT_FEE_STATUS_UNKNOWN,
        NOT_EXPOSED_FEE_EVIDENCE,
        SUPPORTED_VENUES,
        _build_ssl_context,
        audit_venue,
        audit_venues,
        build_embedded_self_test_fixtures,
        compute_response_sha256,
        diff_spec_snapshots,
        main as cli_main,
    )


def _run_cli(argv: list[str]) -> tuple[int, str, str]:
    stdout_buf = io.StringIO()
    stderr_buf = io.StringIO()
    with patch("sys.stdout", stdout_buf), patch("sys.stderr", stderr_buf):
        code = cli_main(argv)
    return code, stdout_buf.getvalue(), stderr_buf.getvalue()


class AuditVenueSpecsTests(unittest.TestCase):
    def test_all_nine_venue_parsers_and_unknown_account_fee_invariant(self) -> None:
        fixtures = build_embedded_self_test_fixtures()
        # Add realistic edge cases: RFQ market on Extended, quarterly contract on Binance,
        # numeric values on BULK, and non-0.0 public fee fields on Lighter.
        fixtures["extended"]["data"].append(
            {
                "name": "ETH-RFQ",
                "active": True,
                "status": "ACTIVE",
                "type": "PERPETUAL",
                "isRfq": True,
                "tradingConfig": {
                    "minOrderSize": "0.01",
                    "minOrderSizeChange": "0.01",
                    "minPriceChange": "0.1",
                    "maxLeverage": "25",
                },
            }
        )
        fixtures["binance"]["symbols"].append(
            {
                "symbol": "BTCUSDT_261225",
                "contractType": "CURRENT_QUARTER",
                "status": "TRADING",
                "filters": [
                    {"filterType": "PRICE_FILTER", "tickSize": "0.1"},
                    {"filterType": "LOT_SIZE", "stepSize": "0.001"},
                ],
            }
        )
        fixtures["bulk"].append(
            {
                "symbol": "BTC-USD",
                "baseAsset": "BTC",
                "quoteAsset": "USD",
                "status": "TRADING",
                "tickSize": 0.1,
                "sizeDecimals": 5,
                "minNotional": 10,
            }
        )

        report = audit_venues(
            SUPPORTED_VENUES,
            live=False,
            fixtures=fixtures,
            fetched_at_utc="2026-11-22T15:35:28+00:00",
        )
        self.assertEqual(report["summary"]["total_venues"], 9)
        self.assertEqual(report["summary"]["ok_venues"], 9)
        self.assertEqual(report["summary"]["error_venues"], 0)
        self.assertTrue(report["summary"]["all_account_fee_status_unknown"])

        venues = report["venues"]
        for venue_name in SUPPORTED_VENUES:
            row = venues[venue_name]
            self.assertEqual(row["status"], "ok", msg=venue_name)
            self.assertEqual(row["account_fee_status"], ACCOUNT_FEE_STATUS_UNKNOWN)
            self.assertGreaterEqual(row["market_count"], 1)
            self.assertEqual(len(row["markets"]), row["market_count"])
            self.assertTrue(row["response_sha256"])

        # Arcus: /v1/markets + /v1/markets/feeSchedules
        arcus_by_sym = {m["symbol"]: m for m in venues["arcus"]["markets"]}
        self.assertEqual(arcus_by_sym["BTC-USD"]["tick_size"], "0.1")
        self.assertEqual(arcus_by_sym["BTC-USD"]["step_size"], "0.001")
        self.assertEqual(arcus_by_sym["BTC-USD"]["min_notional"], "5")
        self.assertEqual(arcus_by_sym["BTC-USD"]["max_leverage"], 50.0)
        self.assertEqual(arcus_by_sym["ANT-USD"]["max_leverage"], 10.0)
        self.assertEqual(arcus_by_sym["ANT-USD"]["public_fee_evidence"]["tiers"][0]["taker_fee_bps"], "1.12")
        self.assertTrue(arcus_by_sym["ANT-USD"]["public_fee_evidence"]["has_custom_schedule"])

        # Hyperliquid core: szDecimals -> step/tick, absent fee schedule
        hl_by_sym = {m["symbol"]: m for m in venues["hyperliquid"]["markets"]}
        self.assertEqual(hl_by_sym["BTC"]["step_size"], "0.00001")
        self.assertEqual(hl_by_sym["BTC"]["tick_size"], "0.1")
        self.assertEqual(hl_by_sym["ETH"]["step_size"], "0.0001")
        self.assertEqual(hl_by_sym["ETH"]["tick_size"], "0.01")
        self.assertIsNone(hl_by_sym["BTC"]["min_notional"])
        self.assertEqual(hl_by_sym["BTC"]["max_leverage"], 50.0)
        self.assertEqual(hl_by_sym["BTC"]["public_fee_evidence"], NOT_EXPOSED_FEE_EVIDENCE)

        # TradeXYZ HIP-3: perpDexs + metaAndAssetCtxs
        xyz_ant = venues["trade_xyz"]["markets"][0]
        self.assertEqual(xyz_ant["symbol"], "xyz:ANT")
        self.assertEqual(xyz_ant["step_size"], "0.01")
        self.assertEqual(xyz_ant["tick_size"], "0.0001")
        self.assertEqual(xyz_ant["public_fee_evidence"]["growth_mode"], "enabled")
        self.assertEqual(xyz_ant["public_fee_evidence"]["deployer_fee_scale"], "1")
        self.assertEqual(xyz_ant["public_fee_evidence"]["asset_offset"], 110000)

        # Lighter: spot filtered out, decimals -> tick/step, public fields preserved, account status unknown
        self.assertEqual([m["symbol"] for m in venues["lighter"]["markets"]], ["ETH"])
        lgt_eth = venues["lighter"]["markets"][0]
        self.assertEqual(lgt_eth["tick_size"], "0.01")
        self.assertEqual(lgt_eth["step_size"], "0.0001")
        self.assertEqual(lgt_eth["min_notional"], "10")
        self.assertEqual(lgt_eth["max_leverage"], 50.0)
        self.assertEqual(lgt_eth["public_fee_evidence"]["maker_fee"], "0")
        self.assertEqual(lgt_eth["public_fee_evidence"]["liquidation_fee"], "1")
        self.assertEqual(lgt_eth["public_fee_evidence"]["market_status"], "active")

        # Backpack: SPOT filtered out, imfFunction -> max_leverage
        self.assertEqual([m["symbol"] for m in venues["backpack"]["markets"]], ["BTC_USDC_PERP"])
        bp_btc = venues["backpack"]["markets"][0]
        self.assertEqual(bp_btc["tick_size"], "0.1")
        self.assertEqual(bp_btc["step_size"], "0.001")
        self.assertEqual(bp_btc["max_leverage"], 50.0)
        self.assertEqual(bp_btc["public_fee_evidence"], NOT_EXPOSED_FEE_EVIDENCE)

        # Extended: RFQ filtered out
        self.assertEqual([m["symbol"] for m in venues["extended"]["markets"]], ["BTC-USD"])
        ext_btc = venues["extended"]["markets"][0]
        self.assertEqual(ext_btc["tick_size"], "1")
        self.assertEqual(ext_btc["step_size"], "0.0001")
        self.assertEqual(ext_btc["max_leverage"], 50.0)
        self.assertEqual(ext_btc["public_fee_evidence"], NOT_EXPOSED_FEE_EVIDENCE)

        # BULK: handles numeric and string fields and sizeDecimals fallback
        bulk_by_sym = {m["symbol"]: m for m in venues["bulk"]["markets"]}
        self.assertEqual(bulk_by_sym["NVDA-USD"]["tick_size"], "0.01")
        self.assertEqual(bulk_by_sym["NVDA-USD"]["step_size"], "0.001")
        self.assertEqual(bulk_by_sym["BTC-USD"]["step_size"], "0.00001")
        self.assertEqual(bulk_by_sym["BTC-USD"]["min_notional"], "10")

        # OKX: SWAP instId, lever, groupId fee group evidence
        okx_btc = venues["okx"]["markets"][0]
        self.assertEqual(okx_btc["symbol"], "BTC-USDT-SWAP")
        self.assertEqual(okx_btc["tick_size"], "0.1")
        self.assertEqual(okx_btc["step_size"], "0.01")
        self.assertIsNone(okx_btc["min_notional"])
        self.assertEqual(okx_btc["max_leverage"], 100.0)
        self.assertEqual(okx_btc["public_fee_evidence"]["groupId"], "1")

        # Binance: includes PERPETUAL & TRADIFI_PERPETUAL, excludes CURRENT_QUARTER, normalizes 0.10 -> 0.1
        bin_syms = [m["symbol"] for m in venues["binance"]["markets"]]
        self.assertEqual(bin_syms, ["BTCUSDT", "NVDAUSDT"])
        bin_by_sym = {m["symbol"]: m for m in venues["binance"]["markets"]}
        self.assertEqual(bin_by_sym["BTCUSDT"]["tick_size"], "0.1")
        self.assertEqual(bin_by_sym["BTCUSDT"]["step_size"], "0.001")
        self.assertEqual(bin_by_sym["BTCUSDT"]["min_notional"], "5")
        self.assertIsNone(bin_by_sym["BTCUSDT"]["max_leverage"])

    def test_deterministic_response_sha256_on_raw_bytes(self) -> None:
        raw_lighter = b'{"code":200,"order_book_details":[{"symbol":"ETH","market_type":"perp","size_decimals":4,"price_decimals":2,"min_quote_amount":"10"}]}'
        expected_hash = hashlib.sha256(raw_lighter).hexdigest()

        res_a = audit_venue("lighter", live=False, fixtures={"lighter": raw_lighter}, fetched_at_utc="2026-11-22T00:00:00+00:00")
        res_b = audit_venue("lighter", live=False, fixtures={"lighter": raw_lighter}, fetched_at_utc="2026-11-22T00:00:00+00:00")

        url = "https://mainnet.zklighter.elliot.ai/api/v1/orderBookDetails"
        self.assertEqual(res_a["response_sha256"][url], expected_hash)
        self.assertEqual(res_a["response_sha256"], res_b["response_sha256"])
        self.assertEqual(compute_response_sha256(raw_lighter), expected_hash)

        # Reordered or whitespace-changed raw bytes must change SHA-256 while preserving parsed spec
        raw_pretty = b'{\n  "code": 200,\n  "order_book_details": [{"symbol": "ETH", "market_type": "perp", "size_decimals": 4, "price_decimals": 2, "min_quote_amount": "10"}]\n}'
        res_c = audit_venue("lighter", live=False, fixtures={"lighter": raw_pretty})
        self.assertNotEqual(res_c["response_sha256"][url], expected_hash)
        self.assertEqual(res_c["markets"], res_a["markets"])

        # Shared Hyperliquid URL gets distinct provenance keys for perpDexs vs metaAndAssetCtxs
        hl_fixtures = build_embedded_self_test_fixtures()
        hl_res = audit_venue("trade_xyz", live=False, fixtures=hl_fixtures)
        self.assertEqual(len(hl_res["response_sha256"]), 2)
        self.assertEqual(
            set(hl_res["response_sha256"].keys()),
            {
                "https://api.hyperliquid.xyz/info#type=perpDexs",
                "https://api.hyperliquid.xyz/info#type=metaAndAssetCtxs&dex=xyz",
            },
        )

    def test_diff_spec_snapshots_clean_and_all_drift_classes(self) -> None:
        base_fixtures = build_embedded_self_test_fixtures()
        prev = audit_venues(SUPPORTED_VENUES, live=False, fixtures=base_fixtures, fetched_at_utc="2026-11-22T00:00:00+00:00")
        curr_same = audit_venues(SUPPORTED_VENUES, live=False, fixtures=copy.deepcopy(base_fixtures), fetched_at_utc="2026-11-22T01:00:00+00:00")

        clean = diff_spec_snapshots(prev, curr_same)
        self.assertFalse(clean["drift_detected"])
        self.assertEqual(clean["summary"]["drifted_venues"], 0)
        self.assertEqual(clean["summary"]["changed_markets"], 0)

        # Mutate across multiple drift dimensions: added/removed symbol, tick/step/min_notional/leverage/fee drift, status regression
        mutated = copy.deepcopy(base_fixtures)
        mutated["binance"]["symbols"].append(
            {
                "symbol": "ETHUSDT",
                "contractType": "PERPETUAL",
                "filters": [
                    {"filterType": "PRICE_FILTER", "tickSize": "0.01"},
                    {"filterType": "LOT_SIZE", "stepSize": "0.001"},
                    {"filterType": "MIN_NOTIONAL", "notional": "20"},
                ],
            }
        )
        mutated["arcus"]["markets"]["markets"] = [mutated["arcus"]["markets"]["markets"][0]]  # removed ANT-USD
        mutated["arcus"]["markets"]["markets"][0]["tickSize"] = "0.5"
        mutated["arcus"]["markets"]["markets"][0]["stepSize"] = "0.01"
        mutated["arcus"]["markets"]["markets"][0]["minOrderNotional"] = "25"
        mutated["arcus"]["markets"]["markets"][0]["initialMarginFraction"] = "0.05"
        mutated["trade_xyz"]["perpDexs"][1]["deployerFeeScale"] = "2.0"
        mutated["trade_xyz"]["metaAndAssetCtxs"][0]["universe"][0]["growthMode"] = "disabled"
        mutated["okx"] = {"code": "50000", "msg": "upstream error"}  # status regression ok -> error

        curr_drifted = audit_venues(SUPPORTED_VENUES, live=False, fixtures=mutated, fetched_at_utc="2026-11-22T02:00:00+00:00")
        diff = diff_spec_snapshots(prev, curr_drifted)
        self.assertTrue(diff["drift_detected"])
        self.assertEqual(diff["summary"]["status_regressions"], 1)
        self.assertEqual(diff["venues"]["binance"]["added_symbols"], ["ETHUSDT"])
        self.assertEqual(diff["venues"]["arcus"]["removed_symbols"], ["ANT-USD"])
        arcus_changes = diff["venues"]["arcus"]["changed_markets"][0]["changes"]
        self.assertEqual(set(arcus_changes.keys()), {"tick_size", "step_size", "min_notional", "max_leverage"})
        xyz_changes = diff["venues"]["trade_xyz"]["changed_markets"][0]["changes"]
        self.assertIn("public_fee_evidence", xyz_changes)
        self.assertTrue(diff["venues"]["okx"]["status_regression"])

        # CLI --diff + --fail-on-drift returns 2 on drift and 0 on clean match
        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            prev_path = tmp_path / "prev.json"
            prev_path.write_text(json.dumps(prev), encoding="utf-8")

            clean_fix_path = tmp_path / "clean_fix.json"
            clean_fix_path.write_text(json.dumps(base_fixtures), encoding="utf-8")
            out_clean = tmp_path / "out_clean.json"
            code_ok, stdout_ok, stderr_ok = _run_cli(
                ["--fixture", str(clean_fix_path), "--diff", str(prev_path), "--fail-on-drift", "--output", str(out_clean)]
            )
            self.assertEqual(code_ok, 0, msg=stderr_ok)
            self.assertFalse(json.loads(stdout_ok)["diff"]["drift_detected"])
            self.assertFalse(json.loads(out_clean.read_text(encoding="utf-8"))["diff"]["drift_detected"])

            drift_fix_path = tmp_path / "drift_fix.json"
            drift_fix_path.write_text(json.dumps(mutated), encoding="utf-8")
            code_drift, stdout_drift, _ = _run_cli(
                ["--fixture", str(drift_fix_path), "--diff", str(prev_path), "--fail-on-drift"]
            )
            self.assertEqual(code_drift, 2)
            self.assertTrue(json.loads(stdout_drift)["diff"]["drift_detected"])

            # Filtered --venues with --diff against full snapshot ignores unrequested venues
            code_filtered, stdout_filt, _ = _run_cli(
                ["--fixture", str(clean_fix_path), "--venues", "lighter,binance", "--diff", str(prev_path), "--fail-on-drift"]
            )
            self.assertEqual(code_filtered, 0)
            self.assertFalse(json.loads(stdout_filt)["diff"]["drift_detected"])

    def test_cli_fails_closed_without_mode_and_makes_zero_network_calls(self) -> None:
        with patch("scripts.audit_venue_specs._live_fetch_bytes", side_effect=AssertionError("network call forbidden")):
            code, stdout, stderr = _run_cli([])
            self.assertEqual(code, 2)
            self.assertEqual(stdout, "")
            self.assertIn("--live", stderr)
            self.assertIn("--fixture", stderr)
            self.assertIn("--self-test", stderr)

            # --fail-on-drift without --diff must also fail closed with exit code 2
            with tempfile.TemporaryDirectory() as tmp:
                fix = Path(tmp) / "f.json"
                fix.write_text(json.dumps(build_embedded_self_test_fixtures()), encoding="utf-8")
                code2, _, stderr2 = _run_cli(["--fixture", str(fix), "--fail-on-drift"])
                self.assertEqual(code2, 2)
                self.assertIn("--diff", stderr2)

            # Unknown venue must fail closed with exit code 2
            code3, _, stderr3 = _run_cli(["--self-test", "--venues", "nonexistent_venue"])
            self.assertEqual(code3, 2)
            self.assertIn("unsupported venue", stderr3)

    def test_error_isolation_preserves_other_venues_and_never_synthesizes_zero_fees(self) -> None:
        fixtures = build_embedded_self_test_fixtures()
        fixtures["arcus"]["feeSchedules"]["feeSchedules"][0]["tiers"][0]["makerFeePpm"] = "not-an-int"
        fixtures["lighter"] = "{broken-json"
        fixtures["extended"] = {"status": "ERROR", "data": []}
        fixtures["binance"] = {
            "symbols": [
                {
                    "symbol": "BTCUSDT",
                    "contractType": "PERPETUAL",
                    "filters": [
                        {"filterType": "PRICE_FILTER", "tickSize": "0"},  # invalid non-positive tick
                        {"filterType": "LOT_SIZE", "stepSize": "0.001"},
                    ],
                }
            ]
        }

        with tempfile.TemporaryDirectory() as tmp:
            fix_path = Path(tmp) / "broken.json"
            fix_path.write_text(json.dumps(fixtures), encoding="utf-8")
            code, stdout, _ = _run_cli(["--fixture", str(fix_path)])
            self.assertEqual(code, 1)
            report = json.loads(stdout)

            self.assertEqual(report["summary"]["ok_venues"], 5)
            self.assertEqual(report["summary"]["error_venues"], 4)
            self.assertTrue(report["summary"]["all_account_fee_status_unknown"])

            for bad in ("arcus", "lighter", "extended", "binance"):
                row = report["venues"][bad]
                self.assertEqual(row["status"], "error")
                self.assertEqual(row["account_fee_status"], ACCOUNT_FEE_STATUS_UNKNOWN)
                self.assertEqual(row["market_count"], 0)
                self.assertEqual(row["markets"], [])
                self.assertTrue(row["error"])

            # Lighter had invalid JSON bytes: provenance hash was still recorded!
            lgt_url = "https://mainnet.zklighter.elliot.ai/api/v1/orderBookDetails"
            self.assertEqual(
                report["venues"]["lighter"]["response_sha256"][lgt_url],
                hashlib.sha256(b"{broken-json").hexdigest(),
            )

            for good in ("hyperliquid", "trade_xyz", "backpack", "bulk", "okx"):
                self.assertEqual(report["venues"][good]["status"], "ok")
                self.assertGreater(report["venues"][good]["market_count"], 0)

    def test_mocked_live_mode_queries_only_public_endpoints_and_isolates_http_errors(self) -> None:
        fixtures = build_embedded_self_test_fixtures()
        observed_calls: list[tuple[str, str, dict | None]] = []

        def fake_live_fetch(url: str, *, method: str = "GET", body: dict | None = None) -> bytes:
            observed_calls.append((method, url, body))
            if url == "https://api.arcus.xyz/v1/markets":
                return json.dumps(fixtures["arcus"]["markets"]).encode("utf-8")
            if url == "https://api.arcus.xyz/v1/markets/feeSchedules":
                return json.dumps(fixtures["arcus"]["feeSchedules"]).encode("utf-8")
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "perpDexs"}:
                return json.dumps(fixtures["hyperliquid"]["perpDexs"]).encode("utf-8")
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "metaAndAssetCtxs"}:
                return json.dumps(fixtures["hyperliquid"]["metaAndAssetCtxs"]).encode("utf-8")
            if url == "https://mainnet-api1.bulk.trade/api/v1/exchangeInfo":
                raise HTTPError(url, 403, "Forbidden", {}, None)
            raise AssertionError(f"unexpected URL {method} {url} {body}")

        report = audit_venues(
            ("arcus", "hyperliquid", "bulk"),
            live=True,
            fetcher=fake_live_fetch,
            fetched_at_utc="2026-11-22T15:35:28+00:00",
        )
        self.assertEqual(report["mode"], "live")
        self.assertEqual(report["venues"]["arcus"]["status"], "ok")
        self.assertEqual(report["venues"]["hyperliquid"]["status"], "ok")
        self.assertEqual(report["venues"]["bulk"]["status"], "error")
        self.assertIn("403", report["venues"]["bulk"]["error"])
        self.assertEqual(report["venues"]["bulk"]["account_fee_status"], ACCOUNT_FEE_STATUS_UNKNOWN)
        self.assertEqual(
            observed_calls,
            [
                ("GET", "https://api.arcus.xyz/v1/markets", None),
                ("GET", "https://api.arcus.xyz/v1/markets/feeSchedules", None),
                ("POST", "https://api.hyperliquid.xyz/info", {"type": "perpDexs"}),
                ("POST", "https://api.hyperliquid.xyz/info", {"type": "metaAndAssetCtxs"}),
                ("GET", "https://mainnet-api1.bulk.trade/api/v1/exchangeInfo", None),
            ],
        )

    def test_self_test_and_no_secret_env_access(self) -> None:
        # Verify --self-test never touches secret env vars (or any env vars at all) and exits 0.
        with patch.dict(os.environ, {"ARCUS_API_PRIVATE_KEY": "should-never-be-read"}, clear=False):
            code, stdout, stderr = _run_cli(["--self-test"])
        self.assertEqual(code, 0, msg=stderr)
        summary = json.loads(stdout)
        self.assertEqual(summary["self_test"], "passed")
        self.assertEqual(summary["venues_tested"], list(SUPPORTED_VENUES))
        self.assertTrue(summary["all_account_fee_status_unknown"])
        self.assertTrue(summary["provenance_verified"])
        self.assertTrue(summary["zero_secret_env_reads_verified"])
        self.assertTrue(summary["diff_clean_verified"])
        self.assertTrue(summary["diff_drift_verified"])
        self.assertTrue(summary["error_isolation_verified"])
        self.assertNotIn("should-never-be-read", stdout)

        # Static AST guard: no non-docstring string literals reference .env or secret env names,
        # and only _build_ssl_context touches os.environ (solely for SSL_CERT_FILE).
        source_text = (REPO_ROOT / "scripts" / "audit_venue_specs.py").read_text(encoding="utf-8")
        tree = ast.parse(source_text)
        docstring_nodes: set[int] = set()
        for node in ast.walk(tree):
            if isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)) and node.body:
                first = node.body[0]
                if isinstance(first, ast.Expr) and isinstance(first.value, ast.Constant) and isinstance(first.value.value, str):
                    docstring_nodes.add(id(first.value))
        for node in ast.walk(tree):
            if isinstance(node, ast.Constant) and isinstance(node.value, str) and id(node) not in docstring_nodes:
                for forbidden in (".env", "PRIVATE_KEY", "API_KEY", "API_SECRET", "MNEMONIC"):
                    self.assertNotIn(forbidden, node.value)

        env_refs: set[str] = set()
        for func in ast.walk(tree):
            if isinstance(func, ast.FunctionDef):
                for node in ast.walk(func):
                    if isinstance(node, ast.Attribute) and node.attr in ("environ", "environb", "getenv"):
                        env_refs.add(func.name)
        self.assertEqual(env_refs, {"_build_ssl_context", "__enter__", "__exit__"})

    def test_arcus_schedule_mismatch_and_live_ssl_cert_file(self) -> None:
        fixtures = build_embedded_self_test_fixtures()
        fixtures["arcus"]["markets"]["markets"][0]["effectiveTakerFeePpm"] = 999
        row = audit_venue("arcus", live=False, fixtures=fixtures)
        self.assertEqual(row["status"], "error")
        self.assertIn("mismatch", row["error"])
        self.assertEqual(row["account_fee_status"], ACCOUNT_FEE_STATUS_UNKNOWN)

        with patch.dict(os.environ, {"SSL_CERT_FILE": "/tmp/custom-ca.pem"}, clear=False), patch(
            "ssl.create_default_context", return_value="mock-ctx"
        ) as create_ctx:
            ctx = _build_ssl_context()
        self.assertEqual(ctx, "mock-ctx")
        create_ctx.assert_called_once_with(cafile="/tmp/custom-ca.pem")

        with patch.dict(os.environ, {}, clear=True), patch(
            "ssl.create_default_context", return_value="default-ctx"
        ) as create_ctx_default:
            ctx_default = _build_ssl_context()
        self.assertEqual(ctx_default, "default-ctx")
        create_ctx_default.assert_called_once_with()

    def test_lighter_margin_fraction_priority_and_inactive_markets(self) -> None:
        # Active BTC prefers min_initial_margin_fraction=200 (50x) over default_initial_margin_fraction=500 (20x);
        # active SOL falls back to default_initial_margin_fraction=500 (20x);
        # inactive MKR with zero margin fractions parses with max_leverage=None and market_status="inactive";
        # inactive ARB without fee fields still emits source + market_status="inactive".
        payload = {
            "code": 200,
            "order_book_details": [
                {
                    "symbol": "BTC",
                    "market_id": 1,
                    "status": "active",
                    "market_type": "perp",
                    "size_decimals": 5,
                    "price_decimals": 1,
                    "min_quote_amount": "10",
                    "default_initial_margin_fraction": 500,
                    "min_initial_margin_fraction": 200,
                    "maker_fee": "0.0000",
                    "taker_fee": "0.0000",
                    "liquidation_fee": "1.0000",
                },
                {
                    "symbol": "SOL",
                    "market_id": 2,
                    "status": "active",
                    "market_type": "perp",
                    "size_decimals": 3,
                    "price_decimals": 3,
                    "min_quote_amount": "10",
                    "default_initial_margin_fraction": 500,
                },
                {
                    "symbol": "MKR",
                    "market_id": 28,
                    "status": "inactive",
                    "market_type": "perp",
                    "size_decimals": 4,
                    "price_decimals": 2,
                    "min_quote_amount": "10",
                    "default_initial_margin_fraction": 0,
                    "min_initial_margin_fraction": 0,
                    "maker_fee": "0.0000",
                    "taker_fee": "0.0000",
                    "liquidation_fee": "1.0000",
                },
                {
                    "symbol": "ARB",
                    "market_id": 29,
                    "status": "INACTIVE",
                    "market_type": "perp",
                    "size_decimals": 2,
                    "price_decimals": 4,
                    "default_initial_margin_fraction": 0,
                    "min_initial_margin_fraction": 0,
                },
                {
                    "symbol": "OP",
                    "market_id": 30,
                    "status": "inactive",
                    "market_type": "perp",
                    "size_decimals": 2,
                    "price_decimals": 4,
                    "default_initial_margin_fraction": 500,
                    "min_initial_margin_fraction": 0,
                },
            ],
        }
        res = audit_venue("lighter", live=False, fixtures={"lighter": payload})
        self.assertEqual(res["status"], "ok", msg=res.get("error"))
        by_sym = {m["symbol"]: m for m in res["markets"]}

        self.assertEqual(by_sym["BTC"]["max_leverage"], 50.0)
        self.assertEqual(by_sym["BTC"]["public_fee_evidence"]["market_status"], "active")
        self.assertEqual(by_sym["BTC"]["public_fee_evidence"]["market_id"], 1)

        self.assertEqual(by_sym["SOL"]["max_leverage"], 20.0)
        self.assertEqual(by_sym["SOL"]["public_fee_evidence"]["market_status"], "active")
        self.assertEqual(by_sym["SOL"]["public_fee_evidence"]["source"], "/api/v1/orderBookDetails")

        self.assertIsNone(by_sym["MKR"]["max_leverage"])
        self.assertEqual(by_sym["MKR"]["public_fee_evidence"]["market_status"], "inactive")
        self.assertEqual(by_sym["MKR"]["public_fee_evidence"]["market_id"], 28)
        self.assertEqual(by_sym["MKR"]["public_fee_evidence"]["maker_fee"], "0")

        self.assertIsNone(by_sym["ARB"]["max_leverage"])
        self.assertEqual(by_sym["ARB"]["public_fee_evidence"]["market_status"], "inactive")
        self.assertEqual(by_sym["ARB"]["public_fee_evidence"]["source"], "/api/v1/orderBookDetails")

        self.assertEqual(by_sym["OP"]["max_leverage"], 20.0)
        self.assertEqual(by_sym["OP"]["public_fee_evidence"]["market_status"], "inactive")

        # Fail closed on active or unspecified markets with zero margin fraction, negative margin on inactive,
        # or invalid non-empty status string.
        fail_cases = [
            {
                "symbol": "BTC",
                "market_id": 1,
                "status": "active",
                "market_type": "perp",
                "size_decimals": 4,
                "price_decimals": 2,
                "min_initial_margin_fraction": 0,
                "default_initial_margin_fraction": 500,
            },
            {
                "symbol": "BTC",
                "market_id": 1,
                "status": "active",
                "market_type": "perp",
                "size_decimals": 4,
                "price_decimals": 2,
                "default_initial_margin_fraction": 0,
            },
            {
                "symbol": "BTC",
                "market_id": 1,
                "market_type": "perp",
                "size_decimals": 4,
                "price_decimals": 2,
                "default_initial_margin_fraction": 0,
            },
            {
                "symbol": "MKR",
                "market_id": 28,
                "status": "inactive",
                "market_type": "perp",
                "size_decimals": 4,
                "price_decimals": 2,
                "min_initial_margin_fraction": -10,
                "default_initial_margin_fraction": 0,
            },
            {
                "symbol": "BTC",
                "market_id": 1,
                "status": "",
                "market_type": "perp",
                "size_decimals": 4,
                "price_decimals": 2,
                "min_initial_margin_fraction": 200,
            },
        ]
        for bad_row in fail_cases:
            bad_res = audit_venue(
                "lighter",
                live=False,
                fixtures={"lighter": {"code": 200, "order_book_details": [bad_row]}},
            )
            self.assertEqual(bad_res["status"], "error", msg=bad_row)
            self.assertTrue(bad_res["error"], msg=bad_row)

    def test_lighter_relisted_duplicate_symbol_disambiguation_and_guards(self) -> None:
        legacy_row = {
            "symbol": "ASELS",
            "market_id": 8286,
            "status": "inactive",
            "market_type": "perp",
            "size_decimals": 2,
            "price_decimals": 4,
            "default_initial_margin_fraction": 3333,
            "min_initial_margin_fraction": 3333,
        }
        active_row = {
            "symbol": "ASELS",
            "market_id": 10635,
            "status": "active",
            "market_type": "perp",
            "size_decimals": 2,
            "price_decimals": 4,
            "default_initial_margin_fraction": 3333,
            "min_initial_margin_fraction": 3333,
        }
        for order_name, rows in (
            ("inactive_first", [legacy_row, active_row]),
            ("active_first", [active_row, legacy_row]),
        ):
            with self.subTest(order=order_name):
                res = audit_venue(
                    "lighter",
                    live=False,
                    fixtures={"lighter": {"code": 200, "order_book_details": rows}},
                )
                self.assertEqual(res["status"], "ok", msg=res.get("error"))
                self.assertEqual(res["market_count"], 2)
                by_sym = {m["symbol"]: m for m in res["markets"]}
                self.assertIn("ASELS", by_sym)
                self.assertIn("ASELS#8286", by_sym)
                self.assertEqual(by_sym["ASELS"]["public_fee_evidence"]["market_id"], 10635)
                self.assertEqual(by_sym["ASELS"]["public_fee_evidence"]["market_status"], "active")
                self.assertEqual(by_sym["ASELS#8286"]["public_fee_evidence"]["market_id"], 8286)
                self.assertEqual(by_sym["ASELS#8286"]["public_fee_evidence"]["market_status"], "inactive")

        # Duplicate active symbol, duplicate market_id, or colliding inactive missing market_id must fail closed.
        invalid_batches = [
            (
                "duplicate_active_symbol",
                [
                    active_row,
                    dict(active_row, market_id=10636),
                ],
            ),
            (
                "duplicate_market_id",
                [
                    legacy_row,
                    dict(active_row, market_id=8286),
                ],
            ),
            (
                "colliding_inactive_missing_market_id",
                [
                    {k: v for k, v in legacy_row.items() if k != "market_id"},
                    active_row,
                ],
            ),
        ]
        for label, bad_rows in invalid_batches:
            with self.subTest(case=label):
                bad_res = audit_venue(
                    "lighter",
                    live=False,
                    fixtures={"lighter": {"code": 200, "order_book_details": bad_rows}},
                )
                self.assertEqual(bad_res["status"], "error", msg=label)
                self.assertTrue(bad_res["error"], msg=label)


if __name__ == "__main__":
    unittest.main()
