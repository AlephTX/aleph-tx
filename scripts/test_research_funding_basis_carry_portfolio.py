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

    def test_p0_defect1_hedge_depth_clamping_and_partial_orphan_unwind(self) -> None:
        pair = mod.CarryPairSpec(
            "NVDA", "rwa", "lighter", "trade_xyz", "USDC/USDC", False, "lighter<->trade_xyz NVDA", "mixed_ws_snapshot12s"
        )
        t0 = 1_700_000_000_000
        win = [{"window_num": 1, "window": "w01", "split": "selection_dev", "t0_ms": t0, "t1_ms": t0 + 4 * mod.HOUR_MS}]
        ts_bbo, ab, aa, absz, aasz, bb, ba, bbsz, basz = [], [], [], [], [], [], [], [], []
        t_cur = t0
        while t_cur <= win[0]["t1_ms"]:
            rich = 0.20 if (t_cur - t0) >= mod.HOUR_MS else 0.0
            ts_bbo.append(t_cur)
            ab.append(100.0 + rich - 0.03)
            aa.append(100.0 + rich + 0.03)
            absz.append(20.0)
            aasz.append(20.0)
            bb.append(99.99)
            ba.append(100.01)
            bbsz.append(3.5)
            basz.append(3.5)  # $350.035 L1 depth on B ask (< $1000 passive fill)
            t_cur += 10_000

        bbo_a = {"ts": ts_bbo, "bid": ab, "ask": aa, "bsz": absz, "asz": aasz}
        bbo_b = {"ts": ts_bbo, "bid": bb, "ask": ba, "bsz": bbsz, "asz": basz}
        fa = {t0 + k * mod.HOUR_MS: 3.0 for k in range(1, 5)}
        fb = {t0 + k * mod.HOUR_MS: 0.0 for k in range(1, 5)}
        tr_a = {"ts": [t0 + mod.HOUR_MS + 300], "sgn": [1], "px": [100.25], "sz": [20.0]}

        p_pas = mod.CarryArmParams(
            "pas_clamp",
            "C2_basis_timed_passive_first",
            min_basis_zscore=0.5,
            take_profit_basis_bps=100.0,
            max_hold_h=48.0,
            passive_first=True,
        )
        res = mod.simulate_carry_pair_timeline(
            pair=pair,
            bbo_a=bbo_a,
            bbo_b=bbo_b,
            trades_a=tr_a,
            trades_b=None,
            funding_a=fa,
            funding_b=fb,
            live_ctx_a=None,
            live_ctx_b=None,
            windows=win,
            fee_a_mk_bps=0.4,
            fee_a_tk_bps=2.8,
            fee_b_mk_bps=0.3,
            fee_b_tk_bps=0.9,
            max_age_a_ms=30_000,
            max_age_b_ms=30_000,
            params=p_pas,
            record_trades=True,
        )
        w0 = res["windows"][0]
        self.assertEqual(w0["passive_fills"], 1)
        self.assertEqual(w0["trades_entered"], 1)
        self.assertEqual(w0["depth_clipped_entries"], 1)
        self.assertEqual(w0["orphan_entries"], 1)
        self.assertEqual(w0["orphan_unwound_count"], 1)
        self.assertAlmostEqual(w0["orphan_notional_usd"], 1000.0 - 350.035, places=3)
        self.assertGreater(w0["orphan_unwind_cost_usd"], 0.0)
        matched = [e for e in res["trade_events"] if e["kind"] == "matched_pair"]
        self.assertEqual(len(matched), 1)
        self.assertAlmostEqual(matched[0]["hedged_notional_usd"], 350.035, places=3)
        reconciled = (
            w0["funding_cash_usd"]
            + w0["basis_pnl_usd"]
            - w0["spread_slip_usd"]
            - w0["fees_usd"]
            - w0["fx_drag_usd"]
        )
        self.assertAlmostEqual(w0["net_pnl_usd"], reconciled, places=9)
        self.assertTrue(res["terminal_flat"])

    def test_p0_defect2_orphan_leg_booked_and_unwound_on_hedge_reject_and_missing_book(self) -> None:
        pair = mod.CarryPairSpec(
            "NVDA", "rwa", "lighter", "trade_xyz", "USDC/USDC", False, "lighter<->trade_xyz NVDA", "mixed_ws_snapshot12s"
        )
        t0 = 1_700_000_000_000
        win = [{"window_num": 1, "window": "w01", "split": "selection_dev", "t0_ms": t0, "t1_ms": t0 + 4 * mod.HOUR_MS}]
        ts_bbo, ab, aa, absz, aasz, bb, ba, bbsz, basz = [], [], [], [], [], [], [], [], []
        t_fill_window = t0 + mod.HOUR_MS
        t_cur = t0
        while t_cur <= win[0]["t1_ms"]:
            rich = 0.20 if t_cur >= t_fill_window else 0.0
            a_adv = 0.10 if t_cur >= t_fill_window + 400 else 0.0
            ts_bbo.append(t_cur)
            ab.append(100.0 + rich + a_adv - 0.03)
            aa.append(100.0 + rich + a_adv + 0.03)
            absz.append(20.0)
            aasz.append(4.0 if t_cur >= t_fill_window + 400 else 20.0)
            # Subcase A: hedge venue B jumps +50 bps right after t_post (100.0 -> 100.50), triggering adverse abort
            b_jump = 0.50 if t_cur >= t_fill_window + 250 else 0.0
            bb.append(99.99 + b_jump)
            ba.append(100.01 + b_jump)
            bbsz.append(20.0)
            basz.append(20.0)
            t_cur += 100

        bbo_a = {"ts": ts_bbo, "bid": ab, "ask": aa, "bsz": absz, "asz": aasz}
        bbo_b_adv = {"ts": ts_bbo, "bid": bb, "ask": ba, "bsz": bbsz, "asz": basz}
        fa = {t0 + k * mod.HOUR_MS: 3.0 for k in range(1, 5)}
        fb = {t0 + k * mod.HOUR_MS: 0.0 for k in range(1, 5)}
        tr_a = {"ts": [t_fill_window + 300], "sgn": [1], "px": [100.25], "sz": [20.0]}

        p_pas = mod.CarryArmParams(
            "pas_orphan",
            "C2_basis_timed_passive_first",
            min_basis_zscore=0.5,
            take_profit_basis_bps=5.0,
            passive_first=True,
            hazard_adverse_bps=3.0,
        )
        res_adv = mod.simulate_carry_pair_timeline(
            pair=pair,
            bbo_a=bbo_a,
            bbo_b=bbo_b_adv,
            trades_a=tr_a,
            trades_b=None,
            funding_a=fa,
            funding_b=fb,
            live_ctx_a=None,
            live_ctx_b=None,
            windows=win,
            fee_a_mk_bps=0.4,
            fee_a_tk_bps=2.8,
            fee_b_mk_bps=0.3,
            fee_b_tk_bps=0.9,
            max_age_a_ms=5_000,
            max_age_b_ms=5_000,
            params=p_pas,
            step_ms=10_000,
            record_trades=True,
        )
        w_adv = res_adv["windows"][0]
        self.assertEqual(w_adv["trades_entered"], 0)
        self.assertEqual(w_adv["passive_fills"], 1)
        self.assertEqual(w_adv["orphan_entries"], 1)
        self.assertEqual(w_adv["orphan_unwound_count"], 1)
        self.assertAlmostEqual(w_adv["orphan_notional_usd"], 1000.0, places=3)
        self.assertLess(w_adv["orphan_unwind_pnl_usd"], 0.0)
        self.assertGreater(w_adv["fees_usd"], 0.0)
        self.assertGreater(w_adv["spread_slip_usd"], 0.0)
        self.assertAlmostEqual(w_adv["net_pnl_usd"], w_adv["orphan_unwind_pnl_usd"], places=6)
        self.assertTrue(res_adv["terminal_flat"])

        # Subcase B: both hedge venue B and passive venue A go dark right after t_post -> residual exposure retained!
        cut = t_fill_window + 200
        keep_idx = [i for i, t in enumerate(ts_bbo) if t <= cut]
        bbo_a_dark = {k: [bbo_a[k][i] for i in keep_idx] for k in bbo_a}
        bbo_b_dark = {k: [bbo_b_adv[k][i] for i in keep_idx] for k in bbo_b_adv}
        tr_a_dark = {"ts": [t_fill_window + 1_000], "sgn": [1], "px": [100.25], "sz": [20.0]}
        res_dark = mod.simulate_carry_pair_timeline(
            pair=pair,
            bbo_a=bbo_a_dark,
            bbo_b=bbo_b_dark,
            trades_a=tr_a_dark,
            trades_b=None,
            funding_a=fa,
            funding_b=fb,
            live_ctx_a=None,
            live_ctx_b=None,
            windows=win,
            fee_a_mk_bps=0.4,
            fee_a_tk_bps=2.8,
            fee_b_mk_bps=0.3,
            fee_b_tk_bps=0.9,
            max_age_a_ms=500,
            max_age_b_ms=500,
            params=p_pas,
            step_ms=10_000,
        )
        w_dark = res_dark["windows"][0]
        self.assertEqual(w_dark["orphan_entries"], 1)
        self.assertEqual(w_dark["orphan_unwound_count"], 0)
        self.assertFalse(res_dark["terminal_flat"])
        self.assertAlmostEqual(res_dark["terminal_residual_notional_usd"], 1000.0, places=3)
        self.assertAlmostEqual(w_dark["orphan_open_residual_usd"], 1000.0, places=3)

    def test_p0_defect3_per_leg_fill_timestamps_and_strict_settlement_skew(self) -> None:
        pair = mod.CarryPairSpec(
            "NVDA", "rwa", "lighter", "trade_xyz", "USDC/USDC", False, "lighter<->trade_xyz NVDA", "mixed_ws_snapshot12s"
        )
        # Align t0 to exact HOUR_MS multiple so settlement hours fall cleanly on HOUR_MS boundaries.
        t0 = 1_700_000_400_000  # 1_700_000_400_000 % 3_600_000 == 0
        t_settle_1 = t0 + 2 * mod.HOUR_MS
        t_settle_2 = t0 + 3 * mod.HOUR_MS
        win = [{"window_num": 1, "window": "w01", "split": "selection_dev", "t0_ms": t0, "t1_ms": t0 + 4 * mod.HOUR_MS}]

        # Passive Leg A fills at t_settle_1 - 100ms; Hedge Leg B fills at t_settle_1 + 230ms (330ms later).
        # Also include a boundary test with step_ms=1_000 where entry decision occurs at t_settle_1 - 1_000.
        ts_bbo, ab, aa, absz, aasz, bb, ba, bbsz, basz = [], [], [], [], [], [], [], [], []
        t_trigger = t_settle_1 - 1_000
        t_cur = t0
        while t_cur <= win[0]["t1_ms"]:
            rich = 0.25 if t_cur >= t_trigger else 0.0
            ts_bbo.append(t_cur)
            ab.append(100.0 + rich - 0.03)
            aa.append(100.0 + rich + 0.03)
            absz.append(20.0)
            aasz.append(20.0)
            bb.append(99.99)
            ba.append(100.01)
            bbsz.append(20.0)
            basz.append(20.0)
            t_cur += 1_000

        bbo_a = {"ts": ts_bbo, "bid": ab, "ask": aa, "bsz": absz, "asz": aasz}
        bbo_b = {"ts": ts_bbo, "bid": bb, "ask": ba, "bsz": bbsz, "asz": basz}
        fa = {
            t0 + mod.HOUR_MS: 5.0,
            t_settle_1: 10.0,
            t_settle_2: 10.0,
        }
        fb = {
            t0 + mod.HOUR_MS: 0.0,
            t_settle_1: 4.0,
            t_settle_2: 4.0,
        }
        # Trade print occurs at t_settle_1 - 100ms (> t_trigger + 200ms = t_settle_1 - 800ms)
        fill_a_ms = t_settle_1 - 100
        tr_a = {"ts": [fill_a_ms], "sgn": [1], "px": [100.30], "sz": [20.0]}

        p_pas = mod.CarryArmParams(
            "pas_skew",
            "C2_basis_timed_passive_first",
            min_basis_zscore=0.5,
            take_profit_basis_bps=200.0,
            max_hold_h=48.0,
            passive_first=True,
        )
        res = mod.simulate_carry_pair_timeline(
            pair=pair,
            bbo_a=bbo_a,
            bbo_b=bbo_b,
            trades_a=tr_a,
            trades_b=None,
            funding_a=fa,
            funding_b=fb,
            live_ctx_a=None,
            live_ctx_b=None,
            windows=win,
            fee_a_mk_bps=0.4,
            fee_a_tk_bps=2.8,
            fee_b_mk_bps=0.3,
            fee_b_tk_bps=0.9,
            max_age_a_ms=5_000,
            max_age_b_ms=5_000,
            params=p_pas,
            step_ms=1_000,
            record_trades=True,
        )
        w0 = res["windows"][0]
        matched = [e for e in res["trade_events"] if e["kind"] == "matched_pair"]
        self.assertEqual(len(matched), 1)
        tr0 = matched[0]
        self.assertEqual(tr0["leg_a_fill_ms"], fill_a_ms)
        self.assertEqual(tr0["leg_b_fill_ms"], fill_a_ms + mod.TAKER_LATENCY_MS)
        self.assertLess(tr0["leg_a_fill_ms"], t_settle_1)
        self.assertGreater(tr0["leg_b_fill_ms"], t_settle_1)
        # Leg A accrued at both t_settle_1 and t_settle_2 (2 payments); Leg B only at t_settle_2 (1 payment)!
        self.assertEqual(w0["leg_a_funding_payments"], 2)
        self.assertEqual(w0["leg_b_funding_payments"], 1)
        self.assertAlmostEqual(w0["leg_a_funding_cash_usd"], 2.0, delta=0.02)
        self.assertAlmostEqual(w0["leg_b_funding_cash_usd"], -0.40, delta=0.02)
        self.assertAlmostEqual(w0["funding_cash_usd"], w0["leg_a_funding_cash_usd"] + w0["leg_b_funding_cash_usd"], places=9)

        # Boundary test: if settlement_ms == leg_a_fill_ms exactly, strict `>` must NOT accrue funding!
        tr_exact = {"ts": [t_settle_1], "sgn": [1], "px": [100.30], "sz": [20.0]}
        res_eq = mod.simulate_carry_pair_timeline(
            pair=pair,
            bbo_a=bbo_a,
            bbo_b=bbo_b,
            trades_a=tr_exact,
            trades_b=None,
            funding_a=fa,
            funding_b=fb,
            live_ctx_a=None,
            live_ctx_b=None,
            windows=win,
            fee_a_mk_bps=0.4,
            fee_a_tk_bps=2.8,
            fee_b_mk_bps=0.3,
            fee_b_tk_bps=0.9,
            max_age_a_ms=5_000,
            max_age_b_ms=5_000,
            params=p_pas,
            step_ms=1_000,
            record_trades=True,
        )
        w_eq = res_eq["windows"][0]
        self.assertEqual(w_eq["leg_a_funding_payments"], 1)
        self.assertEqual(w_eq["leg_b_funding_payments"], 1)


if __name__ == "__main__":
    unittest.main()
