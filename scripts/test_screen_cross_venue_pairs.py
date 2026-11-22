#!/usr/bin/env python3
"""Offline unit tests for `scripts/screen_cross_venue_pairs.py`."""

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
        audit_account_fees,
        build_embedded_self_test_fixtures as build_account_fee_fixtures,
    )
    from scripts.screen_cross_venue_pairs import (
        ACCOUNT_FEE_STATUS_PUBLIC_TIER0,
        ACCOUNT_FEE_STATUS_PUBLIC_UNVERIFIED,
        ACCOUNT_FEE_STATUS_UNKNOWN,
        ACCOUNT_FEE_STATUS_VERIFIED,
        DOMAIN_COIN_M,
        DOMAIN_CRYPTO_QUOTE,
        DOMAIN_U,
        DOMAIN_USD1,
        DOMAIN_USDG,
        DOMAIN_USDH,
        DOMAIN_USDT,
        DOMAIN_USD_USDC,
        EXPECTED_ACCOUNT_FEE_EVIDENCE_SCHEMA,
        SUPPORTED_VENUES,
        build_embedded_self_test_snapshot,
        canonicalize_market,
        compute_step_lcm,
        main as cli_main,
        screen_pairs,
    )
except ImportError:
    from audit_account_fees import (
        audit_account_fees,
        build_embedded_self_test_fixtures as build_account_fee_fixtures,
    )
    from screen_cross_venue_pairs import (
        ACCOUNT_FEE_STATUS_PUBLIC_TIER0,
        ACCOUNT_FEE_STATUS_PUBLIC_UNVERIFIED,
        ACCOUNT_FEE_STATUS_UNKNOWN,
        ACCOUNT_FEE_STATUS_VERIFIED,
        DOMAIN_COIN_M,
        DOMAIN_CRYPTO_QUOTE,
        DOMAIN_U,
        DOMAIN_USD1,
        DOMAIN_USDG,
        DOMAIN_USDH,
        DOMAIN_USDT,
        DOMAIN_USD_USDC,
        EXPECTED_ACCOUNT_FEE_EVIDENCE_SCHEMA,
        SUPPORTED_VENUES,
        build_embedded_self_test_snapshot,
        canonicalize_market,
        compute_step_lcm,
        main as cli_main,
        screen_pairs,
    )


def _run_cli(argv: list[str]) -> tuple[int, str, str]:
    stdout_buf = io.StringIO()
    stderr_buf = io.StringIO()
    with patch("sys.stdout", stdout_buf), patch("sys.stderr", stderr_buf):
        code = cli_main(argv)
    return code, stdout_buf.getvalue(), stderr_buf.getvalue()


class ScreenCrossVenuePairsTests(unittest.TestCase):
    def test_symbol_canonicalization_across_all_nine_venues_and_exclusions(self) -> None:
        # Arcus: -USD stripped, KPEPE/KBONK normalized to 1000x, PEPE stays 1x, KAITO stays KAITO
        c_arc_ant = canonicalize_market(
            "arcus",
            {
                "symbol": "ANT-USD",
                "tick_size": "0.001",
                "step_size": "0.000001",
                "public_fee_evidence": {
                    "tiers": [{"level": 0, "maker_fee_bps": "0", "taker_fee_bps": "1.12"}]
                },
            },
        )
        assert c_arc_ant is not None
        self.assertEqual(c_arc_ant["base"], "ANT")
        self.assertEqual(c_arc_ant["quote_domain"], DOMAIN_USD_USDC)
        self.assertEqual(c_arc_ant["collateral_asset"], "USDC")
        self.assertEqual(c_arc_ant["asset_class_hint"], "equity")

        c_arc_kpepe = canonicalize_market("arcus", {"symbol": "KPEPE-USD", "tick_size": "0.000001", "step_size": "0.001"})
        c_arc_pepe = canonicalize_market("arcus", {"symbol": "PEPE-USD", "tick_size": "0.00001", "step_size": "0.0001"})
        assert c_arc_kpepe is not None and c_arc_pepe is not None
        self.assertEqual(c_arc_kpepe["base"], "1000PEPE")
        self.assertEqual(c_arc_kpepe["unit_multiplier"], 1000)
        self.assertEqual(c_arc_pepe["base"], "PEPE")
        self.assertEqual(c_arc_pepe["unit_multiplier"], 1)

        # TradeXYZ: xyz:* stripped, collateral_token 0->USDC, 3->USDH, other->HL_TOKEN_*, delisted excluded
        c_xyz_ant = canonicalize_market(
            "trade_xyz",
            {
                "symbol": "xyz:ANT",
                "tick_size": "0.0001",
                "step_size": "0.01",
                "public_fee_evidence": {"collateral_token": 0, "deployer_fee_scale": "1", "growth_mode": "enabled", "is_delisted": False},
            },
        )
        assert c_xyz_ant is not None
        self.assertEqual(c_xyz_ant["base"], "ANT")
        self.assertEqual(c_xyz_ant["quote_domain"], DOMAIN_USD_USDC)
        self.assertEqual(c_xyz_ant["collateral_asset"], "USDC")

        c_xyz_usdh = canonicalize_market(
            "trade_xyz",
            {"symbol": "xyz:TSLA", "public_fee_evidence": {"collateral_token": 3, "is_delisted": False}},
        )
        assert c_xyz_usdh is not None
        self.assertEqual(c_xyz_usdh["collateral_asset"], "USDH")
        self.assertEqual(c_xyz_usdh["quote_domain"], DOMAIN_USDH)

        c_xyz_other = canonicalize_market(
            "trade_xyz",
            {"symbol": "xyz:NVDA", "public_fee_evidence": {"collateral_token": 9, "is_delisted": False}},
        )
        assert c_xyz_other is not None
        self.assertEqual(c_xyz_other["collateral_asset"], "HL_TOKEN_9")
        self.assertEqual(c_xyz_other["quote_domain"], "HL_TOKEN_9_GROUP")

        self.assertIsNone(
            canonicalize_market("trade_xyz", {"symbol": "xyz:SNOW", "public_fee_evidence": {"is_delisted": True}})
        )

        # Hyperliquid Core: kPEPE -> 1000PEPE, KAITO stays KAITO
        c_hl_kpepe = canonicalize_market("hyperliquid", {"symbol": "kPEPE"})
        c_hl_kaito = canonicalize_market("hyperliquid", {"symbol": "KAITO"})
        assert c_hl_kpepe is not None and c_hl_kaito is not None
        self.assertEqual(c_hl_kpepe["base"], "1000PEPE")
        self.assertEqual(c_hl_kaito["base"], "KAITO")
        self.assertEqual(c_hl_kaito["unit_multiplier"], 1)

        # Lighter: active kept, inactive/#/_DELISTED excluded, 1000PEPE -> 1000PEPE
        c_lgt_ant = canonicalize_market(
            "lighter",
            {"symbol": "ANT", "public_fee_evidence": {"market_status": "active", "maker_fee": "0", "taker_fee": "0"}},
        )
        c_lgt_pepe = canonicalize_market(
            "lighter",
            {"symbol": "1000PEPE", "public_fee_evidence": {"market_status": "active", "maker_fee": "0", "taker_fee": "0"}},
        )
        assert c_lgt_ant is not None and c_lgt_pepe is not None
        self.assertEqual(c_lgt_ant["base"], "ANT")
        self.assertEqual(c_lgt_pepe["base"], "1000PEPE")
        self.assertIsNone(canonicalize_market("lighter", {"symbol": "ASELS#8286", "public_fee_evidence": {"market_status": "active"}}))
        self.assertIsNone(canonicalize_market("lighter", {"symbol": "MKR", "public_fee_evidence": {"market_status": "inactive"}}))
        self.assertIsNone(canonicalize_market("lighter", {"symbol": "SNDK_DELISTED", "public_fee_evidence": {"market_status": "active"}}))

        # Backpack: .US_USDC_PERP -> equity, kPEPE_USDC_PERP -> 1000PEPE, spot excluded
        c_bp_ant = canonicalize_market("backpack", {"symbol": "ANT.US_USDC_PERP"})
        c_bp_pepe = canonicalize_market("backpack", {"symbol": "kPEPE_USDC_PERP"})
        assert c_bp_ant is not None and c_bp_pepe is not None
        self.assertEqual(c_bp_ant["base"], "ANT")
        self.assertEqual(c_bp_ant["asset_class_hint"], "equity")
        self.assertEqual(c_bp_pepe["base"], "1000PEPE")
        self.assertIsNone(canonicalize_market("backpack", {"symbol": "BTC_USDC"}))

        # Extended: -USD stripped, _24_5 stripped with flag, PLACE_* excluded
        c_ext_nvda = canonicalize_market("extended", {"symbol": "NVDA_24_5-USD"})
        assert c_ext_nvda is not None
        self.assertEqual(c_ext_nvda["base"], "NVDA")
        self.assertEqual(c_ext_nvda["asset_class_hint"], "equity")
        self.assertEqual(c_ext_nvda["session_schedule"], "24_5")
        self.assertIn("extended_24_5_equity_hours", c_ext_nvda["flags"])
        self.assertIsNone(canonicalize_market("extended", {"symbol": "PLACE_JPY_1-USD"}))

        # BULK: -USD stripped
        c_bulk_ant = canonicalize_market("bulk", {"symbol": "ANT-USD"})
        assert c_bulk_ant is not None
        self.assertEqual(c_bulk_ant["base"], "ANT")
        self.assertEqual(c_bulk_ant["quote_domain"], DOMAIN_USD_USDC)

        # OKX: USDT-SWAP / USDC-SWAP / USD-SWAP separated
        c_okx_usdt = canonicalize_market("okx", {"symbol": "BTC-USDT-SWAP"})
        c_okx_usdc = canonicalize_market("okx", {"symbol": "BTC-USDC-SWAP"})
        c_okx_coinm = canonicalize_market("okx", {"symbol": "BTC-USD-SWAP"})
        assert c_okx_usdt is not None and c_okx_usdc is not None and c_okx_coinm is not None
        self.assertEqual(c_okx_usdt["quote_domain"], DOMAIN_USDT)
        self.assertEqual(c_okx_usdc["quote_domain"], DOMAIN_USD_USDC)
        self.assertEqual(c_okx_coinm["quote_domain"], DOMAIN_COIN_M)

        # Binance: USDT / USDC / USD1 / U / BTC / dated delivery
        c_bin_usdc = canonicalize_market("binance", {"symbol": "1000PEPEUSDC"})
        c_bin_usdt = canonicalize_market("binance", {"symbol": "BTCUSDT"})
        c_bin_1m = canonicalize_market("binance", {"symbol": "1MBABYDOGEUSDT"})
        c_bin_usd1 = canonicalize_market("binance", {"symbol": "BTCUSD1"})
        c_bin_u = canonicalize_market("binance", {"symbol": "BTCU"})
        c_bin_btc = canonicalize_market("binance", {"symbol": "ETHBTC"})
        assert all(x is not None for x in (c_bin_usdc, c_bin_usdt, c_bin_1m, c_bin_usd1, c_bin_u, c_bin_btc))
        self.assertEqual(c_bin_usdc["base"], "1000PEPE")
        self.assertEqual(c_bin_usdc["quote_domain"], DOMAIN_USD_USDC)
        self.assertEqual(c_bin_usdt["quote_domain"], DOMAIN_USDT)
        self.assertEqual(c_bin_1m["base"], "1000000BABYDOGE")
        self.assertEqual(c_bin_1m["unit_multiplier"], 1_000_000)
        self.assertEqual(c_bin_usd1["quote_domain"], DOMAIN_USD1)
        self.assertEqual(c_bin_u["quote_domain"], DOMAIN_U)
        self.assertEqual(c_bin_btc["quote_domain"], DOMAIN_CRYPTO_QUOTE)
        self.assertIsNone(canonicalize_market("binance", {"symbol": "BTCUSDT_261225"}))

    def test_exact_decimal_step_lcm_and_compatibility(self) -> None:
        lcm_1, ra_1, rb_1 = compute_step_lcm(Decimal("0.003"), Decimal("0.01"))
        self.assertEqual(lcm_1, Decimal("0.03"))
        self.assertEqual((ra_1, rb_1), (10, 3))

        lcm_2, ra_2, rb_2 = compute_step_lcm(Decimal("0.01"), Decimal("0.0001"))
        self.assertEqual(lcm_2, Decimal("0.01"))
        self.assertEqual((ra_2, rb_2), (1, 100))

        # Integer multiples with fine-to-coarse > 1000 (e.g. Arcus 1e-6 vs TradeXYZ 0.01) remain compatible
        snap = build_embedded_self_test_snapshot()
        res = screen_pairs(snap, venues=["arcus", "trade_xyz"])
        self.assertEqual(len(res["pairs"]), 1)
        ant_pair = res["pairs"][0]
        self.assertEqual(ant_pair["step_lcm"], "0.01")
        self.assertEqual(ant_pair["step_ratio_a"], 10000)
        self.assertEqual(ant_pair["step_ratio_b"], 1)
        self.assertEqual(ant_pair["step_lcm_amplification"], 1)
        self.assertTrue(ant_pair["step_compatible"])

        # Genuinely incompatible non-divisible step pair (min ratio > 1000) sets step_compatible=False
        # 1009 and 1013 are coprime primes -> lcm(0.001009, 0.001013) = 1.022117 (ratios 1013, 1009)
        bad_step_snap = {
            "schema_version": "opt01_venue_spec_audit_v1",
            "venues": {
                "arcus": {
                    "venue": "arcus",
                    "status": "ok",
                    "markets": [
                        {
                            "symbol": "FOO-USD",
                            "tick_size": "0.01",
                            "step_size": "0.001009",
                            "min_notional": "5",
                            "max_leverage": 10.0,
                            "public_fee_evidence": {"status": "not_exposed_in_public_endpoint"},
                        }
                    ],
                },
                "bulk": {
                    "venue": "bulk",
                    "status": "ok",
                    "markets": [
                        {
                            "symbol": "FOO-USD",
                            "tick_size": "0.01",
                            "step_size": "0.001013",
                            "min_notional": "10",
                            "max_leverage": 10.0,
                            "public_fee_evidence": {"status": "not_exposed_in_public_endpoint"},
                        }
                    ],
                },
            },
        }
        bad_res = screen_pairs(bad_step_snap, venues=["arcus", "bulk"])
        self.assertEqual(bad_res["pairs"][0]["step_lcm"], "1.022117")
        self.assertEqual(bad_res["pairs"][0]["step_ratio_a"], 1013)
        self.assertEqual(bad_res["pairs"][0]["step_ratio_b"], 1009)
        self.assertFalse(bad_res["pairs"][0]["step_compatible"])
        self.assertIn("step_lcm_amplification_gt_1000", bad_res["pairs"][0]["risk_flags"])

        # Missing or zero tick_size / step_size markets are excluded
        null_snap = copy.deepcopy(bad_step_snap)
        null_snap["venues"]["arcus"]["markets"][0]["tick_size"] = None
        null_res = screen_pairs(null_snap, venues=["arcus", "bulk"])
        self.assertEqual(null_res["summary"]["total_matched_pairs"], 0)
        self.assertEqual(null_res["summary"]["excluded_markets"]["missing_or_non_positive_tick_or_step"], 1)

    def test_quote_domain_guard_isolates_usdt_usdg_usdh_and_coin_m(self) -> None:
        snap = build_embedded_self_test_snapshot()
        # Add Lighter Robinhood USDG market and TradeXYZ USDH market to test strict domain isolation
        snap["venues"]["lighter"]["markets"].append(
            {
                "symbol": "BTC_USDG",
                "tick_size": "0.1",
                "step_size": "0.0001",
                "min_notional": "10",
                "max_leverage": 50.0,
                "public_fee_evidence": {
                    "source": "/api/v1/orderBookDetails",
                    "settlement_currency": "USDG",
                    "maker_fee": "0",
                    "taker_fee": "0",
                    "market_status": "active",
                },
            }
        )
        snap["venues"]["trade_xyz"]["markets"].append(
            {
                "symbol": "xyz:BTC",
                "tick_size": "0.1",
                "step_size": "0.0001",
                "min_notional": None,
                "max_leverage": 20.0,
                "public_fee_evidence": {
                    "collateral_token": 3,
                    "deployer_fee_scale": "1",
                    "growth_mode": "enabled",
                    "is_delisted": False,
                },
            }
        )

        # Default (allow_usdt_cross_quote=False): zero USDT, zero USDG, zero USDH, zero Coin-M pairs
        res_strict = screen_pairs(snap, allow_usdt_cross_quote=False)
        for pair in res_strict["pairs"]:
            self.assertEqual(pair["leg_a"]["quote_domain"], DOMAIN_USD_USDC)
            self.assertEqual(pair["leg_b"]["quote_domain"], DOMAIN_USD_USDC)
            self.assertTrue(pair["quote_domain_compatible"])
            self.assertFalse(pair["cross_quote_fx_risk"])
            self.assertNotIn("USDG", pair["leg_a"]["collateral_asset"])
            self.assertNotIn("USDG", pair["leg_b"]["collateral_asset"])
            self.assertNotIn("USDH", pair["leg_a"]["collateral_asset"])
            self.assertNotIn("USDH", pair["leg_b"]["collateral_asset"])

        # With allow_usdt_cross_quote=True: USDT pairs appear with explicit fx_risk,
        # while USDG / USDH / Coin-M STILL never merge with USDC or USDT!
        res_cross = screen_pairs(snap, allow_usdt_cross_quote=True)
        usdt_cross_pairs = [p for p in res_cross["pairs"] if p["cross_quote_fx_risk"]]
        self.assertGreater(len(usdt_cross_pairs), 0)
        for p in usdt_cross_pairs:
            self.assertFalse(p["quote_domain_compatible"])
            self.assertIn("cross_quote_fx_risk", p["risk_flags"])

        # OKX BTC-USDT-SWAP vs Binance BTCUSDT would be same-quote USDT if present; check no USDG/USDH/Coin-M anywhere
        for p in res_cross["pairs"]:
            for leg_key in ("leg_a", "leg_b"):
                self.assertIn(p[leg_key]["quote_domain"], (DOMAIN_USD_USDC, DOMAIN_USDT))
                self.assertNotEqual(p[leg_key]["symbol"], "BTC_USDG")
                self.assertNotEqual(p[leg_key]["symbol"], "xyz:BTC")
                self.assertNotEqual(p[leg_key]["symbol"], "BTC-USD-SWAP")

    def test_unknown_fees_never_default_to_zero_and_l0_floors_exact(self) -> None:
        snap = build_embedded_self_test_snapshot()
        res = screen_pairs(snap, allow_usdt_cross_quote=True)

        # Exposed pairs: Arcus ANT-USD (0 / 1.12) + TradeXYZ xyz:ANT (growthMode 0.30 / 0.90)
        arc_xyz = next(
            p
            for p in res["pairs"]
            if p["canonical_base"] == "ANT" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "trade_xyz"}
        )
        self.assertEqual(arc_xyz["fee_completeness"], "public_l0_both_legs")
        self.assertEqual(arc_xyz["round_trip_maker_taker_bps"], 1.8)
        self.assertEqual(arc_xyz["round_trip_dual_taker_bps"], 4.04)
        self.assertEqual(arc_xyz["recommended_topology"], "maker_first_arcus")

        # Unexposed fees on hyperliquid, backpack, extended, bulk, okx, binance stay None (null), NEVER 0.0
        unexposed_venues = {"hyperliquid", "backpack", "extended", "bulk", "okx", "binance"}
        for p in res["pairs"]:
            legs = (p["leg_a"], p["leg_b"])
            if any(leg["venue"] in unexposed_venues for leg in legs):
                self.assertIsNone(p["round_trip_maker_taker_bps"], msg=p["pair_id"])
                self.assertIsNone(p["round_trip_dual_taker_bps"], msg=p["pair_id"])
                for leg in legs:
                    if leg["venue"] in unexposed_venues:
                        self.assertIsNone(leg["maker_fee_bps"], msg=leg)
                        self.assertIsNone(leg["taker_fee_bps"], msg=leg)
                        self.assertEqual(leg["account_fee_status"], ACCOUNT_FEE_STATUS_UNKNOWN)

    def test_lighter_topology_never_allows_concurrent_protected_ioc(self) -> None:
        snap = build_embedded_self_test_snapshot()
        res = screen_pairs(snap, allow_usdt_cross_quote=True)
        lighter_pairs = [p for p in res["pairs"] if "lighter" in (p["leg_a"]["venue"], p["leg_b"]["venue"])]
        self.assertGreater(len(lighter_pairs), 0)
        for p in lighter_pairs:
            self.assertNotEqual(p["recommended_topology"], "concurrent_protected_ioc")
            self.assertEqual(p["recommended_topology"], "maker_first_lighter")
            self.assertFalse(p["concurrent_protected_ioc_allowed"])
            self.assertEqual(p["maker_leg_venue"], "lighter")

        # Fast-fast equal-fee pair (e.g. Arcus crypto 1.5/4.5 + Arcus-like HL? Or when HL fees are unknown -> concurrent allowed)
        hl_arc_btc = next(
            p
            for p in res["pairs"]
            if p["canonical_base"] == "BTC" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "hyperliquid"}
        )
        self.assertTrue(hl_arc_btc["concurrent_protected_ioc_allowed"])
        self.assertEqual(hl_arc_btc["min_notional_floor_usd"], 10.0)

    def test_mocked_live_funding_probe_computes_dislocation_and_isolates_http_errors(self) -> None:
        snap = build_embedded_self_test_snapshot()
        observed_calls: list[tuple[str, str, dict | None]] = []

        def fake_fetcher(url: str, *, method: str = "GET", body: dict | None = None) -> bytes:
            observed_calls.append((method, url, body))
            if url == "https://api.arcus.xyz/v1/markets":
                return json.dumps(
                    {
                        "markets": [
                            {
                                "marketDisplayName": "ANT-USD",
                                "markPrice": "100.0",
                                "fundingRate": "0.00015",  # 1.5 bps/hr
                                "volume24hNotional": "2500000",
                            },
                            {
                                "marketDisplayName": "BTC-USD",
                                "markPrice": "90000.0",
                                "fundingRate": "0.00001",  # 0.1 bps/hr
                                "volume24hNotional": "50000000",
                            },
                        ]
                    }
                ).encode("utf-8")
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "metaAndAssetCtxs", "dex": "xyz"}:
                return json.dumps(
                    [
                        {"universe": [{"name": "xyz:ANT", "isDelisted": False}]},
                        [
                            {
                                "midPx": "100.2",
                                "markPx": "100.15",
                                "funding": "0.00002",  # 0.2 bps/hr
                                "dayNtlVlm": "1800000",
                                "openInterest": "9000",
                            }
                        ],
                    ]
                ).encode("utf-8")
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "metaAndAssetCtxs"}:
                return json.dumps(
                    [
                        {"universe": [{"name": "BTC"}, {"name": "kPEPE"}]},
                        [
                            {"midPx": "90090.0", "funding": "0.00003", "dayNtlVlm": "800000000", "openInterest": "20000"},
                            {"midPx": "0.01", "funding": "0.00005", "dayNtlVlm": "50000000", "openInterest": "100000000"},
                        ],
                    ]
                ).encode("utf-8")
            if url == "https://mainnet.zklighter.elliot.ai/api/v1/funding-rates":
                # Simulate HTTP error on Lighter funding-rates while orderBookDetails still succeeds
                raise HTTPError(url, 503, "Service Unavailable", {}, None)
            if url == "https://mainnet.zklighter.elliot.ai/api/v1/orderBookDetails":
                return json.dumps(
                    {
                        "code": 200,
                        "order_book_details": [
                            {
                                "symbol": "ANT",
                                "market_id": 150,
                                "market_type": "perp",
                                "status": "active",
                                "last_trade_price": 100.1,
                                "daily_quote_token_volume": 750000,
                            }
                        ],
                    }
                ).encode("utf-8")
            raise AssertionError(f"unexpected URL {method} {url} {body}")

        res = screen_pairs(
            snap,
            venues=["arcus", "hyperliquid", "trade_xyz", "lighter"],
            live_funding_probe=True,
            probe_fetcher=fake_fetcher,
        )

        # Verify ONLY public endpoints called and Lighter funding error isolated gracefully
        self.assertEqual(len(observed_calls), 5)
        self.assertEqual(res["live_probe_status"]["arcus"]["status"], "ok")
        self.assertEqual(res["live_probe_status"]["trade_xyz"]["status"], "ok")
        self.assertEqual(res["live_probe_status"]["hyperliquid"]["status"], "ok")
        self.assertEqual(res["live_probe_status"]["lighter"]["status"], "partial_error")
        self.assertIn("503", res["live_probe_status"]["lighter"]["error"])

        ant_arc_xyz = next(
            p
            for p in res["pairs"]
            if p["canonical_base"] == "ANT" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "trade_xyz"}
        )
        lp = ant_arc_xyz["live_probe"]
        self.assertEqual(lp["mid_a"], 100.0)
        self.assertEqual(lp["mid_b"], 100.2)
        # |100 - 100.2| / 100.1 * 10000 = 19.9800 bps
        self.assertAlmostEqual(lp["mid_dislocation_bps"], 19.98, places=2)
        self.assertEqual(lp["funding_a_hourly_bps"], 1.5)
        self.assertEqual(lp["funding_b_hourly_bps"], 0.2)
        self.assertEqual(lp["funding_diff_hourly_bps"], 1.3)
        # 1.3 bps/hr * 8760 / 100 = 113.88% APR
        self.assertAlmostEqual(lp["funding_annualized_pct"], 113.88, places=2)
        self.assertEqual(lp["open_interest_or_vol_24h"]["min_usd"], 1800000.0)

        # Lighter pair has mid from orderBookDetails, funding=None due to isolated 503
        ant_arc_lgt = next(
            p
            for p in res["pairs"]
            if p["canonical_base"] == "ANT" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "lighter"}
        )
        self.assertEqual(ant_arc_lgt["live_probe"]["mid_b"], 100.1)
        self.assertIsNone(ant_arc_lgt["live_probe"]["funding_b_hourly_bps"])
        self.assertIsNone(ant_arc_lgt["live_probe"]["funding_diff_hourly_bps"])
        self.assertIsNone(ant_arc_lgt["live_probe"]["funding_annualized_pct"])

        # Also test Lighter 8h -> 1h normalization when funding-rates succeeds and ignores non-lighter rows
        def fake_lighter_only(url: str, *, method: str = "GET", body: dict | None = None) -> bytes:
            if "funding-rates" in url:
                return json.dumps(
                    {
                        "code": 200,
                        "funding_rates": [
                            {"market_id": 150, "exchange": "binance", "symbol": "ANT", "rate": 0.999},
                            {"market_id": 150, "exchange": "lighter", "symbol": "ANT", "rate": 0.0008},  # 0.0008/8*10000 = 1.0 bps/hr
                        ],
                    }
                ).encode("utf-8")
            if "orderBookDetails" in url:
                return json.dumps(
                    {
                        "code": 200,
                        "order_book_details": [
                            {"symbol": "ANT", "market_id": 150, "market_type": "perp", "status": "active", "last_trade_price": 100.0}
                        ],
                    }
                ).encode("utf-8")
            return fake_fetcher(url, method=method, body=body)

        res2 = screen_pairs(
            snap,
            venues=["arcus", "lighter"],
            live_funding_probe=True,
            probe_fetcher=fake_lighter_only,
        )
        p_lgt = next(p for p in res2["pairs"] if p["canonical_base"] == "ANT")
        self.assertEqual(p_lgt["live_probe"]["funding_b_hourly_bps"], 1.0)
        self.assertEqual(p_lgt["live_probe"]["funding_diff_hourly_bps"], 0.5)

    def test_cli_fail_closed_self_test_and_no_secret_env_reads(self) -> None:
        # Offline mode without --live-funding-probe must make zero network calls
        with patch("scripts.screen_cross_venue_pairs._live_fetch_bytes", side_effect=AssertionError("network forbidden")):
            code_empty, stdout_empty, stderr_empty = _run_cli([])
            self.assertEqual(code_empty, 2)
            self.assertEqual(stdout_empty, "")
            self.assertIn("--snapshot", stderr_empty)
            self.assertIn("--self-test", stderr_empty)

            code_bad_combo, _, stderr_combo = _run_cli(["--self-test", "--live-funding-probe"])
            self.assertEqual(code_bad_combo, 2)
            self.assertIn("strictly offline", stderr_combo)

            code_bad_venue, _, stderr_venue = _run_cli(["--self-test", "--venues", "unknown_dex"])
            self.assertEqual(code_bad_venue, 2)
            self.assertIn("unsupported venue", stderr_venue)

            with patch.dict(os.environ, {"ARCUS_API_PRIVATE_KEY": "do-not-read-me"}, clear=False):
                code_st, stdout_st, stderr_st = _run_cli(["--self-test"])
            self.assertEqual(code_st, 0, msg=stderr_st)
            st_doc = json.loads(stdout_st)
            self.assertEqual(st_doc["self_test"], "passed")
            self.assertEqual(st_doc["venues_tested"], list(SUPPORTED_VENUES))
            self.assertTrue(st_doc["zero_secret_env_reads_verified"])
            self.assertNotIn("do-not-read-me", stdout_st)

            # Snapshot CLI + --top + --output
            with tempfile.TemporaryDirectory() as tmp:
                tmp_path = Path(tmp)
                snap_path = tmp_path / "snap.json"
                out_path = tmp_path / "out.json"
                snap_path.write_text(json.dumps(build_embedded_self_test_snapshot()), encoding="utf-8")
                code_snap, stdout_snap, stderr_snap = _run_cli(
                    ["--snapshot", str(snap_path), "--top", "3", "--output", str(out_path)]
                )
                self.assertEqual(code_snap, 0, msg=stderr_snap)
                parsed_out = json.loads(out_path.read_text(encoding="utf-8"))
                self.assertEqual(parsed_out, json.loads(stdout_snap))
                self.assertEqual(parsed_out["summary"]["returned_pairs"], 3)
                self.assertEqual(len(parsed_out["pairs"]), 3)

        # Static AST guard: no non-docstring string literals reference .env or secret env names,
        # and only _build_ssl_context + _ForbiddenEnvGuard reference os.environ.
        source_text = (REPO_ROOT / "scripts" / "screen_cross_venue_pairs.py").read_text(encoding="utf-8")
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

    def test_account_fee_evidence_integration_growth_mode_standard_and_fail_closed_unverified(self) -> None:
        snap = build_embedded_self_test_snapshot()
        # Add xyz:NVDA (standard growthMode=disabled, scale=2) and Arcus NVDA-USD + HL SOL + xyz:SOL to test both legs verified
        snap["venues"]["arcus"]["markets"].append(
            {
                "symbol": "NVDA-USD",
                "tick_size": "0.01",
                "step_size": "0.001",
                "min_notional": "5",
                "max_leverage": 10.0,
                "public_fee_evidence": {
                    "source": "/v1/markets/feeSchedules",
                    "tiers": [{"level": 0, "maker_fee_bps": "0", "taker_fee_bps": "2.25"}],
                },
            }
        )
        snap["venues"]["trade_xyz"]["markets"].append(
            {
                "symbol": "xyz:NVDA",
                "tick_size": "0.01",
                "step_size": "0.001",
                "min_notional": None,
                "max_leverage": 10.0,
                "public_fee_evidence": {
                    "source": "perpDexs+metaAndAssetCtxs",
                    "dex": "xyz",
                    "collateral_token": 0,
                    "deployer_fee_scale": "2",
                    "growth_mode": "disabled",
                    "is_delisted": False,
                },
            }
        )
        snap["venues"]["hyperliquid"]["markets"].append(
            {
                "symbol": "SOL",
                "tick_size": "0.01",
                "step_size": "0.01",
                "min_notional": None,
                "max_leverage": 20.0,
                "public_fee_evidence": {"status": "not_exposed_in_public_endpoint"},
            }
        )
        snap["venues"]["trade_xyz"]["markets"].append(
            {
                "symbol": "xyz:SOL",
                "tick_size": "0.01",
                "step_size": "0.01",
                "min_notional": None,
                "max_leverage": 20.0,
                "public_fee_evidence": {
                    "source": "perpDexs+metaAndAssetCtxs",
                    "dex": "xyz",
                    "collateral_token": 0,
                    "deployer_fee_scale": "1",
                    "growth_mode": "enabled",
                    "is_delisted": False,
                },
            }
        )

        # Build realistic account fee evidence directly from audit_account_fees()
        acct_fixtures = build_account_fee_fixtures()
        acct_fixtures["trade_xyz"]["userFees"]["userAddRate"] = "0.000144"
        acct_fixtures["trade_xyz"]["userFees"]["userCrossRate"] = "0.000432"
        evidence = audit_account_fees(
            SUPPORTED_VENUES,
            live=False,
            fixtures=acct_fixtures,
            audited_at_utc="2026-11-22T00:00:00+00:00",
        )

        res = screen_pairs(snap, allow_usdt_cross_quote=True, account_fee_evidence=evidence)
        self.assertEqual(res["options"]["account_fee_evidence_schema"], EXPECTED_ACCOUNT_FEE_EVIDENCE_SCHEMA)
        self.assertEqual(res["summary"]["account_fee_status"], "partial_verified_account_tier")
        self.assertEqual(res["summary"]["verified_account_fee_venues"], ["hyperliquid", "trade_xyz"])

        # 1) Arcus BTC (1.5 / 4.5) + Hyperliquid verified BTC (1.44 / 4.32):
        #    min MT = min(1.5 + 4.32, 1.44 + 4.5) = 5.82 -> RT = 11.64; DT = 4.5 + 4.32 = 8.82 -> RT = 17.64
        btc_arc_hl = next(
            p
            for p in res["pairs"]
            if p["canonical_base"] == "BTC" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "hyperliquid"}
        )
        self.assertEqual(btc_arc_hl["fee_completeness"], "mixed_verified_account_and_public_l0")
        self.assertEqual(btc_arc_hl["round_trip_maker_taker_bps"], 11.64)
        self.assertEqual(btc_arc_hl["round_trip_dual_taker_bps"], 17.64)
        hl_leg = btc_arc_hl["leg_b"] if btc_arc_hl["leg_b"]["venue"] == "hyperliquid" else btc_arc_hl["leg_a"]
        self.assertEqual(hl_leg["account_fee_status"], ACCOUNT_FEE_STATUS_VERIFIED)
        self.assertEqual(hl_leg["fee_provenance"], "verified_account_fee_evidence")

        # 2) Arcus ANT (0 / 1.12) + TradeXYZ xyz:ANT (growthMode=enabled, scale=1, base 1.44/4.32 -> 0.288 / 0.864)
        ant_arc_xyz = next(
            p
            for p in res["pairs"]
            if p["canonical_base"] == "ANT" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "trade_xyz"}
        )
        self.assertEqual(ant_arc_xyz["fee_completeness"], "mixed_verified_account_and_public_l0")
        self.assertEqual(ant_arc_xyz["round_trip_maker_taker_bps"], 1.728)
        self.assertEqual(ant_arc_xyz["round_trip_dual_taker_bps"], 3.968)
        xyz_ant_leg = ant_arc_xyz["leg_b"] if ant_arc_xyz["leg_b"]["venue"] == "trade_xyz" else ant_arc_xyz["leg_a"]
        self.assertEqual(xyz_ant_leg["maker_fee_bps"], "0.288")
        self.assertEqual(xyz_ant_leg["taker_fee_bps"], "0.864")
        self.assertEqual(xyz_ant_leg["account_fee_status"], ACCOUNT_FEE_STATUS_VERIFIED)

        # 3) Arcus NVDA (0 / 2.25) + TradeXYZ xyz:NVDA (growthMode=disabled, scale=2 -> scale_factor=4 -> 5.76 / 17.28)
        nvda_arc_xyz = next(
            p
            for p in res["pairs"]
            if p["canonical_base"] == "NVDA" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "trade_xyz"}
        )
        xyz_nvda_leg = nvda_arc_xyz["leg_b"] if nvda_arc_xyz["leg_b"]["venue"] == "trade_xyz" else nvda_arc_xyz["leg_a"]
        self.assertEqual(xyz_nvda_leg["maker_fee_bps"], "5.76")
        self.assertEqual(xyz_nvda_leg["taker_fee_bps"], "17.28")
        # min MT = min(0 + 17.28, 5.76 + 2.25) = 8.01 -> RT = 16.02 (maker_first_trade_xyz)
        self.assertEqual(nvda_arc_xyz["recommended_topology"], "maker_first_trade_xyz")
        self.assertEqual(nvda_arc_xyz["round_trip_maker_taker_bps"], 16.02)
        self.assertEqual(nvda_arc_xyz["round_trip_dual_taker_bps"], 39.06)

        # 4) Hyperliquid SOL (1.44 / 4.32) + TradeXYZ xyz:SOL (0.288 / 0.864) -> BOTH legs verified_account_tier!
        sol_hl_xyz = next(
            p
            for p in res["pairs"]
            if p["canonical_base"] == "SOL" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"hyperliquid", "trade_xyz"}
        )
        self.assertEqual(sol_hl_xyz["fee_completeness"], "verified_account_tier_both_legs")
        # min MT = min(1.44 + 0.864, 0.288 + 4.32) = 2.304 -> RT = 4.608; DT = 4.32 + 0.864 = 5.184 -> RT = 10.368
        self.assertEqual(sol_hl_xyz["round_trip_maker_taker_bps"], 4.608)
        self.assertEqual(sol_hl_xyz["round_trip_dual_taker_bps"], 10.368)

        # 5) Unverified Lighter (public_schedule_only_unverified_account_tier) & Binance stay None / unknown
        for p in res["pairs"]:
            legs = (p["leg_a"], p["leg_b"])
            if any(leg["venue"] in ("lighter", "binance", "okx", "backpack", "extended", "bulk") for leg in legs):
                self.assertIsNone(p["round_trip_maker_taker_bps"], msg=p["pair_id"])
                self.assertIsNone(p["round_trip_dual_taker_bps"], msg=p["pair_id"])
                for leg in legs:
                    if leg["venue"] in ("lighter", "binance"):
                        self.assertIsNone(leg["maker_fee_bps"], msg=leg)
                        self.assertIsNone(leg["taker_fee_bps"], msg=leg)
                        self.assertEqual(leg["account_fee_status"], ACCOUNT_FEE_STATUS_UNKNOWN)

        # 6) When Lighter IS verified_account_tier, Lighter pairs compute round-trip fees cleanly
        lgt_verified_ev = copy.deepcopy(evidence)
        lgt_verified_ev["venues"]["lighter"] = {
            "venue": "lighter",
            "account_fee_status": ACCOUNT_FEE_STATUS_VERIFIED,
            "maker_fee_bps": "0.4",
            "taker_fee_bps": "2.8",
        }
        res_lgt = screen_pairs(snap, venues=["arcus", "lighter"], account_fee_evidence=lgt_verified_ev)
        ant_arc_lgt = next(p for p in res_lgt["pairs"] if p["canonical_base"] == "ANT")
        self.assertEqual(ant_arc_lgt["fee_completeness"], "mixed_verified_account_and_public_l0")
        # Lighter is maker_first due to 300ms speed bump -> 0.4 + 1.12 = 1.52 -> RT = 3.04; DT = 2.8 + 1.12 = 3.92 -> RT = 7.84
        self.assertEqual(ant_arc_lgt["round_trip_maker_taker_bps"], 3.04)
        self.assertEqual(ant_arc_lgt["round_trip_dual_taker_bps"], 7.84)

    def test_live_probe_carry_and_break_even_hours_with_account_fee_evidence(self) -> None:
        snap = build_embedded_self_test_snapshot()
        evidence = audit_account_fees(
            SUPPORTED_VENUES,
            live=False,
            fixtures=build_account_fee_fixtures(),
            audited_at_utc="2026-11-22T00:00:00+00:00",
        )

        def fake_fetcher(url: str, *, method: str = "GET", body: dict | None = None) -> bytes:
            if url == "https://api.arcus.xyz/v1/markets":
                return json.dumps(
                    {
                        "markets": [
                            {"marketDisplayName": "ANT-USD", "markPrice": "100.0", "fundingRate": "0.00015"},
                            {"marketDisplayName": "BTC-USD", "markPrice": "90000.0", "fundingRate": "0.00001"},
                        ]
                    }
                ).encode("utf-8")
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "metaAndAssetCtxs", "dex": "xyz"}:
                return json.dumps(
                    [
                        {"universe": [{"name": "xyz:ANT", "isDelisted": False}]},
                        [{"midPx": "100.1", "funding": "0.00002"}],
                    ]
                ).encode("utf-8")
            if url == "https://api.hyperliquid.xyz/info" and body == {"type": "metaAndAssetCtxs"}:
                return json.dumps(
                    [
                        {"universe": [{"name": "BTC"}]},
                        [{"midPx": "90010.0", "funding": "0.00006"}],
                    ]
                ).encode("utf-8")
            if url == "https://mainnet.zklighter.elliot.ai/api/v1/funding-rates":
                return json.dumps(
                    {
                        "code": 200,
                        "funding_rates": [
                            {"market_id": 150, "exchange": "lighter", "symbol": "ANT", "rate": 0.0008},
                        ],
                    }
                ).encode("utf-8")
            if url == "https://mainnet.zklighter.elliot.ai/api/v1/orderBookDetails":
                return json.dumps(
                    {
                        "code": 200,
                        "order_book_details": [
                            {"symbol": "ANT", "market_id": 150, "market_type": "perp", "status": "active", "last_trade_price": 100.0}
                        ],
                    }
                ).encode("utf-8")
            raise AssertionError(f"unexpected {url}")

        res = screen_pairs(
            snap,
            venues=["arcus", "hyperliquid", "trade_xyz", "lighter"],
            live_funding_probe=True,
            probe_fetcher=fake_fetcher,
            account_fee_evidence=evidence,
        )
        # Arcus ANT (0/1.12) + xyz:ANT (0.3/0.9): RT MT = 1.8, RT DT = 4.04; f_diff = |1.5 - 0.2| = 1.3 bps/hr
        ant_arc_xyz = next(
            p
            for p in res["pairs"]
            if p["canonical_base"] == "ANT" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "trade_xyz"}
        )
        lp = ant_arc_xyz["live_probe"]
        self.assertAlmostEqual(lp["net_7d_carry_maker_taker_bps"], 216.6, places=4)
        self.assertAlmostEqual(lp["net_7d_carry_dual_taker_bps"], 214.36, places=4)
        self.assertAlmostEqual(lp["break_even_hours_maker_taker"], 1.8 / 1.3, places=3)
        self.assertAlmostEqual(lp["break_even_hours_dual_taker"], 4.04 / 1.3, places=3)

        # Arcus BTC (1.5/4.5) + HL BTC (1.44/4.32): RT MT = 11.64, RT DT = 17.64; f_diff = |0.1 - 0.6| = 0.5 bps/hr
        btc_arc_hl = next(
            p
            for p in res["pairs"]
            if p["canonical_base"] == "BTC" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "hyperliquid"}
        )
        lp_hl = btc_arc_hl["live_probe"]
        self.assertAlmostEqual(lp_hl["net_7d_carry_maker_taker_bps"], 0.5 * 168.0 - 11.64, places=4)
        self.assertAlmostEqual(lp_hl["break_even_hours_maker_taker"], 11.64 / 0.5, places=3)

        # Unverified Lighter keeps break_even_hours and net_7d_carry = None
        ant_arc_lgt = next(
            p
            for p in res["pairs"]
            if p["canonical_base"] == "ANT" and {p["leg_a"]["venue"], p["leg_b"]["venue"]} == {"arcus", "lighter"}
        )
        self.assertIsNone(ant_arc_lgt["live_probe"]["net_7d_carry_maker_taker_bps"])
        self.assertIsNone(ant_arc_lgt["live_probe"]["break_even_hours_maker_taker"])

    def test_cli_account_fee_evidence_and_rejects_malformed_or_wrong_schema(self) -> None:
        snap = build_embedded_self_test_snapshot()
        evidence = audit_account_fees(
            SUPPORTED_VENUES,
            live=False,
            fixtures=build_account_fee_fixtures(),
            audited_at_utc="2026-11-22T00:00:00+00:00",
        )

        # Reject malformed / wrong schema in screen_pairs
        for bad_ev in (
            [],
            {},
            {"schema_version": "wrong_v0", "venues": {}},
            {"schema_version": EXPECTED_ACCOUNT_FEE_EVIDENCE_SCHEMA, "venues": {}},
            {
                "schema_version": EXPECTED_ACCOUNT_FEE_EVIDENCE_SCHEMA,
                "venues": {"hyperliquid": {"account_fee_status": ACCOUNT_FEE_STATUS_VERIFIED, "maker_fee_bps": None}},
            },
            {
                "schema_version": EXPECTED_ACCOUNT_FEE_EVIDENCE_SCHEMA,
                "venues": {
                    "trade_xyz": {
                        "account_fee_status": ACCOUNT_FEE_STATUS_VERIFIED,
                        "base_maker_fee_bps": "-0.1",
                        "base_taker_fee_bps": "4.5",
                    }
                },
            },
        ):
            with self.subTest(bad_ev=bad_ev):
                with self.assertRaises(ValueError):
                    screen_pairs(snap, account_fee_evidence=bad_ev)  # type: ignore[arg-type]

        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            snap_path = tmp_path / "snap.json"
            ev_path = tmp_path / "ev.json"
            bad_ev_path = tmp_path / "bad_ev.json"
            snap_path.write_text(json.dumps(snap), encoding="utf-8")
            ev_path.write_text(json.dumps(evidence), encoding="utf-8")
            bad_ev_path.write_text(json.dumps({"schema_version": "bad", "venues": {}}), encoding="utf-8")

            code_ok, out_ok, err_ok = _run_cli(
                ["--snapshot", str(snap_path), "--account-fee-evidence", str(ev_path)]
            )
            self.assertEqual(code_ok, 0, msg=err_ok)
            doc = json.loads(out_ok)
            self.assertEqual(doc["options"]["account_fee_evidence_schema"], EXPECTED_ACCOUNT_FEE_EVIDENCE_SCHEMA)

            code_bad, _, err_bad = _run_cli(
                ["--snapshot", str(snap_path), "--account-fee-evidence", str(bad_ev_path)]
            )
            self.assertEqual(code_bad, 2)
            self.assertIn("schema_version", err_bad)

            code_st_ev, _, err_st_ev = _run_cli(
                ["--self-test", "--account-fee-evidence", str(ev_path)]
            )
            self.assertEqual(code_st_ev, 2)
            self.assertIn("--account-fee-evidence", err_st_ev)


if __name__ == "__main__":
    unittest.main()
