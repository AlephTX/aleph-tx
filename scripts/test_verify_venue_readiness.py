#!/usr/bin/env python3
"""Offline unit tests for `scripts/verify_venue_readiness.py`."""

from __future__ import annotations

import json
from pathlib import Path
import tempfile
import unittest
from urllib.error import HTTPError

from scripts.verify_venue_readiness import evaluate_readiness, extract_public_monitor_config, main as cli_main


class VerifyVenueReadinessTests(unittest.TestCase):
    def test_extract_public_monitor_config_never_imports_private_keys(self) -> None:
        priv = "PRIVATE_" + "KEY"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            env_path = root / ".env.src"
            env_path.write_text(
                "\n".join([
                    "HYPERLIQUID_ACCOUNT_ADDRESS=0x1111111111111111111111111111111111111111",
                    "HYPERLIQUID_VAULT_ADDRESS=0x2222222222222222222222222222222222222222",
                    f"HYPERLIQUID_AGENT_{priv}=" + "ab" * 32,
                    "ARCUS_ACCOUNT_ADDRESS=0x3333333333333333333333333333333333333333",
                    "ARCUS_ACCOUNT_INDEX=1",
                    "LIGHTER_ACCOUNT_INDEX=77",
                ]) + "\n",
                encoding="utf-8",
            )
            pub = extract_public_monitor_config(source_env=env_path)
            self.assertEqual(pub["ALEPH_MONITOR_HL_ADDRESS"], "0x1111111111111111111111111111111111111111")
            self.assertEqual(pub["ALEPH_MONITOR_HL_XYZ_ADDRESS"], "0x2222222222222222222222222222222222222222")
            self.assertFalse(any("KEY" in k or "PRIVATE" in k for k in pub))

    def test_evaluate_readiness_parses_all_seven_venues_and_flags_errors_without_crashing(self) -> None:
        def fake_req(url: str, body: dict | None) -> object:
            if "zklighter.elliot.ai" in url:
                return {
                    "code": 200,
                    "accounts": [{
                        "index": 77,
                        "collateral": "100.5",
                        "available_balance": "100.5",
                        "positions": [{"position": "0", "sign": 1, "open_order_count": 0}],
                    }],
                }
            if "rh.lighter.xyz" in url:
                return {
                    "code": 200,
                    "accounts": [{
                        "index": 88,
                        "collateral": "50.0",
                        "available_balance": "50.0",
                        "positions": [],
                    }],
                }
            if "api.hyperliquid.xyz/info" in url:
                assert body is not None
                req_type = body["type"]
                if req_type == "clearinghouseState":
                    return {"marginSummary": {"accountValue": "250.0"}, "withdrawable": "240.0", "assetPositions": []}
                if req_type == "frontendOpenOrders":
                    return []
                if req_type == "userFees":
                    return {"userCrossRate": "0.00045", "userAddRate": "0.00015", "activeReferralDiscount": "0.04"}
            if "api.arcus.xyz/v1/positions" in url:
                return {"positions": []}
            if "api.arcus.xyz/v1/openOrders" in url:
                return {"orders": []}
            if "bulk.trade" in url:
                raise HTTPError(url, 404, "Not Found", {}, None)
            raise AssertionError(f"unexpected request {url} {body}")

        pub_cfg = {
            "ALEPH_MONITOR_LIGHTER_INDEX": "77",
            "ALEPH_MONITOR_LIGHTER_RH_INDEX": "88",
            "ALEPH_MONITOR_HL_ADDRESS": "0x1111111111111111111111111111111111111111",
            "ALEPH_MONITOR_HL_XYZ_ADDRESS": "0x2222222222222222222222222222222222222222",
            "ALEPH_MONITOR_ARCUS_ADDRESS": "0x3333333333333333333333333333333333333333",
            "ALEPH_MONITOR_ARCUS_INDEX": "0",
            "ALEPH_MONITOR_BULK_PUBKEY": "FuueqefENiGEW6uMqZQgmwjzgpnb85EgUcZa5Em4PQh7",
        }
        rep = evaluate_readiness(pub_cfg, requester=fake_req)
        self.assertEqual(rep["venues"]["Lighter"]["status"], "ok_flat")
        self.assertEqual(rep["venues"]["Hyperliquid"]["fees"]["user_cross_rate_bps"], 4.5)
        self.assertEqual(rep["venues"]["TradeXYZ"]["status"], "ok_flat")
        self.assertEqual(rep["venues"]["Arcus"]["status"], "ok_flat")
        self.assertEqual(rep["venues"]["BULK"]["status"], "error")
        self.assertTrue(rep["readiness"]["arcus_xyz_basis_ready_flat"])
        self.assertTrue(rep["readiness"]["lighter_mm_ready_flat"])
        self.assertEqual(rep["readiness"]["error_venues"], ["BULK"])

    def test_cli_refuses_network_without_live_flag(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            mon = Path(tmp) / "monitor.env"
            mon.write_text("ALEPH_MONITOR_LIGHTER_INDEX=77\n", encoding="utf-8")
            self.assertEqual(cli_main(["--monitor-env", str(mon)]), 2)


if __name__ == "__main__":
    unittest.main()
