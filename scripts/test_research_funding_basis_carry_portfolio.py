#!/usr/bin/env python3
"""Stdlib-only unit tests for scripts/research_funding_basis_carry_portfolio.py."""

from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys
import unittest

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import research_funding_basis_carry_portfolio as mod  # noqa: E402


class TestResearchFundingBasisCarryPortfolio(unittest.TestCase):
    def test_stdlib_only_top_level_imports_in_isolated_subprocess(self) -> None:
        blocker = "\n".join(
            [
                "import sys",
                "for m in ('numpy','duckdb','pyarrow','pandas','scipy'):",
                "    sys.modules[m] = None",
                "import research_funding_basis_carry_portfolio as m",
                "import json",
                "print(json.dumps(m.run_self_test()))",
            ]
        )
        env = dict(os.environ)
        env["PYTHONPATH"] = str(SCRIPT_DIR)
        proc = subprocess.run(
            [sys.executable, "-S", "-c", blocker],
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, msg=f"stderr={proc.stderr}\nstdout={proc.stdout}")
        self.assertIn('"self_test": "ok"', proc.stdout)
        self.assertIn('"has_cscv": true', proc.stdout)

    def test_sync_quote_with_tape_recenters_stale_snapshot(self) -> None:
        q = (99.98, 100.02, 10.0, 10.0, 1_000)
        trades = {"ts": [5_000], "sgn": [1], "px": [100.20], "sz": [5.0]}
        synced = mod._sync_quote_with_tape(q, trades, 5_500)
        self.assertIsNotNone(synced)
        assert synced is not None
        self.assertAlmostEqual(0.5 * (synced[0] + synced[1]), 100.20, places=6)
        self.assertEqual(synced[4], 5_000)
        # Fresh quote (<= 500ms) is untouched
        fresh = mod._sync_quote_with_tape(q, trades, 1_200)
        self.assertEqual(fresh, q)

    def test_market_map_and_cohort_segregation(self) -> None:
        catalog = mod.build_pair_catalog()
        syms = {p.symbol for p in catalog}
        # All 19 lighter symbols plus CRCL & DRAM overlapping pairs
        self.assertTrue({"BTC", "ETH", "SOL", "NVDA", "SPY", "QQQ", "AAPL", "TSLA", "MU", "SNDK"}.issubset(syms))
        self.assertEqual(mod.VENUE_SYMBOL_MAP["trade_xyz"]["SPY"], "xyz:SP500")
        self.assertEqual(mod.VENUE_SYMBOL_MAP["trade_xyz"]["QQQ"], "xyz:XYZ100")

        same_dom = [p for p in catalog if p.same_risk_domain]
        self.assertTrue(
            all(
                {p.venue_a, p.venue_b} in ({"lighter", "lighter_robinhood"}, {"trade_xyz", "entropy"})
                for p in same_dom
            )
        )
        usdg_distinct = [p for p in catalog if p.cohort == "USDG/USDC" and not p.same_risk_domain]
        self.assertTrue(all("lighter_robinhood" in (p.venue_a, p.venue_b) for p in usdg_distinct))

    def test_passive_print_verification_and_fx_drag_and_caps(self) -> None:
        pair_usdg = mod.CarryPairSpec(
            "NVDA", "rwa", "lighter_robinhood", "trade_xyz", "USDG/USDC", False, "rh<->trade_xyz NVDA", "mixed_ws_snapshot12s"
        )
        t0 = 1_700_000_000_000
        win = [{"window_num": 1, "window": "w01", "split": "selection_dev", "t0_ms": t0, "t1_ms": t0 + 4 * mod.HOUR_MS}]
        ts_bbo, ab, aa, absz, aasz, bb, ba, bbsz, basz = [], [], [], [], [], [], [], [], []
        t_cur = t0
        while t_cur <= win[0]["t1_ms"]:
            rich = 0.15 if ((t_cur - t0) // mod.HOUR_MS) >= 1 else 0.0
            ts_bbo.append(t_cur)
            ab.append(100.0 + rich - 0.03)
            aa.append(100.0 + rich + 0.03)
            absz.append(20.0)
            aasz.append(20.0)
            bb.append(99.99)
            ba.append(100.01)
            bbsz.append(20.0)
            basz.append(20.0)
            t_cur += 10_000

        bbo_a = {"ts": ts_bbo, "bid": ab, "ask": aa, "bsz": absz, "asz": aasz}
        bbo_b = {"ts": ts_bbo, "bid": bb, "ask": ba, "bsz": bbsz, "asz": basz}
        fa = {t0 + k * mod.HOUR_MS: 3.0 for k in range(1, 5)}
        fb = {t0 + k * mod.HOUR_MS: 0.0 for k in range(1, 5)}

        p_pas = mod.CarryArmParams(
            "pas", "C2_basis_timed_passive_first", min_basis_zscore=0.5, take_profit_basis_bps=4.0, passive_first=True
        )
        # Without public_trades prints -> zero passive fills!
        r_no_tr = mod.simulate_carry_pair_timeline(
            pair=pair_usdg, bbo_a=bbo_a, bbo_b=bbo_b, trades_a=None, trades_b=None,
            funding_a=fa, funding_b=fb, live_ctx_a=None, live_ctx_b=None, windows=win,
            fee_a_mk_bps=0.0, fee_a_tk_bps=0.0, fee_b_mk_bps=0.3, fee_b_tk_bps=0.9,
            max_age_a_ms=30_000, max_age_b_ms=30_000, params=p_pas, fx_drag_bps=3.0,
        )
        self.assertEqual(r_no_tr["windows"][0]["passive_fills"], 0)
        self.assertEqual(r_no_tr["windows"][0]["trades_entered"], 0)
        self.assertTrue(r_no_tr["terminal_flat"])

        # With strict trade-through prints on Leg A (> ask), passive fills and pays 3bp FX drag on USDG/USDC
        tr_ok = {
            "ts": [t0 + mod.HOUR_MS + 300 + k * 60_000 for k in range(10)],
            "sgn": [1] * 10,
            "px": [100.20] * 10,
            "sz": [20.0] * 10,
        }
        r_ok = mod.simulate_carry_pair_timeline(
            pair=pair_usdg, bbo_a=bbo_a, bbo_b=bbo_b, trades_a=tr_ok, trades_b=None,
            funding_a=fa, funding_b=fb, live_ctx_a=None, live_ctx_b=None, windows=win,
            fee_a_mk_bps=0.0, fee_a_tk_bps=0.0, fee_b_mk_bps=0.3, fee_b_tk_bps=0.9,
            max_age_a_ms=30_000, max_age_b_ms=30_000, params=p_pas, fx_drag_bps=3.0,
        )
        self.assertGreater(r_ok["windows"][0]["passive_fills"], 0)
        self.assertGreater(r_ok["windows"][0]["fx_drag_usd"], 0.0)
        self.assertTrue(r_ok["terminal_flat"])

    def test_waterfill_symbol_cap_and_zero_cross_subsidy(self) -> None:
        raw = {"lighter<->trade_xyz NVDA": 1.0, "rh<->trade_xyz NVDA": 1.0, "lighter<->trade_xyz AAPL": 0.5}
        sym = {
            "lighter<->trade_xyz NVDA": "NVDA",
            "rh<->trade_xyz NVDA": "NVDA",
            "lighter<->trade_xyz AAPL": "AAPL",
        }
        w = mod._waterfill_capped_weights(raw, sym, cap=0.40)
        nvda_tot = w["lighter<->trade_xyz NVDA"] + w["rh<->trade_xyz NVDA"]
        self.assertLessEqual(nvda_tot, 0.40 + 1e-9)
        self.assertLessEqual(sum(w.values()), 1.0 + 1e-9)


if __name__ == "__main__":
    unittest.main()
