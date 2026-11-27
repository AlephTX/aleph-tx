#!/usr/bin/env python3
"""Offline unit tests for `scripts/verify_account_fee_tiers_readonly.py` and `track_fee_evidence.py`."""

from __future__ import annotations

import base64
import hashlib
import hmac
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError
from urllib.parse import parse_qs, urlparse

REPO_ROOT = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

try:
    from scripts import audit_account_fees as aaf
    from scripts import track_fee_evidence as tfe
    from scripts import verify_account_fee_tiers_readonly as vaf
except ImportError:
    import audit_account_fees as aaf  # type: ignore[no-redef]
    import track_fee_evidence as tfe  # type: ignore[no-redef]
    import verify_account_fee_tiers_readonly as vaf  # type: ignore[no-redef]


def _run_cli(argv: list[str]) -> tuple[int, str, str]:
    stdout_buf = io.StringIO()
    stderr_buf = io.StringIO()
    with patch("sys.stdout", stdout_buf), patch("sys.stderr", stderr_buf):
        code = vaf.main(argv)
    return code, stdout_buf.getvalue(), stderr_buf.getvalue()


class VerifyAccountFeeTiersReadonlyTests(unittest.TestCase):
    def test_cli_offline_by_default_and_rejects_invalid_flags(self) -> None:
        with patch.object(vaf, "_live_http_json", side_effect=AssertionError("network forbidden")):
            code, out, err = _run_cli([])
            self.assertEqual(code, 2)
            self.assertEqual(out, "")
            self.assertIn("--live", err)

            code_live, _, err_live = _run_cli(["--live"])
            self.assertEqual(code_live, 2)
            self.assertIn("--env-file", err_live)

            with tempfile.TemporaryDirectory() as tmp:
                env_p = Path(tmp) / ".env"
                env_p.write_text("BINANCE_API_KEY=abc\n", encoding="utf-8")
                code_st, _, err_st = _run_cli(["--self-test", "--env-file", str(env_p)])
                self.assertEqual(code_st, 2)
                self.assertIn("only valid with --live", err_st)

    def test_self_test_and_stdlib_only_isolated_import(self) -> None:
        with patch.object(vaf, "_live_http_json", side_effect=AssertionError("network forbidden")):
            with patch.dict(os.environ, {"OKX_API_SECRET": "must_never_be_read"}, clear=False):
                code, out, err = _run_cli(["--self-test"])
            self.assertEqual(code, 0, msg=err)
            doc = json.loads(out)
            self.assertEqual(doc["self_test"], "passed")
            self.assertTrue(doc["redaction_verified"])
            self.assertEqual(len(doc["venues_tested"]), 10)
            self.assertNotIn("must_never_be_read", out)

        probe_code = "\n".join(
            [
                "import sys",
                "[sys.modules.__setitem__(m, None) for m in ('numpy','duckdb','pyarrow','pandas','scipy','requests','eth_account')]",
                f"sys.path.insert(0, {str(REPO_ROOT / 'scripts')!r})",
                "import verify_account_fee_tiers_readonly as mod",
                "res = mod.run_self_test()",
                "assert res['self_test'] == 'passed'",
                "print('STDLIB_ONLY: ok')",
            ]
        )
        proc = subprocess.run([sys.executable, "-S", "-c", probe_code], capture_output=True, text=True, check=False)
        self.assertEqual(proc.returncode, 0, msg=proc.stderr)
        self.assertIn("STDLIB_ONLY: ok", proc.stdout)

    def test_live_mode_passes_env_only_to_go_and_redacts_all_env_values(self) -> None:
        hl_addr = "0x1111111111111111111111111111111111111111"
        arc_addr = "0x2222222222222222222222222222222222222222"
        lgt_priv = "ab" * 40
        lgt_rh_priv = "cd" * 40
        bp_pub = "BpFakePubKey12345678901234567890123456789012="
        bp_sec = "BpFakeSecKey12345678901234567890123456789012="
        ext_key = "ext_secret_api_key_998877"
        okx_key = "okx_api_key_1122"
        okx_sec = "okx_api_secret_3344"
        okx_pass = "okx_pass_5566"
        bn_key = "binance_api_key_7788"
        bn_sec = "binance_api_secret_9900"
        lgt_idx = "333444"
        lgt_rh_idx = "555666"

        seen_child_envs: list[dict[str, str]] = []
        seen_go_venues: list[list[str]] = []
        seen_http_calls: list[tuple[str, dict[str, str] | None]] = []

        def fake_go_runner(venues: list[str], env_map: dict[str, str]) -> dict[str, Any]:
            seen_go_venues.append(venues)
            # Replicate _run_go_fee_audit child env filtering check:
            self.assertEqual(env_map["LIGHTER_PRIVATE_KEY"], lgt_priv)
            self.assertEqual(env_map["LIGHTER_RH_PRIVATE_KEY"], lgt_rh_priv)
            self.assertEqual(env_map["BACKPACK_SECRET_KEY"], bp_sec)
            seen_child_envs.append(dict(env_map))
            return vaf.build_self_test_fixtures()["go_fee_audit"]

        def fake_http(url: str, body: dict | None = None, headers: dict[str, str] | None = None) -> Any:
            seen_http_calls.append((url, headers))
            if url == aaf.HL_INFO_URL and body == {"type": "userFees", "user": hl_addr}:
                return {"userCrossRate": "0.00045", "userAddRate": "0.00015", "dailyUserVlm": [{"userCross": "99999"}]}
            if url == aaf.HL_INFO_URL and body == {"type": "perpDexs"}:
                return [None, {"name": "xyz", "deployerFeeScale": "1.0"}]
            if url == aaf.HL_INFO_URL and body == {"type": "metaAndAssetCtxs", "dex": "xyz"}:
                return [{"universe": [{"name": "xyz:NVDA", "growthMode": "enabled"}]}, [{}]]
            if url.startswith(vaf.ARCUS_ACCOUNT_URL):
                return {
                    "accountIndex": 1,
                    "address": arc_addr,
                    "netQuoteBalance": "4321.00",
                    "equity": "4321.00",
                    "freeCollateral": "3210.00",
                    "sequenceNumber": 77,
                }
            if url.startswith(vaf.ARCUS_ACCOUNT_STATS_URL):
                return {"address": arc_addr, "tradingFeeTier": {"level": 0, "feeDiscountPpm": 50000}}
            if url == vaf.ARCUS_FEE_SCHEDULES_URL:
                return vaf.build_self_test_fixtures()["arcus"]["feeSchedules"]
            if url == f"{vaf.EXTENDED_BASE_URL}/api/v1/user/fees":
                assert headers is not None and headers.get("X-Api-Key") == ext_key
                return {"status": "OK", "data": [{"market": "BTC-USD", "makerFeeRate": "0", "takerFeeRate": "0.00025"}]}
            if url == vaf.OKX_TRADE_FEE_URL:
                assert headers is not None
                self.assertEqual(headers["OK-ACCESS-KEY"], okx_key)
                self.assertEqual(headers["OK-ACCESS-PASSPHRASE"], okx_pass)
                prehash = f"{headers['OK-ACCESS-TIMESTAMP']}GET/api/v5/account/trade-fee?instType=SWAP"
                expected_sig = base64.b64encode(
                    hmac.new(okx_sec.encode("utf-8"), prehash.encode("utf-8"), hashlib.sha256).digest()
                ).decode("ascii")
                self.assertEqual(headers["OK-ACCESS-SIGN"], expected_sig)
                return {"code": "0", "data": [{"level": "Lv1", "category": "1", "makerU": "-0.0002", "takerU": "-0.0005"}]}
            if url.startswith(vaf.BINANCE_COMMISSION_URL):
                assert headers is not None and headers.get("X-MBX-APIKEY") == bn_key
                parsed_q = parse_qs(urlparse(url).query)
                sig = parsed_q["signature"][0]
                raw_q = urlparse(url).query.rsplit("&signature=", 1)[0]
                expected_sig = hmac.new(bn_sec.encode("utf-8"), raw_q.encode("utf-8"), hashlib.sha256).hexdigest()
                self.assertEqual(sig, expected_sig)
                return {"symbol": "BTCUSDT", "makerCommissionRate": "0.0002", "takerCommissionRate": "0.0005"}
            if url == "https://mainnet-api1.bulk.trade/api/v1/exchangeInfo":
                return [{"symbol": "BTC-USD", "status": "ACTIVE"}]
            raise AssertionError(f"unexpected HTTP call: {url} {body}")

        with tempfile.TemporaryDirectory() as tmp:
            env_path = Path(tmp) / ".env.all"
            out_path = Path(tmp) / "out.json"
            env_lines = [
                f"LIGHTER_BASE_URL=https://mainnet.zklighter.elliot.ai",
                f"LIGHTER_CHAIN_ID=304",
                f"LIGHTER_ACCOUNT_INDEX={lgt_idx}",
                f"LIGHTER_API_KEY_INDEX=2",
                f"LIGHTER_PRIVATE_KEY={lgt_priv}",
                f"LIGHTER_RH_BASE_URL=https://api.rh.lighter.xyz",
                f"LIGHTER_RH_CHAIN_ID=466324",
                f"LIGHTER_RH_ACCOUNT_INDEX={lgt_rh_idx}",
                f"LIGHTER_RH_API_KEY_INDEX=3",
                f"LIGHTER_RH_PRIVATE_KEY={lgt_rh_priv}",
                f"HYPERLIQUID_ACCOUNT_ADDRESS={hl_addr}",
                f"ARCUS_ACCOUNT_ADDRESS={arc_addr}",
                f"ARCUS_ACCOUNT_INDEX=1",
                f"EXTENDED_API_KEY={ext_key}",
                f"BACKPACK_PUBLIC_KEY={bp_pub}",
                f"BACKPACK_SECRET_KEY={bp_sec}",
                f"OKX_API_KEY={okx_key}",
                f"OKX_API_SECRET={okx_sec}",
                f"OKX_PASSPHRASE={okx_pass}",
                f"BINANCE_API_KEY={bn_key}",
                f"BINANCE_API_SECRET={bn_sec}",
                f"BULK_API_URL=https://mainnet-api1.bulk.trade/api/v1",
            ]
            env_path.write_text("\n".join(env_lines) + "\n", encoding="utf-8")

            with patch.object(vaf, "_live_http_json", side_effect=fake_http), patch.object(
                vaf, "_run_go_fee_audit", side_effect=fake_go_runner
            ):
                code, out_str, err_str = _run_cli(["--live", "--env-file", str(env_path), "--out", str(out_path)])

            self.assertEqual(code, 0, msg=err_str)
            report = json.loads(out_str)
            self.assertEqual(report, json.loads(out_path.read_text(encoding="utf-8")))
            self.assertTrue(report["summary"]["redaction_verified"])
            self.assertEqual(report["summary"]["error_venues"], 0)
            self.assertEqual(seen_go_venues, [["lighter", "lighter_robinhood", "backpack"]])

            # Check EVERY non-empty env value has 0 leaks in output JSON.
            env_map = vaf.parse_env_file_in_memory(env_path)
            for val in vaf.collect_all_env_literals(env_map):
                if len(val) >= 2:
                    self.assertNotIn(val.lower(), out_str.lower(), msg=f"leaked env literal {val!r}")
            for extra in ("4321.00", "3210.00", "99999"):
                self.assertNotIn(extra, out_str)

    def test_arcus_verified_tier_resolves_domain_and_custom_market_overrides(self) -> None:
        fix = {
            "arcus": {
                "feeSchedules": vaf.build_self_test_fixtures()["arcus"]["feeSchedules"],
                "account": {
                    "accountIndex": 1,
                    "address": "0x2222222222222222222222222222222222222222",
                    "tradingFeeTier": {"level": 4},
                },
            }
        }
        # BTC-USD already has level 0 & 4 in embedded fixture; add level 4 to ANT-USD & NVDA-USD.
        fix["arcus"]["feeSchedules"]["feeSchedules"][1]["tiers"].append({"level": 4, "makerFeePpm": 0, "takerFeePpm": 90})
        fix["arcus"]["feeSchedules"]["feeSchedules"].append(
            {
                "marketId": 89,
                "marketDisplayName": "NVDA-USD",
                "hasCustomSchedule": True,
                "tiers": [
                    {"level": 0, "makerFeePpm": 0, "takerFeePpm": 225},
                    {"level": 4, "makerFeePpm": 0, "takerFeePpm": 115},
                ],
            }
        )
        rep = vaf.run_fee_tier_audit(["arcus"], live=False, fixtures=fix)
        arc = rep["venues"]["arcus"]
        self.assertEqual(arc["account_fee_status"], aaf.ACCOUNT_FEE_STATUS_VERIFIED)
        self.assertEqual(arc["verified_level"], 4)
        self.assertEqual(arc["crypto_maker_fee_bps"], "0")
        self.assertEqual(arc["crypto_taker_fee_bps"], "2.3")
        self.assertEqual(arc["rwa_maker_fee_bps"], "0")
        self.assertEqual(arc["rwa_taker_fee_bps"], "1.15")
        self.assertEqual(arc["verified_custom_schedule_bps"]["ANT-USD"], {"maker_fee_bps": "0", "taker_fee_bps": "0.9"})

        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "ev.json"
            p.write_text(json.dumps(rep), encoding="utf-8")
            ev = tfe.load_track_fee_evidence(p)
            self.assertEqual(
                tfe.resolve_venue_fee("arcus", "crypto", default_maker_bps=1.5, default_taker_bps=4.5, default_status="public_tier0_schedule_only", evidence=ev, raw_market="BTC-USD"),
                (0.0, 2.3, "verified_account_tier", "evidence_arcus_verified_crypto"),
            )
            self.assertEqual(
                tfe.resolve_venue_fee("arcus", "rwa", default_maker_bps=0.0, default_taker_bps=2.25, default_status="public_tier0_schedule_only", evidence=ev, raw_market="SPY-USD"),
                (0.0, 1.15, "verified_account_tier", "evidence_arcus_verified_rwa"),
            )
            self.assertEqual(
                tfe.resolve_venue_fee("arcus", "rwa", default_maker_bps=0.0, default_taker_bps=2.25, default_status="public_tier0_schedule_only", evidence=ev, raw_market="ANT-USD"),
                (0.0, 0.9, "verified_account_tier", "evidence_arcus_verified_market"),
            )

        # Verify Arcus stats endpoint with feeDiscountPpm=50000 (5% off positive fees)
        fix_disc = {
            "arcus": {
                "feeSchedules": fix["arcus"]["feeSchedules"],
                "stats": {
                    "address": "0x2222222222222222222222222222222222222222",
                    "tradingFeeTier": {"level": 0, "feeDiscountPpm": 50000},
                },
            }
        }
        rep_disc = vaf.run_fee_tier_audit(["arcus"], live=False, fixtures=fix_disc)
        arc_disc = rep_disc["venues"]["arcus"]
        self.assertEqual(arc_disc["account_fee_status"], aaf.ACCOUNT_FEE_STATUS_VERIFIED)
        self.assertEqual(arc_disc["fee_discount_ppm"], 50000)
        self.assertEqual(arc_disc["crypto_maker_fee_bps"], "1.425")
        self.assertEqual(arc_disc["crypto_taker_fee_bps"], "4.275")
        self.assertEqual(arc_disc["rwa_maker_fee_bps"], "0")
        self.assertEqual(arc_disc["rwa_taker_fee_bps"], "2.1375")
        self.assertEqual(arc_disc["verified_custom_schedule_bps"]["ANT-USD"], {"maker_fee_bps": "0", "taker_fee_bps": "1.064"})

        # Verify Extended domain/per-market fee resolution via track_fee_evidence
        ext_rep = vaf.run_fee_tier_audit(
            ["extended"],
            live=False,
            fixtures={
                "extended": {
                    "status": "OK",
                    "data": [
                        {"market": "BTC-USD", "makerFeeRate": "0", "takerFeeRate": "0.000225", "builderFeeRate": "0"},
                        {"market": "AAPL_24_5-USD", "makerFeeRate": "0", "takerFeeRate": "0.00009", "builderFeeRate": "0"},
                    ],
                }
            },
        )
        with tempfile.TemporaryDirectory() as tmp2:
            ep = Path(tmp2) / "ext_ev.json"
            ep.write_text(json.dumps(ext_rep), encoding="utf-8")
            ext_ev = tfe.load_track_fee_evidence(ep)
            self.assertEqual(
                tfe.resolve_venue_fee("extended", "crypto", default_maker_bps=0.0, default_taker_bps=2.5, default_status="unknown", evidence=ext_ev, raw_market="BTC-USD"),
                (0.0, 2.25, "verified_account_tier", "evidence_extended_verified_market"),
            )
            self.assertEqual(
                tfe.resolve_venue_fee("extended", "crypto", default_maker_bps=0.0, default_taker_bps=2.5, default_status="unknown", evidence=ext_ev),
                (0.0, 2.25, "verified_account_tier", "evidence_extended_verified_crypto"),
            )
            self.assertEqual(
                tfe.resolve_venue_fee("extended", "rwa", default_maker_bps=0.0, default_taker_bps=2.5, default_status="unknown", evidence=ext_ev),
                (0.0, 0.9, "verified_account_tier", "evidence_extended_verified_rwa"),
            )

    def test_lighter_robinhood_independent_resolution_and_extended_fallback(self) -> None:
        ev_doc = {
            "schema_version": aaf.SCHEMA_VERSION,
            "venues": {
                "lighter": {
                    "settlement": "USDC",
                    "account_fee_status": "verified_account_tier",
                    "maker_fee_bps": "0",
                    "taker_fee_bps": "0",
                },
                "lighter_robinhood": {
                    "settlement": "USDG",
                    "account_fee_status": "verified_account_tier",
                    "maker_fee_bps": "1.5",
                    "taker_fee_bps": "1.5",
                },
            },
        }
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp) / "ev.json"
            p.write_text(json.dumps(ev_doc), encoding="utf-8")
            ev = tfe.load_track_fee_evidence(p)
            self.assertEqual(
                tfe.resolve_venue_fee("lighter", "crypto", default_maker_bps=0.0, default_taker_bps=0.0, default_status="public_tier0_unverified", evidence=ev),
                (0.0, 0.0, "verified_account_tier", "evidence_account_tier"),
            )
            self.assertEqual(
                tfe.resolve_venue_fee("lighter_robinhood", "crypto", default_maker_bps=0.0, default_taker_bps=0.0, default_status="public_tier0_unverified", evidence=ev),
                (1.5, 1.5, "verified_account_tier", "evidence_account_tier"),
            )

        # Unverified arcus domain override must fail closed.
        bad_arc = {
            "schema_version": aaf.SCHEMA_VERSION,
            "venues": {
                "arcus": {
                    "account_fee_status": "public_tier0_schedule_only",
                    "crypto_maker_fee_bps": "0",
                    "crypto_taker_fee_bps": "2.3",
                }
            },
        }
        with tempfile.TemporaryDirectory() as tmp:
            bp = Path(tmp) / "bad.json"
            bp.write_text(json.dumps(bad_arc), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "verified_account_tier"):
                tfe.load_track_fee_evidence(bp)

        # Extended 400 fallback to ?market=BTC-USD
        def fake_ext(url: str, body: dict | None, headers: dict[str, str] | None) -> Any:
            if url.endswith("/api/v1/user/fees"):
                raise HTTPError(url, 400, "Bad Request", {}, None)
            if url.endswith("/api/v1/user/fees?market=BTC-USD"):
                return {"status": "OK", "data": {"makerFeeRate": "0", "takerFeeRate": "0.00025"}}
            raise AssertionError(f"unexpected {url}")

        res = vaf._probe_extended({"EXTENDED_API_KEY": "secret_ext"}, fake_ext)
        self.assertEqual(res["account_fee_status"], aaf.ACCOUNT_FEE_STATUS_VERIFIED)
        self.assertEqual(res["maker_fee_bps"], "0")
        self.assertEqual(res["taker_fee_bps"], "2.5")

    def test_run_go_fee_audit_passes_only_allowed_env_keys(self) -> None:
        captured: dict[str, Any] = {}

        class FakeProc:
            returncode = 0
            stdout = "shm: ShmMarketState size = 1589248 bytes (1552.00 KB)\n" + json.dumps(
                {"schema_version": "aleph_go_fee_audit_v1", "venues": {}}
            )
            stderr = ""

        def fake_run(cmd, cwd, env, capture_output, text, timeout, check):
            captured["cmd"] = cmd
            captured["cwd"] = cwd
            captured["env"] = env
            return FakeProc()

        env_map = {
            "LIGHTER_PRIVATE_KEY": "secret_lgt",
            "LIGHTER_RH_PRIVATE_KEY": "secret_rh",
            "BACKPACK_SECRET_KEY": "secret_bp",
            "OKX_API_SECRET": "must_not_pass_to_go",
            "BINANCE_API_SECRET": "must_not_pass_to_go",
        }
        with patch("subprocess.run", side_effect=fake_run):
            vaf._run_go_fee_audit(["lighter", "backpack"], env_map)

        self.assertEqual(captured["env"]["LIGHTER_PRIVATE_KEY"], "secret_lgt")
        self.assertEqual(captured["env"]["LIGHTER_RH_PRIVATE_KEY"], "secret_rh")
        self.assertEqual(captured["env"]["BACKPACK_SECRET_KEY"], "secret_bp")
        self.assertNotIn("OKX_API_SECRET", captured["env"])
        self.assertNotIn("BINANCE_API_SECRET", captured["env"])
        # Secrets must never appear in CLI args
        self.assertNotIn("secret_lgt", " ".join(captured["cmd"]))


if __name__ == "__main__":
    unittest.main()
