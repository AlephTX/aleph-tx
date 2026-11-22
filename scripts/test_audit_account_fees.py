#!/usr/bin/env python3
"""Offline unit tests for `scripts/audit_account_fees.py`."""

from __future__ import annotations

import ast
import copy
from decimal import Decimal
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

try:
    from scripts.audit_account_fees import (
        ACCOUNT_FEE_STATUS_LIGHTER_PUBLIC_UNVERIFIED,
        ACCOUNT_FEE_STATUS_PUBLIC_TIER0,
        ACCOUNT_FEE_STATUS_UNKNOWN,
        ACCOUNT_FEE_STATUS_VERIFIED,
        SCHEMA_VERSION,
        SUPPORTED_VENUES,
        _ForbiddenEnvGuard,
        assert_redacted_evidence,
        audit_account_fees,
        build_embedded_self_test_fixtures,
        compute_hip3_execution_fees_bps,
        main as cli_main,
    )
except ImportError:
    from audit_account_fees import (
        ACCOUNT_FEE_STATUS_LIGHTER_PUBLIC_UNVERIFIED,
        ACCOUNT_FEE_STATUS_PUBLIC_TIER0,
        ACCOUNT_FEE_STATUS_UNKNOWN,
        ACCOUNT_FEE_STATUS_VERIFIED,
        SCHEMA_VERSION,
        SUPPORTED_VENUES,
        _ForbiddenEnvGuard,
        assert_redacted_evidence,
        audit_account_fees,
        build_embedded_self_test_fixtures,
        compute_hip3_execution_fees_bps,
        main as cli_main,
    )


def _run_cli(argv: list[str]) -> tuple[int, str, str]:
    stdout_buf = io.StringIO()
    stderr_buf = io.StringIO()
    with patch("sys.stdout", stdout_buf), patch("sys.stderr", stderr_buf):
        code = cli_main(argv)
    return code, stdout_buf.getvalue(), stderr_buf.getvalue()


class AuditAccountFeesTests(unittest.TestCase):
    def test_cli_refuses_default_and_invalid_mode_combinations(self) -> None:
        with patch("scripts.audit_account_fees._live_request_json", side_effect=AssertionError("network forbidden")):
            code_empty, out_empty, err_empty = _run_cli([])
            self.assertEqual(code_empty, 2)
            self.assertEqual(out_empty, "")
            self.assertIn("--live", err_empty)
            self.assertIn("--fixture", err_empty)
            self.assertIn("--self-test", err_empty)

            code_live_no_env, _, err_live = _run_cli(["--live"])
            self.assertEqual(code_live_no_env, 2)
            self.assertIn("--env", err_live)

            with tempfile.TemporaryDirectory() as tmp:
                mon = Path(tmp) / "monitor.env"
                mon.write_text("ALEPH_MONITOR_LIGHTER_INDEX=77\n", encoding="utf-8")
                code_st_env, _, err_st_env = _run_cli(["--self-test", "--monitor-env", str(mon)])
                self.assertEqual(code_st_env, 2)
                self.assertIn("only valid with --live", err_st_env)

    def test_hyperliquid_user_fees_decimal_parsing_and_trade_xyz_hip3_derivation(self) -> None:
        # Check compute_hip3_execution_fees_bps matches src/venues.rs across scale < 1, == 1, > 1, and growth_mode
        s_half = compute_hip3_execution_fees_bps(Decimal("1.50"), Decimal("4.50"), Decimal("0.5"), False)
        self.assertEqual(s_half["scale_factor"], "1.5")
        self.assertEqual(s_half["effective_maker_fee_bps"], "2.25")
        self.assertEqual(s_half["effective_taker_fee_bps"], "6.75")

        s_one_std = compute_hip3_execution_fees_bps(Decimal("1.50"), Decimal("4.50"), Decimal("1"), False)
        self.assertEqual(s_one_std["scale_factor"], "2")
        self.assertEqual(s_one_std["effective_maker_fee_bps"], "3")
        self.assertEqual(s_one_std["effective_taker_fee_bps"], "9")

        s_one_growth = compute_hip3_execution_fees_bps(Decimal("1.50"), Decimal("4.50"), Decimal("1"), True)
        self.assertEqual(s_one_growth["effective_maker_fee_bps"], "0.3")
        self.assertEqual(s_one_growth["effective_taker_fee_bps"], "0.9")

        s_two_std = compute_hip3_execution_fees_bps(Decimal("1.50"), Decimal("4.50"), Decimal("2"), False)
        self.assertEqual(s_two_std["effective_maker_fee_bps"], "6")
        self.assertEqual(s_two_std["effective_taker_fee_bps"], "18")

        with self.assertRaises(ValueError):
            compute_hip3_execution_fees_bps(Decimal("-0.1"), Decimal("4.5"), Decimal("1"), False)
        with self.assertRaises(ValueError):
            compute_hip3_execution_fees_bps(Decimal("1.5"), Decimal("4.5"), Decimal("1"), True, is_verified_core_crypto_asset=True)
        with self.assertRaises(ValueError):
            compute_hip3_execution_fees_bps(Decimal("1.5"), Decimal("4.5"), Decimal("5.5"), False)

        # Full fixture audit including negative HL core maker rebate and uniform growthMode on TradeXYZ
        fixtures = build_embedded_self_test_fixtures()
        fixtures["hyperliquid"]["userAddRate"] = "-0.00002"
        fixtures["hyperliquid"]["userCrossRate"] = "0.00038"
        # Uniform xyz markets all growthMode=enabled -> default maker/taker bps become unambiguous
        fixtures["trade_xyz"]["metaAndAssetCtxs"] = [
            {"universe": [{"name": "xyz:ANT", "growthMode": "enabled", "isDelisted": False}]},
            [{}],
        ]
        with _ForbiddenEnvGuard():
            report = audit_account_fees(
                SUPPORTED_VENUES,
                live=False,
                fixtures=fixtures,
                audited_at_utc="2026-11-22T00:00:00+00:00",
            )

        self.assertEqual(report["schema_version"], SCHEMA_VERSION)
        self.assertEqual(report["summary"]["error_venues"], 0)
        hl = report["venues"]["hyperliquid"]
        self.assertEqual(hl["account_fee_status"], ACCOUNT_FEE_STATUS_VERIFIED)
        self.assertEqual(hl["maker_fee_bps"], "-0.2")
        self.assertEqual(hl["taker_fee_bps"], "3.8")
        self.assertEqual(hl["active_referral_discount"], "0.04")
        self.assertEqual(hl["fee_schedule"]["base_cross_bps"], "4.5")
        self.assertEqual(hl["fee_schedule"]["tiers"]["mm"][0]["add_bps"], "-0.1")

        xyz = report["venues"]["trade_xyz"]
        self.assertEqual(xyz["account_fee_status"], ACCOUNT_FEE_STATUS_VERIFIED)
        self.assertEqual(xyz["deployer_fee_scale"], "1")
        self.assertEqual(xyz["scale_factor"], "2")
        self.assertEqual(xyz["standard_maker_fee_bps"], "3")
        self.assertEqual(xyz["standard_taker_fee_bps"], "9")
        self.assertEqual(xyz["growth_mode_maker_fee_bps"], "0.3")
        self.assertEqual(xyz["growth_mode_taker_fee_bps"], "0.9")
        # Unambiguous since all active xyz markets in fixture have growthMode=enabled
        self.assertEqual(xyz["maker_fee_bps"], "0.3")
        self.assertEqual(xyz["taker_fee_bps"], "0.9")

        # Verify sensitive fixture fields were stripped cleanly
        dumped = json.dumps(report)
        self.assertNotIn("dailyUserVlm", dumped)
        self.assertNotIn("stakingLink", dumped)
        self.assertNotIn("999999.12", dumped)
        self.assertNotIn("0x1111", dumped)
        self.assertNotIn("0x2222", dumped)
        self.assertNotIn("0x4444", dumped)
        self.assertNotIn("77889", dumped)
        self.assertNotIn("1234.56", dumped)

    def test_lighter_arcus_unconfigured_and_remaining_five_venues_fail_closed(self) -> None:
        fixtures = build_embedded_self_test_fixtures()
        with _ForbiddenEnvGuard():
            report = audit_account_fees(
                SUPPORTED_VENUES,
                live=False,
                fixtures=fixtures,
                audited_at_utc="2026-11-22T00:00:00+00:00",
            )

        lgt = report["venues"]["lighter"]
        self.assertEqual(lgt["account_fee_status"], ACCOUNT_FEE_STATUS_LIGHTER_PUBLIC_UNVERIFIED)
        self.assertEqual(lgt["account_type"], 1)
        self.assertEqual(lgt["account_type_label"], "account_type_1")
        self.assertEqual(lgt["public_schedule_maker_fee_bps"], "0")
        self.assertEqual(lgt["public_schedule_taker_fee_bps"], "0")
        self.assertIsNone(lgt["maker_fee_bps"])
        self.assertIsNone(lgt["taker_fee_bps"])

        arc = report["venues"]["arcus"]
        self.assertEqual(arc["account_fee_status"], ACCOUNT_FEE_STATUS_PUBLIC_TIER0)
        self.assertIsNone(arc["maker_fee_bps"])
        self.assertIsNone(arc["taker_fee_bps"])
        self.assertEqual(arc["public_tier0_schedule_summary"]["market_count"], 2)
        self.assertEqual(
            arc["public_tier0_schedule_summary"]["custom_schedule_tier0_bps"]["ANT-USD"],
            {"maker_fee_bps": "0", "taker_fee_bps": "1.12"},
        )

        for unprobed in ("backpack", "extended", "bulk", "okx", "binance"):
            row = report["venues"][unprobed]
            self.assertFalse(row["configured"])
            self.assertEqual(row["account_fee_status"], ACCOUNT_FEE_STATUS_UNKNOWN)
            self.assertIsNone(row["maker_fee_bps"])
            self.assertIsNone(row["taker_fee_bps"])

        # Explicit Lighter per-account maker_fee/taker_fee promotes to verified_account_tier
        verified_lgt_fixtures = copy.deepcopy(fixtures)
        verified_lgt_fixtures["lighter"]["accounts"][0]["maker_fee"] = "0.0040"
        verified_lgt_fixtures["lighter"]["accounts"][0]["taker_fee"] = "0.0280"
        rep_lgt_ver = audit_account_fees(["lighter"], live=False, fixtures=verified_lgt_fixtures)
        self.assertEqual(rep_lgt_ver["venues"]["lighter"]["account_fee_status"], ACCOUNT_FEE_STATUS_VERIFIED)
        self.assertEqual(rep_lgt_ver["venues"]["lighter"]["maker_fee_bps"], "0.4")
        self.assertEqual(rep_lgt_ver["venues"]["lighter"]["taker_fee_bps"], "2.8")

    def test_redaction_guard_rejects_pii_addresses_pubkeys_balances_and_secret_keys(self) -> None:
        base_valid = {
            "schema_version": SCHEMA_VERSION,
            "audited_at_utc": "2026-11-22T17:42:00+00:00",
            "venues": {
                "arcus": {"market_count": 42, "account_fee_status": ACCOUNT_FEE_STATUS_PUBLIC_TIER0},
                "lighter": {"account_type": 1, "account_fee_status": ACCOUNT_FEE_STATUS_LIGHTER_PUBLIC_UNVERIFIED},
            },
        }
        # Clean report with short index "42" should NOT false-positive on benign timestamp/market_count=42
        assert_redacted_evidence(base_valid, ("42", "0", "1"))

        bad_cases: list[tuple[dict, tuple[str, ...]]] = [
            ({"venues": {"hyperliquid": {"note": "0xAbCdEf0123456789012345678901234567890123"}}}, ()),
            ({"venues": {"bulk": {"note": "FuueqefENiGEW6uMqZQgmwjzgpnb85EgUcZa5Em4PQh7"}}}, ()),
            ({"venues": {"lighter": {"account_index": 77}}}, ("77",)),
            ({"venues": {"lighter": {"note": "index 77"}}}, ("77",)),
            ({"venues": {"lighter": {"l1_address": "redacted"}}}, ()),
            ({"venues": {"lighter": {"address": "something"}}}, ()),
            ({"venues": {"lighter": {"pubkey": "something"}}}, ()),
            ({"venues": {"hyperliquid": {"user": "someone"}}}, ()),
            ({"venues": {"hyperliquid": {"wallet": "someone"}}}, ()),
            ({"venues": {"hyperliquid": {"dailyUserVlm": []}}}, ()),
            ({"venues": {"hyperliquid": {"withdrawable": "10.0"}}}, ()),
            ({"venues": {"hyperliquid": {"accountValue": "100.0"}}}, ()),
            ({"venues": {"hyperliquid": {"marginUsed": "5.0"}}}, ()),
            ({"venues": {"lighter": {"collateral": "50.0"}}}, ()),
            ({"venues": {"lighter": {"positions": []}}}, ()),
            ({"venues": {"hyperliquid": {"openOrders": []}}}, ()),
            ({"venues": {"hyperliquid": {"totalRawUsd": "0"}}}, ()),
            ({"venues": {"okx": {"private_key": "abc"}}}, ()),
            ({"venues": {"binance": {"api_key": "abc"}}}, ()),
            ({"venues": {"okx": {"secret": "abc"}}}, ()),
            ({"venues": {"okx": {"token": "abc"}}}, ()),
            ({"venues": {"okx": {"mnemonic": "abc"}}}, ()),
            ({"venues": {"hyperliquid": {"error": "failed for customsecret99"}}}, ("customsecret99",)),
        ]
        for payload, lits in bad_cases:
            with self.subTest(payload=payload):
                with self.assertRaises(ValueError):
                    assert_redacted_evidence(payload, lits)

    def test_live_mode_uses_extract_public_monitor_config_isolates_errors_and_redacts_all_ids(self) -> None:
        hl_addr = "0x1111111111111111111111111111111111111111"
        hl_xyz = "0x2222222222222222222222222222222222222222"
        arc_addr = "0x3333333333333333333333333333333333333333"
        bulk_pk = "FuueqefENiGEW6uMqZQgmwjzgpnb85EgUcZa5Em4PQh7"
        lgt_idx = "987654"
        priv_val = "ab" * 32

        requested: list[tuple[str, dict | None]] = []

        def fake_requester(url: str, body: dict | None) -> object:
            requested.append((url, body))
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "userFees", "user": hl_addr}:
                return {
                    "userCrossRate": "0.00045",
                    "userAddRate": "0.00015",
                    "activeReferralDiscount": "0.0",
                    "dailyUserVlm": [{"date": "2026-11-22", "userCross": "55555"}],
                }
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "userFees", "user": hl_xyz}:
                return {"userCrossRate": "0.000405", "userAddRate": "0.000135", "activeReferralDiscount": "0.1"}
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "perpDexs"}:
                return [None, {"name": "xyz", "deployerFeeScale": "1.0", "deployer": "0x9999999999999999999999999999999999999999"}]
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "metaAndAssetCtxs", "dex": "xyz"}:
                return [{"universe": [{"name": "xyz:ANT", "growthMode": "enabled"}]}, [{}]]
            if url.startswith("https://mainnet.zklighter.elliot.ai/api/v1/account"):
                raise HTTPError(url, 503, "Service Unavailable", {}, None)
            if url == "https://api.arcus.xyz/v1/markets/feeSchedules":
                return build_embedded_self_test_fixtures()["arcus"]["feeSchedules"]
            raise AssertionError(f"unexpected call {url} {body}")

        with tempfile.TemporaryDirectory() as tmp:
            env_path = Path(tmp) / ".env"
            priv_key_name = "HYPERLIQUID_AGENT_" + "PRIVATE_" + "KEY"
            env_path.write_text(
                "\n".join(
                    [
                        f"HYPERLIQUID_ACCOUNT_ADDRESS={hl_addr}",
                        f"HYPERLIQUID_VAULT_ADDRESS={hl_xyz}",
                        f"{priv_key_name}={priv_val}",
                        f"ARCUS_ACCOUNT_ADDRESS={arc_addr}",
                        "ARCUS_ACCOUNT_INDEX=1",
                        f"LIGHTER_ACCOUNT_INDEX={lgt_idx}",
                        f"BULK_ACCOUNT_PUBKEY={bulk_pk}",
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            out_path = Path(tmp) / "fee_evidence.json"

            with patch("scripts.audit_account_fees._live_request_json", side_effect=fake_requester):
                code, stdout_str, stderr_str = _run_cli(
                    ["--live", "--env", str(env_path), "--output", str(out_path)]
                )

            # Lighter 503 -> isolated error_venues == 1 -> exit 1, still outputs full redacted JSON
            self.assertEqual(code, 1, msg=stderr_str)
            parsed = json.loads(stdout_str)
            self.assertEqual(parsed, json.loads(out_path.read_text(encoding="utf-8")))
            self.assertEqual(parsed["summary"]["error_venues"], 1)
            self.assertEqual(parsed["venues"]["lighter"]["status"], "error")
            self.assertEqual(parsed["venues"]["lighter"]["account_fee_status"], ACCOUNT_FEE_STATUS_UNKNOWN)
            self.assertEqual(parsed["venues"]["hyperliquid"]["account_fee_status"], ACCOUNT_FEE_STATUS_VERIFIED)
            self.assertEqual(parsed["venues"]["trade_xyz"]["account_fee_status"], ACCOUNT_FEE_STATUS_VERIFIED)
            self.assertFalse(parsed["venues"]["trade_xyz"]["uses_fallback_hl_address"])
            # 1.35 * 2 * 0.1 = 0.27; 4.05 * 2 * 0.1 = 0.81
            self.assertEqual(parsed["venues"]["trade_xyz"]["growth_mode_maker_fee_bps"], "0.27")
            self.assertEqual(parsed["venues"]["trade_xyz"]["growth_mode_taker_fee_bps"], "0.81")
            self.assertTrue(parsed["venues"]["arcus"]["account_identifier_configured"])

            for forbidden in (hl_addr, hl_xyz, arc_addr, bulk_pk, lgt_idx, priv_val, "0x9999", "55555"):
                self.assertNotIn(forbidden.lower(), stdout_str.lower())
                self.assertNotIn(forbidden.lower(), stderr_str.lower())

        # Unconfigured venues in live mode stay unconfigured + unknown without errors
        rep_unconf = audit_account_fees(
            ["hyperliquid", "trade_xyz", "lighter"],
            live=True,
            pub_cfg={},
            requester=lambda url, body: (_ for _ in ()).throw(AssertionError(f"unexpected {url}")),
        )
        self.assertEqual(rep_unconf["summary"]["error_venues"], 0)
        for v in ("hyperliquid", "trade_xyz", "lighter"):
            self.assertFalse(rep_unconf["venues"][v]["configured"])
            self.assertEqual(rep_unconf["venues"][v]["account_fee_status"], ACCOUNT_FEE_STATUS_UNKNOWN)

    def test_self_test_and_fixture_cli_and_static_ast_env_guard(self) -> None:
        with patch("scripts.audit_account_fees._live_request_json", side_effect=AssertionError("network forbidden")):
            with patch.dict(os.environ, {"ARCUS_API_PRIVATE_KEY": "secret-do-not-read"}, clear=False):
                code_st, out_st, err_st = _run_cli(["--self-test"])
            self.assertEqual(code_st, 0, msg=err_st)
            st_doc = json.loads(out_st)
            self.assertEqual(st_doc["self_test"], "passed")
            self.assertEqual(st_doc["schema_version"], SCHEMA_VERSION)
            self.assertTrue(st_doc["zero_secret_env_reads_verified"])
            self.assertNotIn("secret-do-not-read", out_st)

            with tempfile.TemporaryDirectory() as tmp:
                tmp_path = Path(tmp)
                fix_path = tmp_path / "fix.json"
                out_path = tmp_path / "out.json"
                fix_path.write_text(json.dumps(build_embedded_self_test_fixtures()), encoding="utf-8")
                code_fix, out_fix, err_fix = _run_cli(["--fixture", str(fix_path), "--output", str(out_path)])
                self.assertEqual(code_fix, 0, msg=err_fix)
                doc_fix = json.loads(out_fix)
                self.assertEqual(doc_fix, json.loads(out_path.read_text(encoding="utf-8")))
                self.assertEqual(len(doc_fix["venues"]), 9)

        # Static AST guard: only _build_ssl_context and _ForbiddenEnvGuard touch os.environ
        source_text = (REPO_ROOT / "scripts" / "audit_account_fees.py").read_text(encoding="utf-8")
        tree = ast.parse(source_text)
        env_refs: set[str] = set()
        for func in ast.walk(tree):
            if isinstance(func, ast.FunctionDef):
                for node in ast.walk(func):
                    if isinstance(node, ast.Attribute) and node.attr in ("environ", "environb", "getenv"):
                        env_refs.add(func.name)
        self.assertEqual(env_refs, {"_build_ssl_context", "__enter__", "__exit__"})

    def test_trade_xyz_fallback_address_scale_overrides_and_negative_base_rejection(self) -> None:
        hl_addr = "0x5555555555555555555555555555555555555555"

        def fake_req(url: str, body: dict | None) -> object:
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "userFees", "user": hl_addr}:
                return {"userCrossRate": "0.00045", "userAddRate": "0.00015"}
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "perpDexs"}:
                return [None, {"name": "xyz", "deployerFeeScale": "1.0"}]
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "metaAndAssetCtxs", "dex": "xyz"}:
                return [
                    {
                        "universe": [
                            {"name": "xyz:ANT", "growthMode": "enabled", "deployerFeeScale": "1.0"},
                            {"name": "xyz:NVDA", "growthMode": "disabled", "deployerFeeScale": "2.0"},
                        ]
                    },
                    [{}, {}],
                ]
            if url.startswith("https://mainnet.zklighter.elliot.ai/api/v1/account"):
                return {"code": 200, "accounts": [{"index": 12345, "account_type": 0}]}
            raise AssertionError(f"unexpected {url} {body}")

        with tempfile.TemporaryDirectory() as tmp:
            mon = Path(tmp) / "monitor.env"
            mon.write_text(
                f"ALEPH_MONITOR_HL_ADDRESS={hl_addr}\nALEPH_MONITOR_LIGHTER_INDEX=12345\n",
                encoding="utf-8",
            )
            with patch("scripts.audit_account_fees._live_request_json", side_effect=fake_req):
                code, out_str, err_str = _run_cli(
                    ["--live", "--monitor-env", str(mon), "--venues", "trade_xyz,lighter"]
                )
            self.assertEqual(code, 0, msg=err_str)
            doc = json.loads(out_str)
            xyz = doc["venues"]["trade_xyz"]
            self.assertTrue(xyz["uses_fallback_hl_address"])
            self.assertEqual(xyz["market_growth_mode_summary"]["observed_deployer_fee_scales"], ["1", "2"])
            self.assertEqual(len(xyz["market_growth_mode_summary"]["per_market_scale_overrides"]), 1)
            self.assertEqual(
                xyz["market_growth_mode_summary"]["per_market_scale_overrides"][0]["effective_taker_fee_bps"],
                "18",
            )
            lgt = doc["venues"]["lighter"]
            self.assertEqual(lgt["account_type"], 0)
            self.assertEqual(lgt["account_type_label"], "standard_type_0")
            self.assertEqual(lgt["account_fee_status"], ACCOUNT_FEE_STATUS_LIGHTER_PUBLIC_UNVERIFIED)
            self.assertNotIn(hl_addr.lower(), out_str.lower())
            self.assertNotIn("12345", out_str)

        # Negative maker rate fails closed on TradeXYZ (HIP-3 requires nonnegative base rates per src/venues.rs)
        neg_fix = build_embedded_self_test_fixtures()
        neg_fix["trade_xyz"]["userFees"]["userAddRate"] = "-0.00001"
        neg_rep = audit_account_fees(["trade_xyz"], live=False, fixtures=neg_fix)
        self.assertEqual(neg_rep["summary"]["error_venues"], 1)
        self.assertEqual(neg_rep["venues"]["trade_xyz"]["status"], "error")
        self.assertEqual(neg_rep["venues"]["trade_xyz"]["account_fee_status"], ACCOUNT_FEE_STATUS_UNKNOWN)

    def test_trade_xyz_handles_live_perp_dexs_without_dex_level_fee_scale(self) -> None:
        live_like_fix = {
            "trade_xyz": {
                "userFees": {"userCrossRate": "0.00045", "userAddRate": "0.00015"},
                "perpDexs": [None, {"name": "xyz", "fullName": "XYZ"}],
                "metaAndAssetCtxs": [
                    {
                        "universe": [
                            {"name": "xyz:ANT", "growthMode": "enabled", "deployerFeeScale": "1.0", "isDelisted": False},
                            {"name": "xyz:NVDA", "growthMode": "disabled", "deployerFeeScale": "1.0", "isDelisted": False},
                        ]
                    },
                    [{}, {}],
                ],
            }
        }
        rep = audit_account_fees(["trade_xyz"], live=False, fixtures=live_like_fix)
        self.assertEqual(rep["summary"]["error_venues"], 0)
        xyz = rep["venues"]["trade_xyz"]
        self.assertEqual(xyz["account_fee_status"], ACCOUNT_FEE_STATUS_VERIFIED)
        self.assertEqual(xyz["deployer_fee_scale"], "1")
        self.assertEqual(xyz["growth_mode_maker_fee_bps"], "0.3")
        self.assertEqual(xyz["growth_mode_taker_fee_bps"], "0.9")
        self.assertEqual(xyz["standard_maker_fee_bps"], "3")
        self.assertEqual(xyz["standard_taker_fee_bps"], "9")
        self.assertEqual(xyz["market_growth_mode_summary"]["per_market_scale_overrides"], [])


if __name__ == "__main__":
    unittest.main()
