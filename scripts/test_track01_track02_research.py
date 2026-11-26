#!/usr/bin/env python3
"""Synthetic tests for the Track 01 lead-lag study and Track 02 maker-hedge replay."""

from __future__ import annotations

import math
import sys
import unittest
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

try:
    import numpy as np  # noqa: E402
except ImportError:  # pragma: no cover - stdlib-only offline CI
    np = None  # type: ignore[assignment]

import lakehouse_grid as lg  # noqa: E402
import replay_maker_hedge_basis as mh  # noqa: E402
import research_funding_carry as fc  # noqa: E402
import research_lead_lag_hybrid as hl  # noqa: E402
import research_lead_lag_quant as ll  # noqa: E402
import research_shielded_maker_hedge as sm  # noqa: E402


def _random_walk(n, seed, step_bps=1.0):
    rng = np.random.default_rng(seed)
    return 100.0 * np.exp(np.cumsum(rng.normal(0.0, step_bps * 1e-4, n)))


@unittest.skipIf(np is None, "numpy not installed")
class GridTests(unittest.TestCase):
    def test_build_grid_is_causal_and_tracks_age(self):
        cols = {
            "b": np.array([9, 10, 12]),  # bucket 9 precedes t0 -> seeds index 0
            "bid": np.array([1.0, 2.0, 3.0]),
            "ask": np.array([1.1, 2.1, 3.1]),
            "bsz": np.ones(3),
            "asz": np.ones(3),
        }
        g = lg.build_grid(cols, {"raw_events": 3, "max_gap_ms": 0}, 1000, 1500, 100)
        self.assertEqual(list(g.bid), [2.0, 2.0, 3.0, 3.0, 3.0])
        self.assertEqual(list(g.age_steps), [0, 1, 0, 1, 2])

    def test_ema_causal_excludes_current_sample(self):
        out = lg.ema_causal(np.array([0.0, 10.0, 10.0]), 0.5)
        self.assertEqual(list(out), [0.0, 0.0, 5.0])


@unittest.skipIf(np is None, "numpy not installed")
class LeadLagTests(unittest.TestCase):
    def _run(self, ref, tgt_mid, delay=3):
        n = len(ref)
        half = 0.5e-4 * tgt_mid
        return ll.simulate_lead_lag(
            ref, tgt_mid - half, tgt_mid + half, np.zeros(n, int), np.zeros(n, int),
            theta_bps=2.0, hold_steps=10, alpha=1 - math.exp(-0.1 / 2.0), delay_steps=delay,
            fee_bps=0.0, warmup_steps=100, decision_steps=n, max_age_steps=50,
        )

    def test_profitable_when_target_lags_reference_beyond_delay(self):
        ref = _random_walk(20_000, 1, step_bps=2.0)
        tgt = np.concatenate([np.full(8, ref[0]), ref[:-8]])  # target lags 8 steps
        r = self._run(ref, tgt)
        self.assertGreater(r["trades"], 50)
        self.assertGreater(r["net_bps_mean"], 0.5)

    def test_no_edge_when_target_leads(self):
        tgt = _random_walk(20_000, 2, step_bps=2.0)
        ref = np.concatenate([np.full(8, tgt[0]), tgt[:-8]])  # reference lags: signal is stale
        r = self._run(ref, tgt)
        self.assertLess(r["net_bps_mean"], 0.0)

    def test_select_params_requires_min_trades_and_positive_mean(self):
        grid = [{"trades": 10, "net_bps_mean": 5.0, "net_bps_total": 50.0},
                {"trades": 40, "net_bps_mean": -1.0, "net_bps_total": -40.0}]
        self.assertIsNone(ll.select_params(grid))
        grid.append({"trades": 40, "net_bps_mean": 0.5, "net_bps_total": 20.0})
        self.assertEqual(ll.select_params(grid)["net_bps_total"], 20.0)

    def test_stale_entry_is_skipped_and_counted(self):
        n = 80
        ref = np.full(n, 100.0)
        tgt_mid = np.full(n, 100.0)
        ref[20] = 100.10  # +10 bps deviation at i=20; delay=2 -> entry e=22
        ref_age = np.zeros(n, int)
        tgt_age = np.zeros(n, int)
        tgt_age[22] = 10  # stale at arrival e=22 (> max_age_steps=5)
        r = ll.simulate_lead_lag(
            ref, tgt_mid - 0.001, tgt_mid + 0.001, ref_age, tgt_age,
            theta_bps=2.0, hold_steps=5, alpha=0.01, delay_steps=2,
            fee_bps=0.0, warmup_steps=10, decision_steps=n, max_age_steps=5,
        )
        self.assertEqual(r["trades"], 0)
        self.assertEqual(r["stale_entry_skips"], 1)
        self.assertEqual(r["deferred_exits"], 0)
        self.assertEqual(r["unsettled_exits"], 0)

    def test_deferred_exit_waits_for_fresh_quote_without_overlap(self):
        n = 80
        ref = np.full(n, 100.0)
        tgt_mid = np.full(n, 100.0)
        # Signal persists across steps 20..30; entry at e=22, planned exit x_orig=27.
        ref[20:31] = 100.10
        # Exit quotes are stale on steps 27..30, then fresh at step 31 at 99.90 (-10 bps).
        ref[31:] = 99.90
        tgt_mid[31:] = 99.90
        ref_age = np.zeros(n, int)
        tgt_age = np.zeros(n, int)
        tgt_age[27:31] = 10  # > max_age_steps=5
        r = ll.simulate_lead_lag(
            ref, tgt_mid - 0.001, tgt_mid + 0.001, ref_age, tgt_age,
            theta_bps=2.0, hold_steps=5, alpha=0.0, delay_steps=2,
            fee_bps=0.0, warmup_steps=10, decision_steps=n, max_age_steps=5,
        )
        self.assertEqual(r["trades"], 1)
        self.assertEqual(r["deferred_exits"], 1)
        self.assertEqual(r["unsettled_exits"], 0)
        self.assertLess(r["net_bps_mean"], -9.0)

    def test_unsettled_exit_counts_and_breaks_when_stream_ends_stale(self):
        n = 50
        ref = np.full(n, 100.0)
        tgt_mid = np.full(n, 100.0)
        ref[20:30] = 100.10
        ref_age = np.zeros(n, int)
        tgt_age = np.zeros(n, int)
        tgt_age[27:] = 10  # stale from planned exit through end
        r = ll.simulate_lead_lag(
            ref, tgt_mid - 0.001, tgt_mid + 0.001, ref_age, tgt_age,
            theta_bps=2.0, hold_steps=5, alpha=0.0, delay_steps=2,
            fee_bps=0.0, warmup_steps=10, decision_steps=n, max_age_steps=5,
        )
        self.assertEqual(r["trades"], 0)
        self.assertEqual(r["unsettled_exits"], 1)

    def test_exit_on_revert_defers_stale_exit(self):
        n = 80
        ref = np.full(n, 100.0)
        tgt_mid = np.full(n, 100.0)
        ref[20:23] = 100.10  # entry i=20 -> e=22; reverts at 23 -> exit sent 23+2=25
        ref[28:] = 100.05
        tgt_age = np.zeros(n, int)
        ref_age = np.zeros(n, int)
        tgt_age[25:28] = 10  # stale at revert exit 25..27, fresh at 28
        tgt_mid[28:] = 100.05
        r = ll.simulate_lead_lag(
            ref, tgt_mid - 0.001, tgt_mid + 0.001, ref_age, tgt_age,
            theta_bps=2.0, hold_steps=20, alpha=0.0, delay_steps=2,
            fee_bps=0.0, warmup_steps=10, decision_steps=n, max_age_steps=5,
            exit_on_revert=True,
        )
        self.assertEqual(r["trades"], 1)
        self.assertEqual(r["deferred_exits"], 1)
        self.assertGreater(r["net_bps_mean"], 4.0)


@unittest.skipIf(np is None, "numpy not installed")
class MakerHedgeTests(unittest.TestCase):
    def _books(self, n, mk_mid, hd_mid, half=0.005):
        z = np.zeros(n, int)
        return mk_mid - half, mk_mid + half, z, hd_mid - half, hd_mid + half, z

    def _sim(self, books, trades, **kw):
        params = dict(t0_ms=0, e0_bps=2.0, alpha=0.01, improve=False, tick=0.01,
                      maker_delay_ms=0, hedge_delay_ms=0, maker_fee_bps=0.0,
                      maker_taker_fee_bps=0.0, hedge_fee_bps=0.0, warmup_steps=10,
                      decision_steps=10_000, max_age_steps=50)
        params.update(kw)
        return mh.simulate_maker_hedge(*books, trades, **params)

    def test_strict_trade_through_fill_is_hedged_and_round_trip_profits(self):
        n = 200
        mid = np.full(n, 100.0)
        books = self._books(n, mid, mid)
        # bid cap = 99.995*exp(-2bp) -> 99.97; a sell print at 99.96 fills it, a buy print
        # at 100.04 later fills the ask (cap 100.005*exp(2bp) -> 100.03 with q=+1 skew).
        trades = {"ts": np.array([5_050, 9_050]), "sgn": np.array([-1, 1]),
                  "px": np.array([99.96, 100.04]), "sz": np.array([100.0, 100.0])}
        r = self._sim(books, trades)
        self.assertEqual((r["fills_bid"], r["fills_ask"]), (1, 1))
        self.assertGreater(r["pnl_usd"], 0.0)
        self.assertAlmostEqual(r["terminal_clips"], 0.0, places=6)

    def test_at_touch_print_does_not_fill(self):
        n = 200
        mid = np.full(n, 100.0)
        books = self._books(n, mid, mid)
        trades = {"ts": np.array([5_050]), "sgn": np.array([-1]),
                  "px": np.array([99.97]), "sz": np.array([100.0])}
        self.assertEqual(self._sim(books, trades)["fills"], 0)

    def test_lead_beta_avoids_toxic_fill_when_maker_moved_first(self):
        n = 200
        mk = np.full(n, 100.0)
        hd = np.full(n, 100.0)
        mk[50:] = 99.90  # maker venue drops 10 bp at step 50; hedge follows at step 52
        hd[52:] = 99.90
        books = self._books(n, mk, hd)
        trades = {"ts": np.array([5_150]), "sgn": np.array([-1]),
                  "px": np.array([99.88]), "sz": np.array([100.0])}
        base = self._sim(books, trades, e0_bps=1.0, hedge_delay_ms=300)
        lead = self._sim(books, trades, e0_bps=1.0, hedge_delay_ms=300, lead_beta=1.0)
        self.assertEqual(base["fills_bid"], 1)
        self.assertLess(base["mean_hedge_slip_bps"], -5.0)
        self.assertEqual(lead["fills"], 0)


class FundingCarryTests(unittest.TestCase):
    H = fc.HOUR

    def _series(self, n, rate, mark=100.0, drift=0.0):
        return {i * self.H: (rate, mark * (1 + drift * i)) for i in range(n)}

    def test_persistent_differential_is_harvested_net_of_costs(self):
        a = self._series(200, 1.0)  # A pays 1 bp/h more than B
        b = self._series(200, 0.0)
        r = fc.simulate_pair(a, b, half_life_h=6, mult=1.0, rt_cost_bps=4.0,
                             start_ms=0, end_ms=200 * self.H)
        self.assertEqual(r["entries"], 1)
        self.assertGreater(r["funding_bps"], 190)
        self.assertAlmostEqual(r["cost_bps"], 4.0)
        self.assertGreater(r["net_bps"], 180)

    def test_entry_is_causal_and_needs_hurdle(self):
        a = self._series(50, 0.05)
        b = self._series(50, 0.0)
        r = fc.simulate_pair(a, b, half_life_h=6, mult=1.0, rt_cost_bps=4.0,
                             start_ms=0, end_ms=50 * self.H)
        self.assertEqual(r["entries"], 0)  # 0.05 bp/h * 24 h < 4 bp round trip

    def test_basis_loss_and_gap_force_exit(self):
        a = self._series(100, 1.0, drift=0.001)  # shorted leg rallies 10 bp/h -> basis loss
        b = self._series(100, 0.0)
        r = fc.simulate_pair(a, b, half_life_h=6, mult=1.0, rt_cost_bps=0.0,
                             start_ms=0, end_ms=100 * self.H)
        self.assertLess(r["basis_bps"], 0)
        gap = {t: v for t, v in a.items() if not (40 * self.H <= t < 50 * self.H)}
        r2 = fc.simulate_pair(gap, b, half_life_h=6, mult=1.0, rt_cost_bps=0.0,
                              start_ms=0, end_ms=100 * self.H)
        self.assertGreaterEqual(r2["entries"], 2)

    def test_canonical_symbols_and_cost(self):
        self.assertEqual(fc.canon("trade_xyz", "xyz:NVDA"), "NVDA")
        self.assertEqual(fc.canon("trade_xyz", "xyz:SP500"), "SPX")
        self.assertEqual(fc.pair_cost("lighter", "hyperliquid", "crypto"), 2 * (4.5 + 2.0))


class HybridAndShieldedTests(unittest.TestCase):
    @unittest.skipIf(np is None, "numpy not installed")
    def test_hybrid_passive_exit_captures_spread_and_l1_depth_caps_usd(self):
        n = 300
        ref = np.full(n, 100.0)
        tgt_mid = np.full(n, 100.0)
        bid = tgt_mid - 0.05  # 10 bp wide spread!
        ask = tgt_mid + 0.05
        bsz = np.full(n, 6.0)   # $600 L1 notional -> 0.5 * L1 = $300 cap
        asz = np.full(n, 6.0)
        age = np.zeros(n, int)
        ref[50:55] = 100.08  # +8 bp pulse -> single Long entry at step 53 (t=5400ms, ask=100.05)
        # Passive buy-aggressor trade-through strictly above 100.05 at t=6000ms exits passively!
        prints = {"ts": np.array([6_000]), "sgn": np.array([1]), "px": np.array([100.06]), "sz": np.array([5.0])}
        r_pass = hl.simulate_hybrid(
            ref, bid, ask, bsz, asz, age, age, prints, t0_ms=0, theta_bps=3.0, hold_steps=50,
            alpha=0.05, delay_steps=3, taker_fee_bps=0.0, exit_mode="passive_static_touch",
            warmup_steps=10, decision_steps=n,
        )
        r_taker = hl.simulate_hybrid(
            ref, bid, ask, bsz, asz, age, age, {"ts": np.array([]), "sgn": np.array([]), "px": np.array([]), "sz": np.array([])},
            t0_ms=0, theta_bps=3.0, hold_steps=50, alpha=0.05, delay_steps=3, taker_fee_bps=0.0,
            exit_mode="taker_only", warmup_steps=10, decision_steps=n,
        )
        self.assertEqual(r_pass["trades"], 1)
        self.assertEqual(r_pass["passive_fills"], 1)
        # Entry at 100.05, passive exit at 100.05 -> 0 bp; taker exit at 99.95 -> -10 bp!
        self.assertGreater(r_pass["net_bps_mean"], r_taker["net_bps_mean"])
        # L1 cap ($300) < $1000 clip: taker loss should be $300 * (-10bp) = -$0.30, NOT -$1.00.
        self.assertAlmostEqual(r_taker["clip_pnl_usd"]["1000"], r_taker["clip_pnl_usd"]["500"], places=4)
        self.assertLess(abs(r_taker["clip_pnl_usd"]["1000"]), 0.35)

    @unittest.skipIf(np is None, "numpy not installed")
    def test_shielded_maker_blocks_toxic_binance_move_inside_speedbump(self):
        n = 400
        mk_mid = np.full(n, 100.0)
        hd_mid = np.full(n, 100.0)
        ref_mid = np.full(n, 100.0)
        # External reference crashes -10 bps at step 101 (t=5100ms), 150ms BEFORE toxic fill at 5250ms,
        # and both Lighter maker & hedge reprice down to 99.90 by hedge fill time (5250+330ms = 5580ms).
        ref_mid[101:] = 99.90
        mk_mid[110:] = 99.90
        hd_mid[108:] = 99.90
        z = np.zeros(n, int)
        mt = {"ts": np.array([5_250]), "sgn": np.array([-1]), "px": np.array([99.95]), "sz": np.array([10.0])}
        unsh = sm.simulate_shielded_maker_hedge(
            mk_mid - 0.01, mk_mid + 0.01, z, hd_mid - 0.01, hd_mid + 0.01, z, ref_mid, z, mt, None,
            t0_ms=0, e0_bps=2.0, alpha=0.01, tick=0.01, speedbump_ms=200, rtt_ms=40,
            hedge_delay_ms=330, maker_taker_fee_bps=0.0, hedge_fee_bps=0.0, shield_bps=None,
            warmup_steps=20, decision_steps=n,
        )
        sh = sm.simulate_shielded_maker_hedge(
            mk_mid - 0.01, mk_mid + 0.01, z, hd_mid - 0.01, hd_mid + 0.01, z, ref_mid, z, mt, None,
            t0_ms=0, e0_bps=2.0, alpha=0.01, tick=0.01, speedbump_ms=200, rtt_ms=40,
            hedge_delay_ms=330, maker_taker_fee_bps=0.0, hedge_fee_bps=0.0, shield_bps=2.5,
            warmup_steps=20, decision_steps=n,
        )
        self.assertEqual(unsh["fills"], 1)
        self.assertLess(unsh["pnl_usd"], 0.0)
        self.assertEqual(sh["fills"], 0)
        self.assertEqual(sh["blocked_toxic"], 1)
        import json
        json.dumps({"unsh": unsh, "sh": sh}, allow_nan=False)

    @unittest.skipIf(np is None, "numpy not installed")
    def test_hybrid_stale_entry_deferred_and_unsettled_exit_no_overlap(self):
        n = 80
        ref = np.full(n, 100.0)
        tgt_mid = np.full(n, 100.0)
        bid = tgt_mid - 0.001
        ask = tgt_mid + 0.001
        sz = np.full(n, 50.0)
        ref_age = np.zeros(n, int)

        # 1. Stale arrival at e=22
        tgt_age = np.zeros(n, int)
        ref[20] = 100.10
        tgt_age[22] = 10
        r_stale = hl.simulate_hybrid(
            ref, bid, ask, sz, sz, ref_age, tgt_age, None,
            t0_ms=0, theta_bps=2.0, hold_steps=5, alpha=0.01, delay_steps=2,
            taker_fee_bps=0.0, exit_mode="taker_only", warmup_steps=10,
            decision_steps=n, max_age_steps=5,
        )
        self.assertEqual(r_stale["trades"], 0)
        self.assertEqual(r_stale["stale_entry_skips"], 1)

        # 2. Deferred exit blocks overlapping candidate while waiting for fresh quote
        ref[:] = 100.0
        ref[20:31] = 100.10
        ref[31:] = 99.90
        tgt_age = np.zeros(n, int)
        tgt_age[27:31] = 10  # planned exit 27 stale until 31
        bid2 = bid.copy()
        ask2 = ask.copy()
        bid2[31:] = 99.90 - 0.001
        ask2[31:] = 99.90 + 0.001
        r_def = hl.simulate_hybrid(
            ref, bid2, ask2, sz, sz, ref_age, tgt_age, None,
            t0_ms=0, theta_bps=2.0, hold_steps=5, alpha=0.0, delay_steps=2,
            taker_fee_bps=0.0, exit_mode="taker_only", warmup_steps=10,
            decision_steps=n, max_age_steps=5,
        )
        self.assertEqual(r_def["trades"], 1)
        self.assertEqual(r_def["deferred_exits"], 1)
        self.assertEqual(r_def["unsettled_exits"], 0)
        self.assertLess(r_def["net_bps_mean"], -9.0)

        # 3. Unsettled exit when stale through end
        tgt_age_end = np.zeros(n, int)
        tgt_age_end[27:] = 10
        r_uns = hl.simulate_hybrid(
            ref, bid, ask, sz, sz, ref_age, tgt_age_end, None,
            t0_ms=0, theta_bps=2.0, hold_steps=5, alpha=0.01, delay_steps=2,
            taker_fee_bps=0.0, exit_mode="taker_only", warmup_steps=10,
            decision_steps=n, max_age_steps=5,
        )
        self.assertEqual(r_uns["trades"], 0)
        self.assertEqual(r_uns["unsettled_exits"], 1)

    @unittest.skipIf(np is None, "numpy not installed")
    def test_shielded_maker_unhedged_stale_skip_and_cross_stablecoin_drag(self):
        n = 1_500
        mk_mid = np.full(n, 100.0)
        hd_mid = np.full(n, 100.0)
        ref_mid = np.full(n, 100.0)
        z = np.zeros(n, int)
        # Fill at t=2_050ms -> k=40 (<= 1_500); hedge time j ~ (2050+330)//50 - 1 = 46
        mt = {"ts": np.array([2_050]), "sgn": np.array([-1]), "px": np.array([99.95]), "sz": np.array([10.0])}

        # 1. Hedge stays stale past the 1_200-step scan -> skipped cleanly with zero ledger mutation
        hd_age_stale = np.zeros(n, int)
        hd_age_stale[45:] = 999
        r_skip = sm.simulate_shielded_maker_hedge(
            mk_mid - 0.01, mk_mid + 0.01, z, hd_mid - 0.01, hd_mid + 0.01, hd_age_stale, ref_mid, z, mt, None,
            t0_ms=0, e0_bps=2.0, alpha=0.01, tick=0.01, speedbump_ms=200, rtt_ms=40,
            hedge_delay_ms=330, maker_taker_fee_bps=0.0, hedge_fee_bps=0.0, shield_bps=None,
            warmup_steps=20, decision_steps=n, max_age_steps=50,
        )
        self.assertEqual(r_skip["fills"], 0)
        self.assertEqual(r_skip["unhedged_stale_skips"], 1)
        self.assertEqual(r_skip["maker_notional_usd"], 0.0)
        self.assertEqual(r_skip["pnl_usd"], 0.0)

        # 2. Cross-stablecoin drag (1 bp and 2 bp) reduces PnL proportionally to maker_notional_usd
        r0 = sm.simulate_shielded_maker_hedge(
            mk_mid - 0.01, mk_mid + 0.01, z, hd_mid - 0.01, hd_mid + 0.01, z, ref_mid, z, mt, None,
            t0_ms=0, e0_bps=2.0, alpha=0.01, tick=0.01, speedbump_ms=200, rtt_ms=40,
            hedge_delay_ms=330, maker_taker_fee_bps=0.0, hedge_fee_bps=0.0, shield_bps=None,
            warmup_steps=20, decision_steps=n, cross_stablecoin_bps=0.0,
        )
        r1 = sm.simulate_shielded_maker_hedge(
            mk_mid - 0.01, mk_mid + 0.01, z, hd_mid - 0.01, hd_mid + 0.01, z, ref_mid, z, mt, None,
            t0_ms=0, e0_bps=2.0, alpha=0.01, tick=0.01, speedbump_ms=200, rtt_ms=40,
            hedge_delay_ms=330, maker_taker_fee_bps=0.0, hedge_fee_bps=0.0, shield_bps=None,
            warmup_steps=20, decision_steps=n, cross_stablecoin_bps=1.0,
        )
        r2 = sm.simulate_shielded_maker_hedge(
            mk_mid - 0.01, mk_mid + 0.01, z, hd_mid - 0.01, hd_mid + 0.01, z, ref_mid, z, mt, None,
            t0_ms=0, e0_bps=2.0, alpha=0.01, tick=0.01, speedbump_ms=200, rtt_ms=40,
            hedge_delay_ms=330, maker_taker_fee_bps=0.0, hedge_fee_bps=0.0, shield_bps=None,
            warmup_steps=20, decision_steps=n, cross_stablecoin_bps=2.0,
        )
        self.assertEqual(r0["fills"], 1)
        self.assertGreater(r0["maker_notional_usd"], 0.0)
        self.assertAlmostEqual(r0["pnl_usd"] - r1["pnl_usd"], r0["maker_notional_usd"] * 1e-4, places=3)
        self.assertAlmostEqual(r0["pnl_usd"] - r2["pnl_usd"], r0["maker_notional_usd"] * 2e-4, places=3)

    def test_shielded_discover_windows_respects_max_window(self):
        import tempfile
        with tempfile.TemporaryDirectory() as td:
            root = Path(td)
            for w, cid in ((19, "tokyo-crypto-20261123T061504Z-w19"),
                           (24, "tokyo-crypto-20261125T093007Z-w24"),
                           (25, "tokyo-crypto-20261125T194507Z-w25")):
                (root / "parquet/bbo_quotes/collector_region=tokyo/venue=lighter/utc_date=2026-11-25" / f"campaign_id={cid}").mkdir(parents=True, exist_ok=True)
            self.assertEqual(sorted(sm.discover_windows(root, "crypto", max_window=24).keys()), [19, 24])
            self.assertEqual(sorted(sm.discover_windows(root, "crypto", max_window=25).keys()), [19, 24, 25])

    @unittest.skipIf(np is None, "numpy not installed")
    def test_hybrid_taker_only_print_verified_matches_quant_mean(self):
        n = 120
        ref = np.full(n, 100.0)
        tgt_mid = np.full(n, 100.0)
        bid = tgt_mid - 0.01
        ask = tgt_mid + 0.01
        sz = np.full(n, 20.0)
        z = np.zeros(n, int)
        # Single Long signal at i=20 -> delay=2 -> entry e=22 (t_entry_wall=2300ms),
        # hold=10 -> exit x_=32 (t_exit_wall=3300ms).
        ref[20] = 100.10
        # Multiple matching-side prints inside [2300, 3300) and [3300, 4300) with distinct prices,
        # plus opposite-side noise prints that must be ignored.
        prints = {
            "ts": np.array([2350, 2500, 2800, 3300, 3600, 3900]),
            "sgn": np.array([1, -1, 1, -1, 1, -1]),
            "px": np.array([100.00, 99.50, 100.20, 100.40, 101.00, 100.60]),
            "sz": np.full(6, 5.0),
        }
        r_quant = ll.simulate_lead_lag(
            ref, bid, ask, z, z,
            theta_bps=2.0, hold_steps=10, alpha=0.01, delay_steps=2,
            fee_bps=0.5, warmup_steps=10, decision_steps=n, max_age_steps=5,
            prints=prints, t0_ms=0,
        )
        r_hybrid = hl.simulate_hybrid(
            ref, bid, ask, sz, sz, z, z, prints,
            t0_ms=0, theta_bps=2.0, hold_steps=10, alpha=0.01, delay_steps=2,
            taker_fee_bps=0.5, exit_mode="taker_only",
            warmup_steps=10, decision_steps=n, max_age_steps=5,
        )
        self.assertEqual(r_hybrid["print_verified_trades"], 1)
        self.assertEqual(r_hybrid["print_verified_trades"], r_quant["print_verified_trades"])
        self.assertEqual(r_hybrid["print_verified_net_bps_mean"], r_quant["print_verified_net_bps_mean"])
        # Mean entry = (100.00+100.20)/2 = 100.10, mean exit = (100.40+100.60)/2 = 100.50 -> ~38.96 bps net
        # (whereas first-print would be 100.00 -> 100.40 = ~39.00 bps net).
        expected = round(((100.50 / 100.10) - 1.0) * 1e4 - 1.0, 4)
        self.assertEqual(r_hybrid["print_verified_net_bps_mean"], expected)

    def test_resolve_pairs_rwa_external_ref_disjoint_from_maker_and_hedge(self):
        self.assertEqual(sm.resolve_pairs(rwa_external_ref=False), sm.ALL_PAIRS)
        ext_pairs = sm.resolve_pairs(rwa_external_ref=True)
        self.assertEqual(len(ext_pairs), len(sm.ALL_PAIRS))
        rwa_pairs = [p for p in ext_pairs if p[7] == "rwa"]
        self.assertGreater(len(rwa_pairs), 0)
        for cohort, mv, mm, hv, hm, rv, rm, domain, label in rwa_pairs:
            self.assertNotEqual(rv, hv, f"{label}: ref_venue must differ from hd_venue")
            self.assertNotEqual(rv, mv, f"{label}: ref_venue must differ from mk_venue")
            self.assertFalse(rv == hv and rm == hm, f"{label}: shield_is_self_hedge_venue must be False")
        # Crypto pairs remain untouched.
        self.assertEqual(
            [p for p in ext_pairs if p[7] == "crypto"],
            [p for p in sm.ALL_PAIRS if p[7] == "crypto"],
        )

    @unittest.skipIf(np is None, "numpy not installed")
    def test_hybrid_passive_exit_excludes_exit_print_from_entry_witness(self):
        n = 120
        ref = np.full(n, 100.0)
        tgt_mid = np.full(n, 100.0)
        bid = tgt_mid - 0.01
        ask = tgt_mid + 0.01
        sz = np.full(n, 20.0)
        z = np.zeros(n, int)
        # Long signal at i=20 -> e=22 (t_entry_wall=2300ms), static passive exit ask = 100.01.
        ref[20] = 100.10

        # Case 1: Only the passive-exit trade-through print exists at 2600ms (< 1000ms after entry).
        # It must NOT self-verify the earlier entry.
        only_exit_prints = {
            "ts": np.array([2600]),
            "sgn": np.array([1]),
            "px": np.array([100.05]),
            "sz": np.array([5.0]),
        }
        r1 = hl.simulate_hybrid(
            ref, bid, ask, sz, sz, z, z, only_exit_prints,
            t0_ms=0, theta_bps=2.0, hold_steps=10, alpha=0.01, delay_steps=2,
            taker_fee_bps=0.0, maker_fee_bps=0.0, exit_mode="passive_static_touch",
            warmup_steps=10, decision_steps=n, max_age_steps=5,
        )
        self.assertEqual(r1["passive_fills"], 1)
        self.assertEqual(r1["print_verified_trades"], 0)

        # Case 2: An independent earlier entry print at 2350ms (100.00) precedes the exit
        # witness at 2600ms (100.20) -> entry uses only 100.00, exit is static_limit 100.01.
        with_entry_prints = {
            "ts": np.array([2350, 2600]),
            "sgn": np.array([1, 1]),
            "px": np.array([100.00, 100.20]),
            "sz": np.array([5.0, 5.0]),
        }
        r2 = hl.simulate_hybrid(
            ref, bid, ask, sz, sz, z, z, with_entry_prints,
            t0_ms=0, theta_bps=2.0, hold_steps=10, alpha=0.01, delay_steps=2,
            taker_fee_bps=0.0, maker_fee_bps=0.0, exit_mode="passive_static_touch",
            warmup_steps=10, decision_steps=n, max_age_steps=5,
        )
        self.assertEqual(r2["passive_fills"], 1)
        self.assertEqual(r2["print_verified_trades"], 1)
        self.assertAlmostEqual(r2["print_verified_net_bps_mean"], (100.01 / 100.00 - 1.0) * 1e4, places=3)


if __name__ == "__main__":
    unittest.main()
