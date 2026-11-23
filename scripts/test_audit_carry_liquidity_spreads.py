#!/usr/bin/env python3
"""Offline unit tests for `scripts/audit_carry_liquidity_spreads.py`."""

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

from audit_carry_liquidity_spreads import (  # noqa: E402
    HISTORICAL_CASH_STATUS,
    _build_synthetic_liquidity_archive,
    _evaluate_and_optional_write,
    _fetch_live_liquidity_archive,
    _parse_iso_utc_epoch_us,
    _sha256_bytes,
    _strict_positive_int,
    evaluate_carry_liquidity_offline,
    load_verified_liquidity_archive,
    main,
    parse_arcus_bbo,
    parse_hl_l2_book,
)


class _ForbiddenEnvGuard:
    def __init__(self) -> None:
        self._orig = os.environ.copy()

    def __enter__(self) -> "_ForbiddenEnvGuard":
        os.environ["ARCUS_PRIVATE_KEY"] = "0xdeadbeef_do_not_read"
        os.environ["LIGHTER_API_KEY"] = "do_not_read_api_key"
        return self

    def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
        os.environ.clear()
        os.environ.update(self._orig)


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


class TestAuditCarryLiquiditySpreads(unittest.TestCase):
    def test_exact_vwap_slippage_and_crossed_or_http_error_books(self) -> None:
        # Multi-level HL book: L1 bid=99.90 sz=1 ($99.90), L2 bid=99.00 sz=10 ($990)
        # L1 ask=100.10 sz=1 ($100.10), L2 ask=101.00 sz=10 ($1010)
        raw_l2 = json.dumps(
            {
                "coin": "TEST",
                "time": 1795387000000,
                "levels": [
                    [{"px": "99.90", "sz": "1", "n": 1}, {"px": "99.00", "sz": "10", "n": 2}],
                    [{"px": "100.10", "sz": "1", "n": 1}, {"px": "101.00", "sz": "10", "n": 2}],
                ],
            }
        ).encode("utf-8")
        book = parse_hl_l2_book("hyperliquid", "TEST", raw_l2)
        self.assertEqual(book.status, "ok")
        self.assertEqual(book.mid_px, Decimal("100.0"))
        self.assertEqual(book.spread_bps, Decimal("20"))
        # $100 buy fills completely at 100.10; $100 sell takes $99.90 (1 unit) + $0.10 at 99.00 -> > 20 bps
        self.assertIsNotNone(book.rt_slip_bps_by_tier[100])
        assert book.rt_slip_bps_by_tier[100] is not None
        self.assertGreater(book.rt_slip_bps_by_tier[100], Decimal("20"))
        self.assertIsNotNone(book.rt_slip_bps_by_tier[500])
        self.assertGreater(book.rt_slip_bps_by_tier[500], book.rt_slip_bps_by_tier[100])
        # $1,000 still fits ($99.90+$990 = $1089.90 bids, $100.10+$1010 = $1110.10 asks)
        self.assertIsNotNone(book.rt_slip_bps_by_tier[1000])

        # Crossed Arcus BBO fails closed
        crossed = parse_arcus_bbo(
            "TEST-USD",
            json.dumps(
                {
                    "bestBid": {"price": "100.05", "size": "1"},
                    "bestAsk": {"price": "100.00", "size": "1"},
                    "timestamp": 1,
                }
            ).encode("utf-8"),
        )
        self.assertEqual(crossed.status, "crossed_book")
        self.assertIsNone(crossed.spread_bps)

        # Empty/null HL response fails closed cleanly
        self.assertEqual(parse_hl_l2_book("hyperliquid", "MISSING", b"null").status, "empty_book")
        err_arc = parse_arcus_bbo("MISSING-USD", json.dumps({"_http_error": "HTTP 404: Not Found"}).encode("utf-8"))
        self.assertEqual(err_arc.status, "http_error")

    def test_nonempty_peer_book_requires_the_requested_coin(self) -> None:
        payload = {
            "time": 1795387000000,
            "levels": [[{"px": "99.90", "sz": "1"}], [{"px": "100.10", "sz": "1"}]],
        }
        for identity in ({"coin": "xyz:OTHER"}, {}):
            with self.subTest(identity=identity):
                raw = json.dumps(payload | identity).encode("utf-8")
                with self.assertRaisesRegex(ValueError, "response coin does not match the requested market"):
                    parse_hl_l2_book("trade_xyz", "xyz:NVDA", raw)

    def test_thin_l1_and_oracle_divergence_fail_hard_gates(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            liq_dir = _build_synthetic_liquidity_archive(Path(tmp))
            m, pairs, cs, am, hc, ab, hb = load_verified_liquidity_archive(liq_dir)
            # Mutate BTC oracle in memory to deviate by 100 bps (>50 bps threshold)
            from audit_carry_liquidity_spreads import HlMarketContext

            hc[("hyperliquid", "BTC")] = HlMarketContext(
                venue="hyperliquid",
                coin="BTC",
                oracle_px=Decimal("101.0"),  # vs Arcus 100.0 => 100 bps divergence!
                mark_px=Decimal("100.0"),
                mid_px=Decimal("100.0"),
                funding_rate=Decimal("0.00025"),
                premium=Decimal("0"),
                day_ntl_vlm_usd=Decimal("1000000"),
                open_interest_base=Decimal("1000"),
            )
            res = evaluate_carry_liquidity_offline(
                universe_pairs=pairs,
                carry_series=cs,
                arcus_markets=am,
                hl_ctxs=hc,
                arcus_bbos=ab,
                hl_books=hb,
                metadata={"fetched_at_utc": m["fetched_at_utc"], "receipt_count": m["receipt_count"]},
            )
            self.assertIsNone(res["historical_40d_realized_cash_usd"])
            self.assertEqual(res["historical_40d_realized_cash_status"], HISTORICAL_CASH_STATUS)

            tsla = next(r for r in res["pairs"] if r["canonical_base"] == "TSLA")
            self.assertFalse(tsla["depth_100usd_ok"])
            self.assertIn("arcus_l1_lt_100usd", ",".join(tsla["flags"]))
            self.assertIn("equity_outside_rth_snapshot", tsla["flags"])

            btc = next(r for r in res["pairs"] if r["canonical_base"] == "BTC")
            self.assertFalse(btc["oracle_ok"])
            self.assertFalse(btc["passes_hard_gates"])
            self.assertIn("oracle_div_gt_50bps", ",".join(btc["flags"]))

            nvda = next(r for r in res["pairs"] if r["canonical_base"] == "NVDA")
            self.assertTrue(nvda["passes_hard_gates"])
            self.assertEqual(nvda["session_regime"], "equity_outside_rth")

    def test_train_fixed_static_direction_and_all_episode_slippage(self) -> None:
        from dataclasses import replace
        from eval_multiday_carry_universe import (
            HOUR_MS,
            align_pair_rates,
            compute_chronological_split,
        )

        with tempfile.TemporaryDirectory() as tmp:
            liq_dir = _build_synthetic_liquidity_archive(Path(tmp))
            m, pairs, cs, am, hc, ab, hb = load_verified_liquidity_archive(liq_dir)
            aligned = [
                align_pair_rates(
                    sp,
                    cs[(sp.leg_a_venue, sp.leg_a_symbol)],
                    cs[(sp.leg_b_venue, sp.leg_b_symbol)],
                )
                for sp in pairs
            ]
            split = compute_chronological_split(aligned)
            # Flip BTC Holdout carry slightly negative (-0.20 bps/hr vs rt_mt=12 bps): Train-fixed direction is negative,
            # causal L24/L72 stay below hurdle (0 episodes), AND only Holdout-peeking static would look positive.
            btc_pair = next(sp for sp in pairs if sp.canonical_base == "BTC")
            hl_key = (btc_pair.leg_b_venue, btc_pair.leg_b_symbol)
            orig_hl = cs[hl_key]
            ho_hours = sorted(
                h for h in orig_hl.rates_by_hour_ms if split["holdout_start_ms"] <= h <= split["holdout_end_ms"]
            )
            mid_gap_h = ho_hours[len(ho_hours) // 2]
            new_rates = {
                h: (Decimal("0.00000") if h >= split["holdout_start_ms"] else rate)
                for h, rate in orig_hl.rates_by_hour_ms.items()
                if h != mid_gap_h
            }
            cs[hl_key] = replace(
                orig_hl,
                rates_by_hour_ms=new_rates,
                raw_row_count=len(new_rates),
                min_hour_ms=min(new_rates),
                max_hour_ms=max(new_rates),
            )
            # Keep NVDA train direction positive (+0.00024) but drop one mid-Holdout hour to force 2 static episodes!
            nvda_pair = next(sp for sp in pairs if sp.canonical_base == "NVDA")
            nvda_key = (nvda_pair.leg_b_venue, nvda_pair.leg_b_symbol)
            nvda_hl = cs[nvda_key]
            nvda_rates = {h: r for h, r in nvda_hl.rates_by_hour_ms.items() if h != mid_gap_h}
            cs[nvda_key] = replace(
                nvda_hl,
                rates_by_hour_ms=nvda_rates,
                raw_row_count=len(nvda_rates),
                min_hour_ms=min(nvda_rates),
                max_hour_ms=max(nvda_rates),
            )

            res = evaluate_carry_liquidity_offline(
                universe_pairs=pairs,
                carry_series=cs,
                arcus_markets=am,
                hl_ctxs=hc,
                arcus_bbos=ab,
                hl_books=hb,
                metadata={"fetched_at_utc": m["fetched_at_utc"], "receipt_count": m["receipt_count"]},
            )
            btc = next(r for r in res["pairs"] if r["canonical_base"] == "BTC")
            self.assertEqual(btc["train_static_dir"], "long_a_short_b")
            self.assertEqual(btc["holdout_train_fixed_static_episodes"], 2)
            # Reversed sign on Holdout must remain negative under Train-frozen static direction (no Holdout sign peek!)
            self.assertLess(btc["holdout_train_fixed_static_net_rt_mt_bps"], 0.0)
            self.assertLess(btc["holdout_train_fixed_static_after_pt_mt_100_bps"], 0.0)
            self.assertFalse(btc["passes_train_fixed_static_only"])
            self.assertFalse(btc["passes_hard_gates"])

            nvda = next(r for r in res["pairs"] if r["canonical_base"] == "NVDA")
            self.assertEqual(nvda["train_static_dir"], "long_a_short_b")
            self.assertEqual(nvda["holdout_train_fixed_static_episodes"], 2)
            # Verify 2-episode slippage (MT) & 2-episode spread+slippage (TT) charged cleanly
            slip = Decimal(str(nvda["hl_rt_slip_100_bps"]))
            spr = Decimal(str(nvda["arcus_spread_bps"]))
            mt_raw = Decimal(str(nvda["holdout_train_fixed_static_net_rt_mt_bps"]))
            mt_after = Decimal(str(nvda["holdout_train_fixed_static_after_pt_mt_100_bps"]))
            tt_after = Decimal(str(nvda["holdout_train_fixed_static_after_pt_tt_100_bps"]))
            self.assertAlmostEqual(float(mt_raw - mt_after), float(Decimal("2") * slip), places=2)
            self.assertGreater(float(mt_after - tt_after), float(Decimal("1.5") * spr))
            _ = HOUR_MS

    def test_tamper_detection_and_offline_determinism_and_ast_guard(self) -> None:
        with _ForbiddenEnvGuard():
            with (
                patch("eval_multiday_carry_universe._http_fetch_bytes", side_effect=AssertionError("network forbidden")),
                patch("audit_carry_liquidity_spreads._http_fetch_bytes", side_effect=AssertionError("network forbidden")),
            ):
                c_st, out_st, err_st = _run_cli(["--self-test"])
                self.assertEqual(c_st, 0, msg=err_st)
                st = json.loads(out_st)
                self.assertEqual(st["self_test"], "passed")
                self.assertTrue(st["zero_secret_env_reads_verified"])

                with tempfile.TemporaryDirectory() as tmp:
                    liq_dir = _build_synthetic_liquidity_archive(Path(tmp))
                    c1, out1, err1 = _run_cli(["--input-dir", str(liq_dir)])
                    c2, out2, err2 = _run_cli(["--input-dir", str(liq_dir)])
                    self.assertEqual(c1, 0, msg=err1)
                    self.assertEqual(c2, 0, msg=err2)
                    self.assertEqual(out1, out2)

                    # Tamper raw liquidity payload -> ValueError
                    man = json.loads((liq_dir / "manifest.json").read_text(encoding="utf-8"))
                    target = liq_dir / man["receipts"][0]["rel_path"]
                    orig = target.read_bytes()
                    target.write_bytes(b" " + orig[1:])
                    with self.assertRaises(ValueError):
                        load_verified_liquidity_archive(liq_dir)

        src = (REPO_ROOT / "scripts" / "audit_carry_liquidity_spreads.py").read_text(encoding="utf-8")
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
        self.assertEqual(env_funcs, set())

    def test_strict_positive_int_rejects_bool_zero_negative_and_float(self) -> None:
        for bad in (True, False, 0, -1, -100, 100.5, "100", None):
            self.assertIsNone(_strict_positive_int(bad))
        self.assertEqual(_strict_positive_int(1), 1)
        self.assertEqual(_strict_positive_int(1_795_392_000_000), 1_795_392_000_000)

        bad_arc = parse_arcus_bbo(
            "TEST-USD",
            json.dumps({
                "bestBid": {"price": "99.98", "size": "10"},
                "bestAsk": {"price": "100.02", "size": "10"},
                "timestamp": True,
            }).encode("utf-8"),
        )
        self.assertIsNone(bad_arc.timestamp_us)

        bad_hl = parse_hl_l2_book(
            "hyperliquid",
            "TEST",
            json.dumps({
                "coin": "TEST",
                "time": False,
                "levels": [[{"px": "99.99", "sz": "10", "n": 1}], [{"px": "100.01", "sz": "10", "n": 1}]],
            }).encode("utf-8"),
        )
        self.assertIsNone(bad_hl.timestamp_ms)

    def test_clock_skew_future_stale_bool_and_tradability_gates_fail_closed(self) -> None:
        rec_iso = "2026-11-23T00:00:00+00:00"
        older_rec_iso = "2026-11-22T23:59:20+00:00"
        rec_us = _parse_iso_utc_epoch_us(rec_iso)
        older_rec_us = _parse_iso_utc_epoch_us(older_rec_iso)

        def _rewrite_and_eval(
            mutate_fn: Any,
        ) -> dict[str, Any]:
            with tempfile.TemporaryDirectory() as tmp:
                root = _build_synthetic_liquidity_archive(Path(tmp))
                manifest = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
                nvda_docs: dict[str, tuple[dict[str, Any], dict[str, Any]]] = {}
                meta_docs: dict[str, tuple[dict[str, Any] | list[Any], dict[str, Any]]] = {}
                all_pairs: list[tuple[dict[str, Any] | list[Any], dict[str, Any]]] = []
                for rc in manifest["receipts"]:
                    p = root / rc["rel_path"]
                    obj = json.loads(p.read_text(encoding="utf-8"))
                    all_pairs.append((obj, rc))
                    if rc["kind"] in {"arcus_markets", "hl_meta"}:
                        meta_docs[f"{rc['kind']}:{rc['venue']}"] = (obj, rc)
                    if rc["symbol"] in {"NVDA-USD", "xyz:NVDA"}:
                        nvda_docs[rc["kind"]] = (obj, rc)
                mutate_fn(nvda_docs, meta_docs)
                for obj, rc in all_pairs:
                    raw = (json.dumps(obj, sort_keys=True) + "\n").encode("utf-8")
                    (root / rc["rel_path"]).write_bytes(raw)
                    rc["bytes"] = len(raw)
                    rc["sha256"] = _sha256_bytes(raw)
                (root / "manifest.json").write_text(json.dumps(manifest, sort_keys=True) + "\n", encoding="utf-8")
                res = _evaluate_and_optional_write(root, None)
                self.assertIsNone(res["historical_40d_realized_cash_usd"])
                return next(r for r in res["pairs"] if r["canonical_base"] == "NVDA")

        # 1. Baseline passes
        ok_row = _rewrite_and_eval(lambda d, m: None)
        self.assertTrue(ok_row["joint_clock_ok"])
        self.assertTrue(ok_row["depth_100usd_ok"])
        self.assertTrue(ok_row["passes_hard_gates"])
        self.assertIsNotNone(ok_row["cross_mid_basis_bps"])
        self.assertIsNotNone(ok_row["payback_hours_mt_100"])

        # 2. Boundary: exact 1000 ms passes vs 1001 ms fails cleanly
        exact_1000 = _rewrite_and_eval(lambda d, m: d["arcus_bbo"][0].__setitem__("timestamp", rec_us - 1_000_000))
        self.assertTrue(exact_1000["arcus_clock_ok"])
        self.assertTrue(exact_1000["joint_clock_ok"])
        self.assertTrue(exact_1000["passes_hard_gates"])

        stale_1001 = _rewrite_and_eval(lambda d, m: d["arcus_bbo"][0].__setitem__("timestamp", rec_us - 1_001_000))
        self.assertFalse(stale_1001["arcus_clock_ok"])
        self.assertFalse(stale_1001["joint_clock_ok"])
        self.assertFalse(stale_1001["depth_100usd_ok"])
        self.assertFalse(stale_1001["passes_hard_gates"])
        self.assertTrue(stale_1001["hl_l2_100usd_ok"])
        self.assertIsNone(stale_1001["cross_mid_basis_bps"])
        self.assertIsNone(stale_1001["friction_mt_plus_hl_cross_100_bps"])
        self.assertIsNone(stale_1001["holdout_causal_L72_M1p5_after_pt_mt_100_bps"])
        self.assertIsNotNone(stale_1001["holdout_causal_L72_M1p5_net_bps"])

        # 3. All 5 bad clock variants from peer probe fail closed & withhold joint values
        clock_bad_cases = [
            ("arcus_24h_old", lambda d, m: d["arcus_bbo"][0].__setitem__("timestamp", rec_us - 86_400_000_000)),
            ("hl_24h_future", lambda d, m: d["hl_l2"][0].__setitem__("time", (rec_us // 1_000) + 86_400_000)),
            ("arcus_missing_stamp", lambda d, m: d["arcus_bbo"][0].pop("timestamp")),
            ("hl_bool_stamp", lambda d, m: d["hl_l2"][0].__setitem__("time", True)),
            (
                "locally_fresh_40s_apart",
                lambda d, m: (
                    d["arcus_bbo"][0].__setitem__("timestamp", older_rec_us - 1_000),
                    d["arcus_bbo"][1].__setitem__("received_at_utc", older_rec_iso),
                ),
            ),
        ]
        for label, fn in clock_bad_cases:
            row = _rewrite_and_eval(fn)
            self.assertFalse(row["joint_clock_ok"], msg=label)
            self.assertFalse(row["depth_100usd_ok"], msg=label)
            self.assertFalse(row["passes_hard_gates"], msg=label)
            self.assertIsNone(row["cross_mid_basis_bps"], msg=label)
            self.assertIsNone(row["payback_hours_mt_100"], msg=label)

        # 4. Dual-leg tradability gates: Arcus OFFLINE & TradeXYZ isDelisted
        arc_offline = _rewrite_and_eval(
            lambda d, m: [
                row.__setitem__("status", "OFFLINE")
                for row in m["arcus_markets:arcus"][0]["markets"]
                if row["marketDisplayName"] == "NVDA-USD"
            ]
        )
        self.assertFalse(arc_offline["arcus_tradable_ok"])
        self.assertFalse(arc_offline["passes_hard_gates"])
        self.assertIn("arcus_market_not_online", arc_offline["flags"])
        self.assertIsNone(arc_offline["cross_mid_basis_bps"])

        xyz_delisted = _rewrite_and_eval(
            lambda d, m: [
                row.__setitem__("isDelisted", True)
                for row in m["hl_meta:trade_xyz"][0][0]["universe"]
                if row["name"] == "xyz:NVDA"
            ]
        )
        self.assertFalse(xyz_delisted["hl_tradable_ok"])
        self.assertFalse(xyz_delisted["passes_hard_gates"])
        self.assertIn("hl_market_delisted_or_missing", xyz_delisted["flags"])
        self.assertIsNone(xyz_delisted["cross_mid_basis_bps"])

        # 5. Naive or missing received_at_utc fails manifest load
        with tempfile.TemporaryDirectory() as tmp:
            root = _build_synthetic_liquidity_archive(Path(tmp))
            man = json.loads((root / "manifest.json").read_text(encoding="utf-8"))
            man["receipts"][0]["received_at_utc"] = "2026-11-23T00:00:00"
            (root / "manifest.json").write_text(json.dumps(man), encoding="utf-8")
            with self.assertRaises(ValueError):
                load_verified_liquidity_archive(root)

    def test_pair_synchronized_live_fetch_interleaves_by_pair_identity(self) -> None:
        from eval_multiday_carry_universe import _build_synthetic_archive, load_verified_offline_archive

        with tempfile.TemporaryDirectory() as tmp:
            tmp_path = Path(tmp)
            carry_dir = tmp_path / "carry"
            liq_dir = tmp_path / "liq"
            carry_dir.mkdir(parents=True, exist_ok=True)
            liq_dir.mkdir(parents=True, exist_ok=True)
            _build_synthetic_archive(carry_dir, num_hours=360)
            _, pairs, _ = load_verified_offline_archive(carry_dir)

            base_iso = "2026-11-23T00:00:00+00:00"
            base_us = _parse_iso_utc_epoch_us(base_iso)
            base_ms = base_us // 1_000
            call_log: list[tuple[str, dict[str, Any] | None]] = []

            def _fake_fetch(url: str, *, method: str = "GET", body: dict[str, Any] | None = None) -> tuple[int, bytes]:
                call_log.append((url, body))
                if url.endswith("/v1/markets"):
                    markets = [
                        {
                            "marketDisplayName": sp.leg_a_symbol,
                            "status": "ONLINE",
                            "oraclePrice": "100.0",
                            "fundingRate": "0.00001",
                            "volume24hNotional": "5000000",
                            "isOutsideRth": False,
                        }
                        for sp in pairs
                    ]
                    return 200, json.dumps({"markets": markets}).encode("utf-8")
                if body and body.get("type") == "metaAndAssetCtxs":
                    venue = "trade_xyz" if body.get("dex") == "xyz" else "hyperliquid"
                    subset = [sp for sp in pairs if sp.leg_b_venue == venue]
                    uni = [{"name": sp.leg_b_symbol, "isDelisted": False} for sp in subset]
                    ctxs = [{"oraclePx": "100.01", "funding": "0.00025", "dayNtlVlm": "8000000"} for _ in subset]
                    return 200, json.dumps([{"universe": uni}, ctxs]).encode("utf-8")
                if "/v1/bbo/" in url:
                    return 200, json.dumps({
                        "bestBid": {"price": "99.98", "size": "50"},
                        "bestAsk": {"price": "100.02", "size": "50"},
                        "timestamp": base_us - 5_000,
                    }).encode("utf-8")
                if body and body.get("type") == "l2Book":
                    return 200, json.dumps({
                        "coin": body["coin"],
                        "time": base_ms - 5,
                        "levels": [
                            [{"px": "99.99", "sz": "20", "n": 1}],
                            [{"px": "100.01", "sz": "20", "n": 1}],
                        ],
                    }).encode("utf-8")
                raise AssertionError(f"unexpected request {url} {body}")

            with (
                patch("audit_carry_liquidity_spreads._http_fetch_bytes", side_effect=_fake_fetch),
                patch("audit_carry_liquidity_spreads._utc_now_iso", return_value=base_iso),
            ):
                manifest = _fetch_live_liquidity_archive(
                    universe_pairs=pairs,
                    out_dir=liq_dir,
                    carry_archive_dir=carry_dir,
                    sleep_ms=0,
                )

            book_receipts = [r for r in manifest["receipts"] if r["kind"] in {"arcus_bbo", "hl_l2"}]
            self.assertEqual(len(book_receipts), len(pairs) * 2)
            for idx in range(0, len(book_receipts), 2):
                leg_a = book_receipts[idx]
                leg_b = book_receipts[idx + 1]
                self.assertEqual(leg_a["kind"], "arcus_bbo")
                self.assertEqual(leg_b["kind"], "hl_l2")
                self.assertEqual(leg_a["pair_id"], leg_b["pair_id"])

            res = _evaluate_and_optional_write(liq_dir, None)
            nvda = next(r for r in res["pairs"] if r["canonical_base"] == "NVDA")
            self.assertTrue(nvda["joint_clock_ok"])
            self.assertTrue(nvda["passes_hard_gates"])


if __name__ == "__main__":
    unittest.main()
