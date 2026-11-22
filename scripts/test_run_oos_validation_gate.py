#!/usr/bin/env python3
"""Unit tests for `scripts/run_oos_validation_gate.py` (`Step 2`)."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path
import tempfile
import unittest

from scripts.run_oos_validation_gate import (
    evaluate_rth_ant_gate,
    evaluate_w15_rwa_gate,
    verify_w17_crash_pause_prereg,
)


def _sample_ant_window(*, net: float = 5.0, missing: int = 0, dd: float = 4.0) -> dict:
    variants = []
    for mode in ("TAKER_TAKER", "MAKER_ARCUS_TAKER_XYZ"):
        for lat in (100, 250, 500):
            variants.append(
                {
                    "mode": mode,
                    "lat_ms": lat,
                    "net_cash_usd": net,
                    "terminal_flat": True,
                    "max_drawdown_usd": dd,
                    "halted_58pct": False,
                    "cycles_closed": 5,
                    "win_rate": 0.80,
                }
            )
    return {
        "funding_hours_witnessed": 10,
        "funding_hours_missing_witness": missing,
        "variants": variants,
    }


def _sample_rwa_summary(*, nvda_cash: float, other_cash: float, other_dd: float = 12.0) -> dict:
    markets = {}
    for sym in ("NVDA", "SPY", "QQQ", "AMZN", "AAPL", "TSLA"):
        cash = nvda_cash if sym == "NVDA" else other_cash
        markets[sym] = {
            "arms": {
                "CORE_PURE_PULL05_C310": {
                    "core_maker_usd": 500_000.0,
                    "dual_usd": 500_000.0,
                    "terminal_liq_cash_usd": cash,
                    "terminal_flat_liq_ok": True,
                    "max_drawdown_usd": other_dd,
                    "halt_blocked": 50,
                },
                "CORE_HEDGE_SELF_PULL05_C310": {
                    "core_maker_usd": 500_000.0,
                    "dual_usd": 1_000_000.0,
                    "terminal_liq_cash_usd": cash - 5.0,
                    "terminal_flat_liq_ok": True,
                    "max_drawdown_usd": other_dd,
                    "halt_blocked": 1490,
                },
            }
        }
    total_core = 3_000_000.0
    total_pure = nvda_cash + 5 * other_cash
    total_hedge = (nvda_cash - 5.0) + 5 * (other_cash - 5.0)
    return {
        "aggregate_6markets": {
            "CORE_PURE_PULL05_C310": {
                "core_maker_usd": total_core,
                "dual_usd": total_core,
                "terminal_liq_cash_usd": total_pure,
                "single_venue_core_usd_per_m_liq": round(-total_pure / (total_core / 1e6), 2),
                "terminal_flat_all": True,
                "drawdown_58_breached": False,
            },
            "CORE_HEDGE_SELF_PULL05_C310": {
                "core_maker_usd": total_core,
                "dual_usd": 6_000_000.0,
                "terminal_liq_cash_usd": total_hedge,
                "single_venue_core_usd_per_m_liq": round(-total_hedge / (total_core / 1e6), 2),
                "terminal_flat_all": True,
                "drawdown_58_breached": False,
            },
        },
        "markets": markets,
    }


class OosValidationGateTests(unittest.TestCase):
    def test_rth_ant_gate_enforces_oos_role_and_failure_conditions(self) -> None:
        good = _sample_ant_window()
        self.assertEqual(evaluate_rth_ant_gate(good, role="monday_rth_oos")["verdict"], "GO")
        # Historical selection_dev can NEVER emit GO.
        dry = evaluate_rth_ant_gate(good, role="selection_dev_dryrun")
        self.assertEqual(dry["verdict"], "DRY_RUN_NOT_OOS")
        self.assertTrue(dry["passed_metrics"])

        # Missing funding witness or negative taker leg -> NO_GO.
        self.assertEqual(
            evaluate_rth_ant_gate(_sample_ant_window(missing=1), role="monday_rth_oos")["verdict"],
            "NO_GO",
        )
        self.assertEqual(
            evaluate_rth_ant_gate(_sample_ant_window(net=-0.5), role="monday_rth_oos")["verdict"],
            "NO_GO",
        )
        self.assertEqual(
            evaluate_rth_ant_gate(_sample_ant_window(dd=60.0), role="monday_rth_oos")["verdict"],
            "NO_GO",
        )

    def test_w15_rwa_gate_distinguishes_hedged_unhedged_and_no_go(self) -> None:
        # Pure ex-NVDA = +$2.5M volume, -$25 cash = $10/M (<=20); Hedged ex-NVDA = -$50 = $20/M (<=20)
        hedged_ok = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-5.0)
        res_hedged = evaluate_w15_rwa_gate(hedged_ok)
        self.assertEqual(res_hedged["verdict"], "GO_HEDGED")

        # Pure ex-NVDA = -$5/M (-$5 -> +$25), Hedged = -$30 -> $12/M? Wait: let pure=-7.0 ($14/M), hedge=-12.0 ($24/M)
        unhedged_only = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-7.0)
        res_unhedged = evaluate_w15_rwa_gate(unhedged_only)
        self.assertEqual(res_unhedged["verdict"], "GO_UNHEDGED_ONLY")

        bad = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-15.0)
        self.assertEqual(evaluate_w15_rwa_gate(bad)["verdict"], "NO_GO")

        dd_breach = _sample_rwa_summary(nvda_cash=-80.0, other_cash=-5.0, other_dd=600.0)
        self.assertEqual(evaluate_w15_rwa_gate(dd_breach)["verdict"], "NO_GO")

    def test_w17_crash_pause_verify_detects_tampering(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            target = root / "src" / "frozen.rs"
            target.parent.mkdir(parents=True)
            content = b"fn frozen() {}\n"
            target.write_bytes(content)
            prereg = root / "prereg.json"
            prereg.write_text(
                json.dumps(
                    {
                        "card_id": "TEST-CARD",
                        "method_sha256": "abc",
                        "builder_cli": "echo ok",
                        "artifact_sha256": {
                            "src/frozen.rs": hashlib.sha256(content).hexdigest()
                        },
                    }
                )
            )
            ok = verify_w17_crash_pause_prereg(prereg, root)
            self.assertEqual(ok["verdict"], "VERIFIED")

            target.write_bytes(b"fn tampered() {}\n")
            bad = verify_w17_crash_pause_prereg(prereg, root)
            self.assertEqual(bad["verdict"], "MISMATCH")


if __name__ == "__main__":
    unittest.main()
