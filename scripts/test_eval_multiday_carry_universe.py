#!/usr/bin/env python3
"""Offline unit tests for `scripts/eval_multiday_carry_universe.py`."""

from __future__ import annotations

import ast
from contextlib import redirect_stdout
from decimal import Decimal
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from eval_multiday_carry_universe import (  # noqa: E402
    COHORT_CRYPTO,
    COHORT_EQUITY_RWA,
    COST_REGIME_NAMES,
    DECIMAL_ZERO,
    EMBARGO_HOURS,
    HOUR_MS,
    MISSING_CASH_STATUS,
    PairSpec,
    US_PER_HOUR,
    _build_synthetic_archive,
    align_pair_rates,
    compute_causal_signal_at_hour,
    compute_chronological_split,
    evaluate_universe_offline,
    extract_known_fee_universe,
    load_verified_offline_archive,
    main,
    parse_arcus_funding_payloads,
    parse_hyperliquid_funding_payloads,
    simulate_equal_weight_topk_portfolio,
    simulate_pair_variant,
    simulate_static_train_selected_holdout,
)
from screen_cross_venue_pairs import build_embedded_self_test_snapshot  # noqa: E402


class _ForbiddenEnvGuard:
    """Temporarily inject decoy secrets and ban secret reads."""

    def __init__(self) -> None:
        self._orig_environ = os.environ.copy()

    def __enter__(self) -> "_ForbiddenEnvGuard":
        os.environ["ARCUS_PRIVATE_KEY"] = "0xdeadbeef_secret_should_never_be_read"
        os.environ["LIGHTER_API_KEY"] = "secret_api_key_do_not_read"
        return self

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        os.environ.clear()
        os.environ.update(self._orig_environ)


def _run_cli(argv: list[str]) -> tuple[int, str, str]:
    buf = io.StringIO()
    err = io.StringIO()
    with redirect_stdout(buf):
        orig_err = sys.stderr
        sys.stderr = err
        try:
            code = main(argv)
        finally:
            sys.stderr = orig_err
    return code, buf.getvalue(), err.getvalue()


class TestEvalMultidayCarryUniverse(unittest.TestCase):
    def test_sign_convention_jitter_snap_and_conflict_rejection(self) -> None:
        base_ms = 1_793_000_400_000 - (1_793_000_400_000 % HOUR_MS)
        arc_payload = json.dumps(
            {
                "fundingRates": [
                    {"marketDisplayName": "NVDA-USD", "time": base_ms * 1000, "fundingRate": "0.0001"},
                    # Off-grid microseconds -> rejected
                    {"marketDisplayName": "NVDA-USD", "time": base_ms * 1000 + 500, "fundingRate": "0.009"},
                ]
            }
        ).encode("utf-8")
        hl_payload = json.dumps(
            [
                # +139ms jitter <= 60s -> snapped cleanly to base_ms
                {"coin": "xyz:NVDA", "time": base_ms + 139, "fundingRate": "0.0003"},
                # Same hour conflict with different rate -> excluded cleanly
                {"coin": "xyz:NVDA", "time": base_ms + HOUR_MS, "fundingRate": "0.0005"},
                {"coin": "xyz:NVDA", "time": base_ms + HOUR_MS + 10, "fundingRate": "0.0009"},
                # >60s off-grid -> excluded cleanly
                {"coin": "xyz:NVDA", "time": base_ms + 2 * HOUR_MS + 90_000, "fundingRate": "0.0007"},
            ]
        ).encode("utf-8")

        sa = parse_arcus_funding_payloads("NVDA-USD", [arc_payload])
        sb = parse_hyperliquid_funding_payloads("trade_xyz", "xyz:NVDA", [hl_payload])
        self.assertEqual(sa.off_grid_rows, 1)
        self.assertEqual(sb.max_jitter_ms, 139)
        self.assertEqual(sb.off_grid_rows, 1)
        self.assertEqual(sb.duplicate_conflict_rows, 1)
        self.assertNotIn(base_ms + HOUR_MS, sb.rates_by_hour_ms)

        spec = PairSpec(
            "arcus:NVDA-USD__vs__trade_xyz:xyz:NVDA",
            "NVDA",
            "equity",
            COHORT_EQUITY_RWA,
            "arcus",
            "NVDA-USD",
            "trade_xyz",
            "xyz:NVDA",
            Decimal("1.8"),
            Decimal("6.3"),
            "mixed",
            "maker_first_arcus",
            (),
        )
        aligned = align_pair_rates(spec, sa, sb)
        # Positive rate means longs pay, shorts receive -> Long A / Short B = (rate_b - rate_a) * 10_000 = 2.0 bps
        self.assertEqual(aligned.common_hours, [base_ms])
        self.assertEqual(aligned.carry_by_hour[base_ms], Decimal("2.0"))

        # Verify static Long A/Short B vs Short A/Long B symmetry minus round-trip fee
        m_long = simulate_pair_variant(
            carry_by_hour=aligned.carry_by_hour,
            eval_hours=[base_ms],
            rt_cost_bps=Decimal("1.8"),
            mode="static_long_a_short_b",
        )
        m_short = simulate_pair_variant(
            carry_by_hour=aligned.carry_by_hour,
            eval_hours=[base_ms],
            rt_cost_bps=Decimal("1.8"),
            mode="static_short_a_long_b",
        )
        self.assertEqual(m_long.gross_carry_bps, Decimal("2.0"))
        self.assertEqual(m_long.fee_bps, Decimal("1.8"))
        self.assertEqual(m_long.net_carry_bps, Decimal("0.2"))
        self.assertEqual(m_short.gross_carry_bps, Decimal("-2.0"))
        self.assertEqual(m_short.fee_bps, Decimal("1.8"))
        self.assertEqual(m_short.net_carry_bps, Decimal("-3.8"))

    def test_strict_causality_future_mutation_cannot_change_signal(self) -> None:
        base_ms = 1_793_000_000_000 - (1_793_000_000_000 % HOUR_MS)
        hours = [base_ms + i * HOUR_MS for i in range(40)]
        carry_orig = {h: Decimal("0.20") for h in hours}
        carry_mutated = dict(carry_orig)

        target_h = hours[30]
        # Flip hours[30] and all future hours wildly negative
        for h in hours[30:]:
            carry_mutated[h] = Decimal("-99.0")

        sig_orig = compute_causal_signal_at_hour(carry_orig, target_h, lookback_h=24)
        sig_mut = compute_causal_signal_at_hour(carry_mutated, target_h, lookback_h=24)
        self.assertEqual(sig_orig, sig_mut)
        self.assertIsNotNone(sig_orig)
        assert sig_orig is not None
        self.assertEqual(sig_orig.direction, 1)

    def test_missing_hour_gap_breaks_lookback_and_forces_flat_with_exit_fee(self) -> None:
        base_ms = 1_793_000_000_000 - (1_793_000_000_000 % HOUR_MS)
        # Hours 0..27 contiguous (+0.20 bps), then skip hours 28..29 (2h gap!), then 30..60 contiguous (+0.20 bps)
        hours = [base_ms + i * HOUR_MS for i in range(60) if i not in {28, 29}]
        carry = {h: Decimal("0.20") for h in hours}

        # At hour 24..27: causal 24h lookback is intact -> enters at hour 24!
        # Across gap after hour 27 -> forced flat, charging 0.9 half_fee on exit before gap.
        # At hour 30..53: 24h trailing window is broken by missing 28..29 -> cannot re-enter until hour 54!
        res = simulate_pair_variant(
            carry_by_hour=carry,
            eval_hours=hours,
            rt_cost_bps=Decimal("1.8"),
            mode="causal_dynamic",
            lookback_h=24,
            hurdle_mult=Decimal("1.5"),
        )
        # First episode: hours 24, 25, 26, 27 = 4h, closed at gap (1.8 RT fee)
        # Re-entry only at hour 54 (once [30..53] gives 24 contiguous hours): hours 54..59 = 6h, closed at cutoff (1.8 RT fee)
        self.assertEqual(res.episodes, 2)
        self.assertEqual(res.active_hours, 10)
        self.assertEqual(res.fee_bps, Decimal("3.6"))
        self.assertEqual(res.gross_carry_bps, Decimal("2.0"))
        self.assertEqual(res.net_carry_bps, Decimal("-1.6"))

    def test_cost_regimes_and_zero_trade_behavior(self) -> None:
        spec = PairSpec(
            "arcus:ANT-USD__vs__trade_xyz:xyz:ANT",
            "ANT",
            "equity",
            COHORT_EQUITY_RWA,
            "arcus",
            "ANT-USD",
            "trade_xyz",
            "xyz:ANT",
            Decimal("1.8"),
            Decimal("4.04"),
            "mixed",
            "maker_first_arcus",
            (),
        )
        expected = {
            "rt_mt": Decimal("1.8"),
            "rt_tt": Decimal("4.04"),
            "rt_mt_plus_4bps": Decimal("5.8"),
            "rt_mt_plus_10bps": Decimal("11.8"),
            "rt_tt_plus_10bps": Decimal("14.04"),
        }
        for reg, val in expected.items():
            self.assertEqual(spec.regime_cost_bps(reg), val)

        # Hurdle too high -> zero trades, zero fees, zero PnL
        base_ms = 1_793_000_000_000 - (1_793_000_000_000 % HOUR_MS)
        hours = [base_ms + i * HOUR_MS for i in range(48)]
        carry = {h: Decimal("0.05") for h in hours}  # 0.05 * 72 = 3.6 < 11.8 * 1.5 = 17.7
        m = simulate_pair_variant(
            carry_by_hour=carry,
            eval_hours=hours,
            rt_cost_bps=spec.regime_cost_bps("rt_mt_plus_10bps"),
            mode="causal_dynamic",
            lookback_h=24,
            hurdle_mult=Decimal("1.5"),
        )
        self.assertEqual(m.episodes, 0)
        self.assertEqual(m.active_hours, 0)
        self.assertEqual(m.fee_bps, DECIMAL_ZERO)
        self.assertEqual(m.net_carry_bps, DECIMAL_ZERO)

    def test_chronological_split_embargo_and_static_train_selection_isolation(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = _build_synthetic_archive(Path(tmp), num_hours=300)
            _, pairs, series = load_verified_offline_archive(root)
            aligned = [
                align_pair_rates(sp, series[(sp.leg_a_venue, sp.leg_a_symbol)], series[(sp.leg_b_venue, sp.leg_b_symbol)])
                for sp in pairs
            ]
            split = compute_chronological_split(aligned)
            self.assertEqual(split["embargo_hours"], EMBARGO_HOURS)
            self.assertEqual(
                (split["embargo_start_ms"] - split["train_end_ms"]) // HOUR_MS,
                1,
            )
            self.assertEqual(
                (split["holdout_start_ms"] - split["embargo_end_ms"]) // HOUR_MS,
                1,
            )
            self.assertEqual(
                split["train_grid_hours"] + EMBARGO_HOURS + split["holdout_grid_hours"],
                split["total_grid_hours"],
            )

            # Mutate holdout in TSLA to verify Train selection for Static Top-K does not look at Holdout
            eq_pairs = [p for p in aligned if p.spec.cohort == COHORT_EQUITY_RWA]
            st_a = simulate_static_train_selected_holdout(
                cohort_pairs=eq_pairs,
                train_start_ms=split["train_start_ms"],
                train_end_ms=split["train_end_ms"],
                holdout_start_ms=split["holdout_start_ms"],
                holdout_end_ms=split["holdout_end_ms"],
                k_slots=3,
                regime="rt_mt",
            )
            # In synthetic fixture TSLA had -2.0 bps/h on train so selected as short_a_long_b,
            # then flips +1.8 bps/h on holdout so holdout loses!
            tsla_sel = [s for s in st_a["selected_pairs"] if "TSLA" in s["pair_id"]][0]
            self.assertEqual(tsla_sel["direction"], "short_a_long_b")
            self.assertGreater(tsla_sel["train_net_bps"], 0)
            self.assertLess(tsla_sel["holdout_net_bps"], 0)

    def test_topk_portfolio_cohort_isolation_and_no_hourly_churn(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = _build_synthetic_archive(Path(tmp), num_hours=300)
            _, pairs, series = load_verified_offline_archive(root)
            res = evaluate_universe_offline(
                universe_pairs=pairs,
                venue_series=series,
                metadata={"fetched_at_utc": "2026-11-23T00:00:00+00:00"},
            )
            self.assertEqual(res["full_realized_cash_usd"], None)
            self.assertEqual(res["full_realized_cash_status"], MISSING_CASH_STATUS)

            # NVDA is persistently +2.4 bps/h (> hurdle), so held in equity_rwa without spurious hourly churn
            eq_p3 = res["cohorts"][COHORT_EQUITY_RWA]["variants"]["causal_L24h_M1p5"]["rt_mt"]["portfolios_by_k"]["top_3"]["full"]
            self.assertLessEqual(eq_p3["episodes"], 4)
            self.assertGreater(eq_p3["net_carry_bps"], 0)
            # Crypto never appears inside equity_rwa unique_pair_ids
            for pid in eq_p3["unique_pair_ids"]:
                self.assertIn("trade_xyz", pid)

            cr_p3 = res["cohorts"][COHORT_CRYPTO]["variants"]["causal_L24h_M1p5"]["rt_mt"]["portfolios_by_k"]["top_3"]["full"]
            for pid in cr_p3["unique_pair_ids"]:
                self.assertIn("hyperliquid", pid)

    def test_manifest_tamper_byte_mismatch_and_path_traversal_fail_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = _build_synthetic_archive(Path(tmp), num_hours=180)
            manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))

            # 1. Tamper raw payload -> SHA-256 or length mismatch
            target = root / manifest["receipts"][0]["rel_path"]
            orig = target.read_bytes()
            target.write_bytes(b" " + orig[1:])
            with self.assertRaises(ValueError):
                load_verified_offline_archive(root)
            target.write_bytes(orig)

            # 2. Byte count tampering -> ValueError
            target.write_bytes(orig + b" ")
            with self.assertRaises(ValueError):
                load_verified_offline_archive(root)
            target.write_bytes(orig)

            # 3. Path traversal in manifest -> ValueError
            manifest["receipts"][0]["rel_path"] = "../outside.json"
            (root / "manifest.json").write_text(json.dumps(manifest), encoding="utf-8")
            with self.assertRaises(ValueError):
                load_verified_offline_archive(root)

    def test_offline_zero_network_zero_secret_env_and_ast_guard(self) -> None:
        with _ForbiddenEnvGuard():
            with patch("eval_multiday_carry_universe._http_fetch_bytes", side_effect=AssertionError("network forbidden")):
                code_st, stdout_st, stderr_st = _run_cli(["--self-test"])
                self.assertEqual(code_st, 0, msg=stderr_st)
                st_doc = json.loads(stdout_st)
                self.assertEqual(st_doc["self_test"], "passed")
                self.assertTrue(st_doc["zero_secret_env_reads_verified"])
                self.assertIsNone(st_doc["full_realized_cash_usd"])
                self.assertNotIn("secret", stdout_st.lower().replace("zero_secret_env_reads_verified", ""))

                # Offline CLI roundtrip determinism check
                with tempfile.TemporaryDirectory() as tmp:
                    arch = _build_synthetic_archive(Path(tmp), num_hours=240)
                    c1, out1, err1 = _run_cli(["--input-dir", str(arch)])
                    c2, out2, err2 = _run_cli(["--input-dir", str(arch)])
                    self.assertEqual(c1, 0, msg=err1)
                    self.assertEqual(c2, 0, msg=err2)
                    self.assertEqual(out1, out2)

        # Static AST guard: no non-docstring string literals mention secret env/file names,
        # and only _build_ssl_context touches os.environ.
        src = (REPO_ROOT / "scripts" / "eval_multiday_carry_universe.py").read_text(encoding="utf-8")
        tree = ast.parse(src)
        doc_ids: set[int] = set()
        for node in ast.walk(tree):
            if isinstance(node, (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef)) and node.body:
                first = node.body[0]
                if isinstance(first, ast.Expr) and isinstance(first.value, ast.Constant) and isinstance(first.value.value, str):
                    doc_ids.add(id(first.value))
        for node in ast.walk(tree):
            if isinstance(node, ast.Constant) and isinstance(node.value, str) and id(node) not in doc_ids:
                for forbidden in (".env", "PRIVATE_KEY", "API_KEY", "API_SECRET", "MNEMONIC"):
                    self.assertNotIn(forbidden, node.value)

        env_funcs: set[str] = set()
        for func in ast.walk(tree):
            if isinstance(func, ast.FunctionDef):
                for inner in ast.walk(func):
                    if isinstance(inner, ast.Attribute) and inner.attr in ("environ", "environb", "getenv"):
                        env_funcs.add(func.name)
        self.assertEqual(env_funcs, {"_build_ssl_context"})

    def test_extract_known_fee_universe_uses_embedded_fixture_cleanly(self) -> None:
        snap = build_embedded_self_test_snapshot()
        # With empty/no account fee evidence, only public_l0 schedules have fees,
        # which screen_cross_venue_pairs preserves deterministically.
        pairs = extract_known_fee_universe(snap, None)
        self.assertTrue(all(p.rt_mt_bps is not None and p.rt_tt_bps is not None for p in pairs))
        self.assertTrue(all(p.leg_a_venue == "arcus" for p in pairs))
        self.assertTrue(all(p.leg_b_venue in {"hyperliquid", "trade_xyz"} for p in pairs))


if __name__ == "__main__":
    unittest.main()
