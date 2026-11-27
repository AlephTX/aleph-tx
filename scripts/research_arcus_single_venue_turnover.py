#!/usr/bin/env python3
"""Method 1 (Single-Venue Arcus RWA High-Frequency MM) Benchmark (`W01-W27`, `NOT -w28`).

Strictly stdlib-only module top level (`duckdb` imported lazily inside lakehouse functions ONLY).
Reuses P0-fixed simulator and accounting primitives from:
  - `research_track03_core4_hold_ablation.py` (`simulate_core4_arm`, `compute_ols_beta_regression`,
    `compute_regime_symmetry`, `compute_portfolio_max_gross_exposure`, `compute_quantiles`,
    `politis_romano_stationary_bootstrap_paired`, `EXPECTED_FEE_EVIDENCE_SHA256`, `sha256_file`)
  - `research_track03_full_universe_speedbump_mm.py` (`FROZEN_STAGE_B_PARAMS`, `VENUE_SYMBOL_MAP`,
    `SpeedbumpArmConfig`, `_infer_tick_from_prices`, `_window_sort_key`, `prepare_replay_stream`)
  - `research_track03_patient_profit_frontier.py` (`assert_no_w28_leak`)
  - `eval_volume_farming_cost.py` (`compute_volume_farming_cost`)
  - `audit_multiple_testing_overfitting.py` (`cscv_pbo`, `deflated_sharpe_report`,
    `probabilistic_sharpe_ratio`, `series_moments`)
  - `track_fee_evidence.py` (`VENUE_CAPABILITY_SNAPSHOT_VERSION`, `build_fee_provenance`,
    `load_track_fee_evidence`, `resolve_venue_fee`)
"""

from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
import gc
import glob
import hashlib
import json
import math
import os
import random
import re
import sys
from collections import defaultdict
from datetime import date, timedelta
from pathlib import Path
from typing import Any, Sequence

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from audit_multiple_testing_overfitting import (  # noqa: E402
    cscv_pbo,
    deflated_sharpe_report,
    probabilistic_sharpe_ratio,
    series_moments,
)
from eval_volume_farming_cost import compute_volume_farming_cost  # noqa: E402
from research_track03_core4_hold_ablation import (  # noqa: E402
    CORE_4_SYMBOLS,
    DEFAULT_PREREG_BLOCK_LEN,
    DEFAULT_PREREG_BOOTSTRAP_SEED,
    DEFAULT_PREREG_N_BOOT,
    EXPECTED_FEE_EVIDENCE_SHA256,
    MAX_CONTINUOUS_GAP_MS,
    TEN_HOURS_MS,
    compute_ols_beta_regression,
    compute_portfolio_max_gross_exposure,
    compute_quantiles,
    compute_regime_symmetry,
    politis_romano_stationary_bootstrap_paired,
    sha256_file,
    simulate_core4_arm,
)
from research_track03_full_universe_speedbump_mm import (  # noqa: E402
    FROZEN_STAGE_B_PARAMS,
    VENUE_SYMBOL_MAP,
    SpeedbumpArmConfig,
    _infer_tick_from_prices,
    _window_sort_key,
    prepare_replay_stream,
)
from research_track03_patient_profit_frontier import assert_no_w28_leak  # noqa: E402
from track_fee_evidence import (  # noqa: E402
    VENUE_CAPABILITY_SNAPSHOT_VERSION,
    build_fee_provenance,
    load_track_fee_evidence,
    resolve_venue_fee,
)

TARGET_VENUE = "arcus"
TARGET_SETTLEMENT = "USDC"
REF_VENUES: tuple[str, ...] = ("lighter_robinhood", "trade_xyz")

# All 18 continuous Arcus RWA markets in W01-W27 (confirmed on Aleph).
ARCUS_RWA_SYMBOLS: tuple[str, ...] = (
    "AAPL",
    "AMD",
    "AMZN",
    "COIN",
    "CRCL",
    "DRAM",
    "GOOGL",
    "HOOD",
    "META",
    "MSFT",
    "MSTR",
    "MU",
    "NVDA",
    "PLTR",
    "QQQ",
    "SNDK",
    "SPY",
    "TSLA",
)
ARCUS_RWA_MARKET_MAP: dict[str, str] = {sym: f"{sym}-USD" for sym in ARCUS_RWA_SYMBOLS}

# Exact reference market mappings (includes DRAM for trade_xyz).
REF_SYMBOL_MAP: dict[str, dict[str, str]] = {
    "lighter_robinhood": dict(VENUE_SYMBOL_MAP["lighter_robinhood"]),
    "trade_xyz": {**VENUE_SYMBOL_MAP["trade_xyz"], "DRAM": "xyz:DRAM"},
}

ARCUS_FEE_SCHEDULES: dict[str, dict[str, Any]] = {
    "verified": {
        "maker_fee_bps": 0.0,
        "taker_fee_bps": 2.1375,
        "account_fee_status": "verified_account_tier",
        "note": "Level 0 + 5% feeDiscountPpm=50000 verified on account",
    },
    "public_l0": {
        "maker_fee_bps": 0.0,
        "taker_fee_bps": 2.25,
        "account_fee_status": "public_tier0_schedule_only",
        "note": "Public Level 0 RWA fee schedule",
    },
}

_S4_BASE: dict[str, Any] = dict(FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"])

M1_ARM_SPECS: dict[str, dict[str, Any]] = {
    "S4_hold15s": {
        **_S4_BASE,
        "max_hold_ms": 15_000,
        "reduce_only_penny_ticks": 0,
        "inventory_skew_bps_per_full_limit": 2.0,
    },
    "S4_hold60s": {
        **_S4_BASE,
        "max_hold_ms": 60_000,
        "reduce_only_penny_ticks": 0,
        "inventory_skew_bps_per_full_limit": 2.0,
    },
    "S4_hold180s": {
        **_S4_BASE,
        "max_hold_ms": 180_000,
        "reduce_only_penny_ticks": 0,
        "inventory_skew_bps_per_full_limit": 2.0,
    },
    "S4_hold600s_only": {
        **_S4_BASE,
        "max_hold_ms": 600_000,
        "reduce_only_penny_ticks": 0,
        "inventory_skew_bps_per_full_limit": 2.0,
    },
    "W5": {
        **_S4_BASE,
        "max_hold_ms": 180_000,
        "min_entry_edge_bps": 1.50,
        "latency_vol_spread_mult": 2.50,
        "vol_spread_mult": 1.00,
        "reduce_only_penny_ticks": 0,
        "inventory_skew_bps_per_full_limit": 4.0,
    },
}

CORE_LATENCY_TRIPLES: tuple[tuple[int, int, int], ...] = (
    (50, 50, 100),
    (100, 150, 150),
    (200, 300, 330),
)

# Full marginal cadence grid covering submit_rtt_ms={50,100,200}, cancel_ms={50,150,300}, ioc_exit_delay_ms={100,150,330}
CADENCE_SWEEP_TRIPLES: tuple[tuple[int, int, int], ...] = (
    (50, 50, 100),
    (100, 150, 150),
    (200, 300, 330),
    (50, 150, 150),
    (200, 150, 150),
    (100, 50, 150),
    (100, 300, 150),
    (100, 150, 100),
    (100, 150, 330),
)

PRIMARY_LATENCY_TRIPLE: tuple[int, int, int] = (100, 150, 150)
CADENCE_SWEEP_ARMS: tuple[str, ...] = ("S4_hold600s_only",)
CLIP_SIZES_USD: tuple[float, ...] = (100.0, 200.0, 500.0)

TOXIC_STRESS_SCENARIOS: tuple[tuple[str, str, float], ...] = (
    ("baseline_prints_only", "strict_through", 0.0),
    ("strict_through_25", "strict_through", 0.25),
    ("strict_through_50", "strict_through", 0.50),
    ("strict_through_100", "strict_through", 1.00),
    ("opposite_touch_25", "opposite_touch", 0.25),
    ("opposite_touch_50", "opposite_touch", 0.50),
)

_CAMPAIGN_RE = re.compile(r"^tokyo-rwa-.+-w(\d+)$")


@dataclasses.dataclass(frozen=True)
class SimSpec:
    """Single simulation specification executed over each `(win, symbol)` stream."""

    spec_id: str
    section: str
    arm_name: str
    order_delay_ms: int
    cancel_delay_ms: int
    exit_delay_ms: int
    quote_usd: float
    max_pos_usd: float
    book_cross_mode: str = "strict_through"
    book_cross_toxic_fill_frac: float = 0.0
    scenario_id: str = "baseline_prints_only"

    @property
    def latency_label(self) -> str:
        return f"o{self.order_delay_ms}_c{self.cancel_delay_ms}_e{self.exit_delay_ms}"


def build_arm_config(
    arm_name: str,
    *,
    order_delay_ms: int,
    cancel_delay_ms: int,
    exit_delay_ms: int,
    quote_usd: float = 200.0,
    max_pos_usd: float | None = None,
    maker_fee_bps: float = 0.0,
    taker_fee_bps: float = 2.1375,
    rebate_bps: float = 0.0,
    book_cross_mode: str = "strict_through",
    book_cross_toxic_fill_frac: float = 0.0,
) -> SpeedbumpArmConfig:
    """Build a SpeedbumpArmConfig for Method 1 enforcing strictly positive delays."""
    if arm_name not in M1_ARM_SPECS:
        raise ValueError(f"Unknown M1 arm {arm_name!r}; expected one of {sorted(M1_ARM_SPECS)}")
    if order_delay_ms <= 0 or cancel_delay_ms <= 0 or exit_delay_ms <= 0:
        raise ValueError(
            f"All latency delays must be > 0 ms (never 0 ms), got {(order_delay_ms, cancel_delay_ms, exit_delay_ms)}"
        )
    spec = dict(M1_ARM_SPECS[arm_name])
    use_sb_cancel = bool(spec.pop("use_speed_bump_cancel_ms", False))
    if use_sb_cancel:
        spec["speed_bump_cancel_ms"] = float(cancel_delay_ms)
    eff_max_pos = float(max_pos_usd) if max_pos_usd is not None else (2.0 * float(quote_usd))
    return SpeedbumpArmConfig(
        name=arm_name,
        order_delay_ms=int(order_delay_ms),
        cancel_delay_ms=int(cancel_delay_ms),
        exit_delay_ms=int(exit_delay_ms),
        quote_usd=float(quote_usd),
        max_pos_usd=eff_max_pos,
        maker_fee_bps=float(maker_fee_bps),
        taker_fee_bps=float(taker_fee_bps),
        rebate_bps=float(rebate_bps),
        book_cross_mode=book_cross_mode,
        book_cross_toxic_fill_frac=float(book_cross_toxic_fill_frac),
        **spec,
    )


def build_benchmark_specs(*, compact: bool = False) -> list[SimSpec]:
    """Build the deduplicated M1 benchmark matrix across arms, cadences, clips, and toxic stress."""
    specs: dict[tuple[Any, ...], SimSpec] = {}

    def _add(
        section: str,
        arm: str,
        triple: tuple[int, int, int],
        clip: float,
        sc_id: str,
        mode: str,
        frac: float,
    ) -> None:
        key = (arm, triple, clip, mode, frac)
        if key in specs:
            return
        lat = f"o{triple[0]}_c{triple[1]}_e{triple[2]}"
        sid = f"{arm}|{lat}|q{int(clip)}|{sc_id}"
        specs[key] = SimSpec(
            spec_id=sid,
            section=section,
            arm_name=arm,
            order_delay_ms=triple[0],
            cancel_delay_ms=triple[1],
            exit_delay_ms=triple[2],
            quote_usd=clip,
            max_pos_usd=2.0 * clip,
            book_cross_mode=mode,
            book_cross_toxic_fill_frac=frac,
            scenario_id=sc_id,
        )

    # 1. Core 5 arms x 3 core cadences ($200 clip, prints-only baseline)
    for arm in M1_ARM_SPECS:
        for triple in CORE_LATENCY_TRIPLES:
            _add("arm_cadence", arm, triple, 200.0, "baseline_prints_only", "strict_through", 0.0)

    if not compact:
        # 2. Marginal cadence sweeps across submit/cancel/exit delays on S4_hold600s_only
        for arm in CADENCE_SWEEP_ARMS:
            for triple in CADENCE_SWEEP_TRIPLES:
                _add("cadence_sweep", arm, triple, 200.0, "baseline_prints_only", "strict_through", 0.0)

        # 3. Clip size sensitivity ($100 / $200 / $500) at Arcus primary cadence
        for arm in ("S4_hold15s", "S4_hold600s_only", "W5"):
            for clip in CLIP_SIZES_USD:
                _add("clip_size", arm, PRIMARY_LATENCY_TRIPLE, clip, "baseline_prints_only", "strict_through", 0.0)

        # 4. Mandatory unprinted toxic-fill stress at primary cadence across core arms
        for arm in ("S4_hold15s", "S4_hold600s_only", "W5"):
            for sc_id, mode, frac in TOXIC_STRESS_SCENARIOS:
                _add("toxic_stress", arm, PRIMARY_LATENCY_TRIPLE, 200.0, sc_id, mode, frac)
        for arm in ("S4_hold60s", "S4_hold180s"):
            for sc_id, mode, frac in (
                ("strict_through_50", "strict_through", 0.50),
                ("opposite_touch_25", "opposite_touch", 0.25),
            ):
                _add("toxic_stress", arm, PRIMARY_LATENCY_TRIPLE, 200.0, sc_id, mode, frac)
    else:
        for arm in ("S4_hold15s", "S4_hold600s_only"):
            for clip in CLIP_SIZES_USD:
                _add("clip_size", arm, PRIMARY_LATENCY_TRIPLE, clip, "baseline_prints_only", "strict_through", 0.0)
            for sc_id, mode, frac in TOXIC_STRESS_SCENARIOS:
                _add("toxic_stress", arm, PRIMARY_LATENCY_TRIPLE, 200.0, sc_id, mode, frac)

    return list(specs.values())


def resolve_arcus_fee_provenance(fee_evidence_path: Path | None) -> tuple[float, float, str, str, dict[str, Any]]:
    """Load OPT-01 fee provenance and verify Arcus RWA 0.0 / 2.1375 bps + L0 0.0 / 2.25 bps."""
    if fee_evidence_path is not None and not fee_evidence_path.is_file():
        raise FileNotFoundError(f"Missing fee evidence file: {fee_evidence_path}")
    ev = load_track_fee_evidence(fee_evidence_path)
    mk, tk, st, src = resolve_venue_fee(
        TARGET_VENUE,
        "rwa",
        default_maker_bps=0.0,
        default_taker_bps=2.1375,
        default_status="verified_account_tier" if ev.get("loaded") else "public_tier0_schedule_only",
        evidence=ev,
    )
    prov = build_fee_provenance(
        ev,
        arcus_fee_tier="level_0",
        venues_in_scope=(TARGET_VENUE, "lighter_robinhood", "trade_xyz"),
    )
    if fee_evidence_path is not None:
        digest = sha256_file(fee_evidence_path)
        prov["fee_evidence_sha256"] = digest
        prov["expected_fee_evidence_sha256"] = EXPECTED_FEE_EVIDENCE_SHA256
        prov["fee_evidence_sha256_matches"] = digest == EXPECTED_FEE_EVIDENCE_SHA256
    prov["verified_rwa_maker_bps"] = float(mk)
    prov["verified_rwa_taker_bps"] = float(tk)
    prov["verified_account_fee_status"] = st
    prov["verified_fee_source"] = src
    prov["public_l0_rwa_maker_bps"] = 0.0
    prov["public_l0_rwa_taker_bps"] = 2.25
    prov["zero_points_rebate_bps"] = 0.0
    return float(mk), float(tk), st, src, prov


def compute_dual_volume_costs(
    *,
    realized_gross_usd: float,
    mtm_gross_usd: float,
    ntl: float,
    maker_volume_usd: float,
    taker_volume_usd: float,
    maker_fee_bps: float,
    taker_fee_bps: float,
    rebate_bps: float = 0.0,
) -> dict[str, Any]:
    """Compute exact cash PnL and $/M cost floors under both single-sided and double-sided Arcus denominators."""
    eff_maker_bp = float(maker_fee_bps) - float(rebate_bps)
    fee_usd = maker_volume_usd * (eff_maker_bp * 1e-4) + taker_volume_usd * (float(taker_fee_bps) * 1e-4)
    realized_net_usd = realized_gross_usd - fee_usd
    mtm_net_usd = mtm_gross_usd - fee_usd

    exec_ds_notl = maker_volume_usd + taker_volume_usd
    exec_ss_notl = 0.5 * exec_ds_notl
    s_share = min(1.0, max(0.0, taker_volume_usd / ntl)) if ntl > 0.0 else 0.0

    # Double-sided maker+exit executed volume ($/M): positive = cost, negative = profit
    ds_exec_mtm = round((-mtm_net_usd / exec_ds_notl) * 1e6, 4) if exec_ds_notl > 0.0 else None
    ds_exec_real = round((-realized_net_usd / exec_ds_notl) * 1e6, 4) if exec_ds_notl > 0.0 else None
    # Single-sided Arcus turnover volume ($/M per 0.5 * exec_ds_notl)
    ss_exec_mtm = round((-mtm_net_usd / exec_ss_notl) * 1e6, 4) if exec_ss_notl > 0.0 else None
    ss_exec_real = round((-realized_net_usd / exec_ss_notl) * 1e6, 4) if exec_ss_notl > 0.0 else None
    # Single-sided closed round-trip volume ($/M per ntl)
    ss_cycle_mtm = round((-mtm_net_usd / ntl) * 1e6, 4) if ntl > 0.0 else None

    ds_cycle_mtm: float | None = None
    ds_cycle_real: float | None = None
    maker_only_usd_per_m: float | None = None
    if ntl > 0.0:
        mtm_gbp = (mtm_gross_usd / ntl) * 1e4
        real_gbp = (realized_gross_usd / ntl) * 1e4
        mtm_bd = compute_volume_farming_cost(
            mtm_gbp, s_share, taker_fee_bps, maker_fee_bps=maker_fee_bps, rebate_bps=rebate_bps
        )
        real_bd = compute_volume_farming_cost(
            real_gbp, s_share, taker_fee_bps, maker_fee_bps=maker_fee_bps, rebate_bps=rebate_bps
        )
        ds_cycle_mtm = mtm_bd.double_sided_usd_per_m
        ds_cycle_real = real_bd.double_sided_usd_per_m
        maker_only_usd_per_m = mtm_bd.maker_volume_usd_per_m

    fee_drag_ds = round((fee_usd / exec_ds_notl) * 1e6, 4) if exec_ds_notl > 0.0 else None
    fee_drag_ss = round((fee_usd / exec_ss_notl) * 1e6, 4) if exec_ss_notl > 0.0 else None
    gross_alpha_ds = round((-mtm_gross_usd / exec_ds_notl) * 1e6, 4) if exec_ds_notl > 0.0 else None
    gross_alpha_ss = round((-mtm_gross_usd / exec_ss_notl) * 1e6, 4) if exec_ss_notl > 0.0 else None

    return {
        "maker_fee_bps": float(maker_fee_bps),
        "taker_fee_bps": float(taker_fee_bps),
        "fee_usd": round(fee_usd, 6),
        "realized_net_usd": round(realized_net_usd, 6),
        "mtm_net_usd": round(mtm_net_usd, 6),
        "double_sided_exec_mtm_usd_per_m": ds_exec_mtm,
        "double_sided_exec_realized_usd_per_m": ds_exec_real,
        "single_sided_exec_mtm_usd_per_m": ss_exec_mtm,
        "single_sided_exec_realized_usd_per_m": ss_exec_real,
        "single_sided_cycle_mtm_usd_per_m": ss_cycle_mtm,
        "double_sided_cycle_mtm_usd_per_m": ds_cycle_mtm,
        "double_sided_cycle_realized_usd_per_m": ds_cycle_real,
        "maker_volume_usd_per_m": maker_only_usd_per_m,
        "fee_drag_double_sided_usd_per_m": fee_drag_ds,
        "fee_drag_single_sided_usd_per_m": fee_drag_ss,
        "gross_cost_ex_fee_double_sided_usd_per_m": gross_alpha_ds,
        "gross_cost_ex_fee_single_sided_usd_per_m": gross_alpha_ss,
    }


def _campaign_parquet_files(
    lakehouse_dir: Path,
    dataset: str,
    venue: str,
    utc_date: str,
    campaign_id: str,
) -> list[str]:
    date_dirs = [utc_date]
    try:
        date_dirs.append((date.fromisoformat(utc_date) + timedelta(days=1)).isoformat())
    except ValueError:
        pass
    files: list[str] = []
    for d in date_dirs:
        base = (
            lakehouse_dir
            / "parquet"
            / dataset
            / "collector_region=tokyo"
            / f"venue={venue}"
            / f"utc_date={d}"
            / f"campaign_id={campaign_id}"
        )
        files.extend(glob.glob(str(base / "*.parquet")))
    return sorted(set(files))


def discover_arcus_rwa_campaigns(
    lakehouse_dir: Path,
    *,
    split: str = "all",
    window_ids: Sequence[str] | None = None,
) -> dict[str, dict[str, str]]:
    """Discover W01-W27 Arcus RWA campaigns strictly excluding `-w28` via DuckDB hive query."""
    import duckdb  # Lazy import so isolated `python3 -S` never requires duckdb

    root_glob = str(
        lakehouse_dir
        / "parquet"
        / "bbo_quotes"
        / "collector_region=tokyo"
        / f"venue={TARGET_VENUE}"
        / "utc_date=*"
        / "campaign_id=tokyo-rwa-*"
        / "*.parquet"
    )
    con = duckdb.connect(":memory:")
    try:
        rows = con.execute(
            f"""
            SELECT campaign_id, MIN(CAST(utc_date AS VARCHAR)) AS first_utc_date
            FROM read_parquet('{root_glob}', hive_partitioning=true)
            WHERE NOT regexp_matches(campaign_id, '-w28$')
            GROUP BY campaign_id
            ORDER BY first_utc_date ASC, campaign_id ASC
            """
        ).fetchall()
    finally:
        con.close()

    def _keep(win_num: int) -> bool:
        if window_ids:
            return f"w{win_num:02d}" in set(window_ids)
        if split == "selection_dev":
            return 1 <= win_num <= 20
        if split == "holdout":
            return 21 <= win_num <= 25
        if split == "w26_w27_oos":
            return win_num in (26, 27)
        if split == "all":
            return 1 <= win_num <= 27
        raise ValueError(f"Unsupported split {split!r}")

    out_unsorted: dict[str, dict[str, str]] = {}
    for cid, utc_d in rows:
        cid_s = str(cid)
        if cid_s.endswith("-w28"):
            continue
        m = _CAMPAIGN_RE.match(cid_s)
        if not m:
            continue
        win_num = int(m.group(1))
        if win_num < 1 or win_num > 27 or not _keep(win_num):
            continue
        win_id = f"w{win_num:02d}"
        if win_id not in out_unsorted or str(utc_d) < out_unsorted[win_id]["utc_date"]:
            out_unsorted[win_id] = {"campaign_id": cid_s, "utc_date": str(utc_d)}

    return {w: out_unsorted[w] for w in sorted(out_unsorted.keys(), key=_window_sort_key)}


def _build_bbo_stats_sql(f_sql: str, m_sql: str) -> str:
    return f"""
        WITH s AS (
            SELECT market,
                   available_at_ms,
                   available_at_ms - LAG(available_at_ms) OVER (PARTITION BY market ORDER BY available_at_ms) AS dt_ms,
                   (ask_px_f64 - bid_px_f64) / (0.5 * (bid_px_f64 + ask_px_f64)) * 1e4 AS sp_bps,
                   CAST(bid_sz AS DOUBLE) * bid_px_f64 AS bid_usd,
                   CAST(ask_sz AS DOUBLE) * ask_px_f64 AS ask_usd
            FROM read_parquet({f_sql}, hive_partitioning=true)
            WHERE NOT regexp_matches(campaign_id, '-w28$')
              AND market IN ({m_sql})
              AND tradable = true
              AND bid_px_f64 > 0
              AND ask_px_f64 > bid_px_f64
        )
        SELECT market,
               COUNT(*) AS cnt,
               MIN(available_at_ms) AS t0,
               MAX(available_at_ms) AS t1,
               COALESCE(MAX(dt_ms), 0) AS max_gap_ms,
               COALESCE(APPROX_QUANTILE(dt_ms, 0.50), 0) AS med_dt_ms,
               COALESCE(APPROX_QUANTILE(dt_ms, 0.95), 0) AS p95_dt_ms,
               COALESCE(SUM(CASE WHEN dt_ms > 5000 THEN 1 ELSE 0 END), 0) AS gaps_gt_5s,
               COALESCE(SUM(CASE WHEN dt_ms > 5000 THEN dt_ms - 5000 ELSE 0 END), 0) AS stale_excess_ms,
               COALESCE(APPROX_QUANTILE(sp_bps, 0.50), 0) AS med_sp_bps,
               COALESCE(APPROX_QUANTILE(bid_usd, 0.50), 0) AS med_bid_usd,
               COALESCE(APPROX_QUANTILE(ask_usd, 0.50), 0) AS med_ask_usd
        FROM s
        GROUP BY market
    """


def _build_bbo_bucket_sql(f_sql: str, m_sql: str, bucket_ms: int = 250) -> str:
    return f"""
        SELECT market,
               MIN(available_at_ms) AS ts_ms,
               arg_min(bid_px_f64, available_at_ms) AS bid_px,
               arg_min(CAST(bid_sz AS DOUBLE), available_at_ms) AS bid_sz,
               arg_min(ask_px_f64, available_at_ms) AS ask_px,
               arg_min(CAST(ask_sz AS DOUBLE), available_at_ms) AS ask_sz
        FROM read_parquet({f_sql}, hive_partitioning=true)
        WHERE NOT regexp_matches(campaign_id, '-w28$')
          AND market IN ({m_sql})
          AND tradable = true
          AND bid_px_f64 > 0
          AND ask_px_f64 > bid_px_f64
        GROUP BY market, (available_at_ms // {int(bucket_ms)})
        ORDER BY market ASC, ts_ms ASC
    """


def _build_trades_sql(f_sql: str, m_sql: str) -> str:
    return f"""
        SELECT market, available_at_ms, aggressor_side, price_f64, size_f64
        FROM read_parquet({f_sql}, hive_partitioning=true)
        WHERE NOT regexp_matches(campaign_id, '-w28$')
          AND market IN ({m_sql})
          AND NOT is_zero_price_transfer
          AND price_f64 > 0
          AND size_f64 > 0
        ORDER BY market ASC, available_at_ms ASC
    """


def _eval_arcus_window(
    lakehouse_dir: Path,
    win_id: str,
    c_info: dict[str, str],
    symbols: Sequence[str],
    specs: Sequence[SimSpec],
    *,
    rebate_bps: float = 0.0,
    verify_parity: bool = False,
) -> tuple[list[dict[str, Any]], dict[str, Any], float, float, float, int]:
    """Evaluate all M1 specs across one 10h RWA window on Arcus with LRH + TradeXYZ refs."""
    if str(c_info["campaign_id"]).endswith("-w28") or win_id == "w28":
        raise ValueError(f"Forbidden -w28 campaign {c_info['campaign_id']!r}")

    import duckdb  # Lazy import

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    def _load_bbo(venue: str, mkts: Sequence[str]) -> tuple[dict[str, list[tuple[int, float, float, float, float]]], dict[str, dict[str, Any]]]:
        files = _campaign_parquet_files(lakehouse_dir, "bbo_quotes", venue, c_info["utc_date"], c_info["campaign_id"])
        if not files or not mkts:
            return {}, {}
        f_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        m_sql = ", ".join(f"'{m}'" for m in sorted(set(mkts)))

        stat_rows = con.execute(_build_bbo_stats_sql(f_sql, m_sql)).fetchall()
        cont_map: dict[str, dict[str, Any]] = {}
        for mkt, cnt, t0, t1, mgap, med_dt, p95_dt, gt5s, stale_ms, med_sp, med_bu, med_au in stat_rows:
            if not cnt or cnt < 2:
                cont_map[str(mkt)] = {
                    "raw_events": int(cnt or 0),
                    "duration_ms": 0,
                    "hours": 0.0,
                    "max_gap_ms": 0,
                    "median_dt_ms": 0.0,
                    "p95_dt_ms": 0.0,
                    "gaps_gt_5s": 0,
                    "stale_time_share": 1.0,
                    "median_spread_bps": 0.0,
                    "median_bid_usd": 0.0,
                    "median_ask_usd": 0.0,
                    "is_continuous_10h": False,
                }
            else:
                dur = int(t1 - t0)
                cont_map[str(mkt)] = {
                    "raw_events": int(cnt),
                    "duration_ms": dur,
                    "hours": round(dur / 3_600_000.0, 3),
                    "max_gap_ms": int(mgap),
                    "median_dt_ms": round(float(med_dt), 2),
                    "p95_dt_ms": round(float(p95_dt), 2),
                    "gaps_gt_5s": int(gt5s),
                    "stale_time_share": round(float(stale_ms) / max(1.0, float(dur)), 6),
                    "median_spread_bps": round(float(med_sp), 4),
                    "median_bid_usd": round(float(med_bu), 2),
                    "median_ask_usd": round(float(med_au), 2),
                    "is_continuous_10h": dur >= TEN_HOURS_MS and int(mgap) <= MAX_CONTINUOUS_GAP_MS,
                }

        b_rows = con.execute(_build_bbo_bucket_sql(f_sql, m_sql, bucket_ms=250)).fetchall()
        by_mkt: dict[str, list[tuple[int, float, float, float, float]]] = defaultdict(list)
        for mkt, ts_ms, bb, bsz, ba, asz in b_rows:
            by_mkt[str(mkt)].append((int(ts_ms), float(bb), float(bsz), float(ba), float(asz)))
        return by_mkt, cont_map

    def _load_trades(mkts: Sequence[str]) -> dict[str, list[tuple[int, str, float, float]]]:
        files = _campaign_parquet_files(
            lakehouse_dir, "public_trades", TARGET_VENUE, c_info["utc_date"], c_info["campaign_id"]
        )
        if not files or not mkts:
            return {}
        f_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        m_sql = ", ".join(f"'{m}'" for m in sorted(set(mkts)))
        out: dict[str, list[tuple[int, str, float, float]]] = defaultdict(list)
        for mkt, ts_ms, agg_side, px, sz in con.execute(_build_trades_sql(f_sql, m_sql)).fetchall():
            out[str(mkt)].append((int(ts_ms), str(agg_side), float(px), float(sz)))
        return out

    tgt_mkts = [ARCUS_RWA_MARKET_MAP[s] for s in symbols]
    bbo_tgt, cont_tgt = _load_bbo(TARGET_VENUE, tgt_mkts)
    bbo_refs: dict[str, tuple[dict[str, list[tuple[int, float, float, float, float]]], dict[str, dict[str, Any]]]] = {}
    for rv in REF_VENUES:
        rv_mkts = [REF_SYMBOL_MAP[rv][s] for s in symbols if s in REF_SYMBOL_MAP[rv]]
        bbo_refs[rv] = _load_bbo(rv, rv_mkts)
    tr_map = _load_trades(tgt_mkts)
    con.close()

    rows_out: list[dict[str, Any]] = []
    market_stats: dict[str, Any] = {}
    max_gross_err = 0.0
    max_net_err = 0.0
    max_mtm_cash_err = 0.0
    non_flat_count = 0

    for sym in symbols:
        raw_mkt = ARCUS_RWA_MARKET_MAP[sym]
        b_rows = bbo_tgt.pop(raw_mkt, [])
        cont = cont_tgt.pop(
            raw_mkt,
            {
                "raw_events": 0,
                "duration_ms": 0,
                "hours": 0.0,
                "max_gap_ms": 0,
                "median_dt_ms": 0.0,
                "p95_dt_ms": 0.0,
                "gaps_gt_5s": 0,
                "stale_time_share": 1.0,
                "median_spread_bps": 0.0,
                "median_bid_usd": 0.0,
                "median_ask_usd": 0.0,
                "is_continuous_10h": False,
            },
        )
        sym_trades = tr_map.pop(raw_mkt, [])
        market_stats[f"{win_id}|{sym}"] = {
            **cont,
            "public_trades_count": len(sym_trades),
            "public_trades_notional_usd": round(sum(px * sz for _, _, px, sz in sym_trades), 2),
        }
        if len(b_rows) < 200:
            for sp in specs:
                rows_out.append(
                    {
                        "win": win_id,
                        "campaign_id": c_info["campaign_id"],
                        "symbol": sym,
                        "raw_market": raw_mkt,
                        "spec_id": sp.spec_id,
                        "section": sp.section,
                        "arm": sp.arm_name,
                        "latency_label": sp.latency_label,
                        "quote_usd": sp.quote_usd,
                        "max_pos_usd": sp.max_pos_usd,
                        "book_cross_mode": sp.book_cross_mode,
                        "book_cross_toxic_fill_frac": sp.book_cross_toxic_fill_frac,
                        "scenario_id": sp.scenario_id,
                        "cont": False,
                        "continuity": cont,
                        "skipped": f"too_few_books:{len(b_rows)}",
                    }
                )
            continue

        events: list[dict[str, Any]] = [
            {"ts": r[0], "kind": "book", "bb": r[1], "bsz": max(r[2], 1e-9), "ba": r[3], "asz": max(r[4], 1e-9)}
            for r in b_rows
        ]
        refs_used: list[str] = []
        for rv in REF_VENUES:
            if sym not in REF_SYMBOL_MAP[rv]:
                continue
            ref_raw = REF_SYMBOL_MAP[rv][sym]
            r_rows = bbo_refs[rv][0].pop(ref_raw, [])
            if len(r_rows) >= 200:
                refs_used.append(rv)
                for rr in r_rows:
                    events.append({"ts": rr[0], "kind": "ref", "venue": rv, "bid": rr[1], "ask": rr[3]})

        for ts_ms, agg_side, px, sz in sym_trades:
            s_up = agg_side.upper()
            side_str = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
            events.append({"ts": ts_ms, "kind": "trade", "side": side_str, "price": px, "size": sz})

        stream = prepare_replay_stream(events)
        tick = _infer_tick_from_prices([r[1] for r in b_rows])
        declared_end = int(b_rows[-1][0])
        del events
        del b_rows
        del sym_trades

        for sp in specs:
            arm_cfg = build_arm_config(
                sp.arm_name,
                order_delay_ms=sp.order_delay_ms,
                cancel_delay_ms=sp.cancel_delay_ms,
                exit_delay_ms=sp.exit_delay_ms,
                quote_usd=sp.quote_usd,
                max_pos_usd=sp.max_pos_usd,
                maker_fee_bps=0.0,
                taker_fee_bps=ARCUS_FEE_SCHEDULES["verified"]["taker_fee_bps"],
                rebate_bps=rebate_bps,
                book_cross_mode=sp.book_cross_mode,
                book_cross_toxic_fill_frac=sp.book_cross_toxic_fill_frac,
            )
            sim_res = simulate_core4_arm(stream, tick, arm_cfg, declared_end_ms=declared_end)
            if verify_parity:
                l0_cfg = dataclasses.replace(arm_cfg, taker_fee_bps=ARCUS_FEE_SCHEDULES["public_l0"]["taker_fee_bps"])
                l0_res = simulate_core4_arm(stream, tick, l0_cfg, declared_end_ms=declared_end)
                for k in ("maker_volume_usd", "taker_volume_usd", "ntl", "realized_gross_usd", "mtm_gross_usd"):
                    assert abs(float(sim_res[k]) - float(l0_res[k])) <= 1e-12

            ex_cash = float(sim_res.pop("_exact_cash_ex_fee_usd"))
            ex_real_gross = float(sim_res.pop("_exact_realized_gross_usd"))
            ex_mtm_gross = float(sim_res.pop("_exact_mtm_gross_usd"))
            ex_fee = float(sim_res.pop("_exact_fee_usd"))
            ex_net = float(sim_res.pop("_exact_net_usd"))
            max_mtm_cash_err = max(max_mtm_cash_err, abs(ex_mtm_gross - ex_cash))
            if sim_res["terminal_flat"]:
                max_gross_err = max(max_gross_err, abs(ex_real_gross - ex_cash))
            else:
                non_flat_count += 1
            max_net_err = max(max_net_err, abs(ex_net - (ex_mtm_gross - ex_fee)))

            l0_costs = compute_dual_volume_costs(
                realized_gross_usd=float(sim_res["gross_usd"]),
                mtm_gross_usd=float(sim_res["mtm_gross_usd"]),
                ntl=float(sim_res["ntl"]),
                maker_volume_usd=float(sim_res["maker_volume_usd"]),
                taker_volume_usd=float(sim_res["taker_volume_usd"]),
                maker_fee_bps=0.0,
                taker_fee_bps=ARCUS_FEE_SCHEDULES["public_l0"]["taker_fee_bps"],
                rebate_bps=rebate_bps,
            )
            ver_costs = compute_dual_volume_costs(
                realized_gross_usd=float(sim_res["gross_usd"]),
                mtm_gross_usd=float(sim_res["mtm_gross_usd"]),
                ntl=float(sim_res["ntl"]),
                maker_volume_usd=float(sim_res["maker_volume_usd"]),
                taker_volume_usd=float(sim_res["taker_volume_usd"]),
                maker_fee_bps=0.0,
                taker_fee_bps=ARCUS_FEE_SCHEDULES["verified"]["taker_fee_bps"],
                rebate_bps=rebate_bps,
            )

            # Only keep big timeline lists for primary arm_cadence & clip_size rows (keeps worker RSS tiny)
            hold_samples = sim_res.pop("_hold_samples_ms", [])
            exp_tl = sim_res.pop("_exposure_timeline", [])
            if sp.section in ("arm_cadence", "clip_size"):
                sim_res["_hold_samples_ms"] = hold_samples
                sim_res["_exposure_timeline"] = exp_tl

            rows_out.append(
                {
                    "win": win_id,
                    "campaign_id": c_info["campaign_id"],
                    "symbol": sym,
                    "raw_market": raw_mkt,
                    "spec_id": sp.spec_id,
                    "section": sp.section,
                    "arm": sp.arm_name,
                    "latency_label": sp.latency_label,
                    "order_delay_ms": sp.order_delay_ms,
                    "cancel_delay_ms": sp.cancel_delay_ms,
                    "exit_delay_ms": sp.exit_delay_ms,
                    "quote_usd": sp.quote_usd,
                    "max_pos_usd": sp.max_pos_usd,
                    "book_cross_mode": sp.book_cross_mode,
                    "book_cross_toxic_fill_frac": sp.book_cross_toxic_fill_frac,
                    "scenario_id": sp.scenario_id,
                    "tick_inferred": tick,
                    "declared_end_ms": declared_end,
                    "cont": bool(cont["is_continuous_10h"]),
                    "continuity": cont,
                    "refs_used": refs_used,
                    "verified_schedule": ver_costs,
                    "public_l0_schedule": l0_costs,
                    **sim_res,
                }
            )
        del stream

    gc.collect()
    return rows_out, market_stats, max_gross_err, max_net_err, max_mtm_cash_err, non_flat_count


def aggregate_arcus_bucket(grp: Sequence[dict[str, Any]], *, include_symbol: bool = False) -> dict[str, Any]:
    """Aggregate Arcus M1 rows with both verified (0/2.1375) and L0 public (0/2.25) dual-volume $/M."""
    first = grp[0]
    spr = sum(float(r["spr"]) for r in grp)
    drft = sum(float(r["drft"]) for r in grp)
    pex = sum(float(r["pex"]) for r in grp)
    tslp = sum(float(r["tslp"]) for r in grp)
    realized_gross_usd = spr + drft + pex + tslp
    unrealized_mtm_usd = sum(float(r.get("unrealized_mtm_usd", 0.0)) for r in grp)
    mtm_gross_usd = realized_gross_usd + unrealized_mtm_usd

    ntl = sum(float(r["ntl"]) for r in grp)
    maker_vol = sum(float(r["maker_volume_usd"]) for r in grp)
    taker_vol = sum(float(r["taker_volume_usd"]) for r in grp)
    exec_ds_notl = maker_vol + taker_vol
    exec_ss_notl = 0.5 * exec_ds_notl
    res_notl = sum(float(r.get("terminal_residual_notional_usd", 0.0)) for r in grp)
    depth_clipped = sum(int(r.get("depth_clipped_exits", 0)) for r in grp)
    stale_cancels = sum(int(r.get("stale_book_cancels", 0)) for r in grp)
    stale_blocks = sum(int(r.get("stale_exit_blocks", 0)) for r in grp)
    tox_pauses = sum(int(r.get("toxicity_pauses", 0)) for r in grp)
    ubx_fills = sum(int(r.get("unprinted_book_cross_fills", 0)) for r in grp)
    ubx_notl = sum(float(r.get("unprinted_book_cross_notional_usd", 0.0)) for r in grp)
    hold_wsum = sum(float(r.get("hold_weighted_sum_ms", 0.0)) for r in grp)
    max_hold_obs = max((int(r.get("max_hold_observed_ms", 0)) for r in grp), default=0)
    s_share = min(1.0, max(0.0, taker_vol / ntl)) if ntl > 0.0 else 0.0

    ver = compute_dual_volume_costs(
        realized_gross_usd=realized_gross_usd,
        mtm_gross_usd=mtm_gross_usd,
        ntl=ntl,
        maker_volume_usd=maker_vol,
        taker_volume_usd=taker_vol,
        maker_fee_bps=0.0,
        taker_fee_bps=ARCUS_FEE_SCHEDULES["verified"]["taker_fee_bps"],
    )
    pub = compute_dual_volume_costs(
        realized_gross_usd=realized_gross_usd,
        mtm_gross_usd=mtm_gross_usd,
        ntl=ntl,
        maker_volume_usd=maker_vol,
        taker_volume_usd=taker_vol,
        maker_fee_bps=0.0,
        taker_fee_bps=ARCUS_FEE_SCHEDULES["public_l0"]["taker_fee_bps"],
    )

    pooled_holds: list[int] = []
    for r in grp:
        hs = r.get("_hold_samples_ms")
        if hs:
            pooled_holds.extend(int(v) for v in hs)
    pooled_holds.sort()
    hold_q = compute_quantiles(pooled_holds, (0.50, 0.95, 0.99))
    hold_max_ms = int(pooled_holds[-1]) if pooled_holds else (max_hold_obs if ntl > 0.0 else None)

    by_win: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for r in grp:
        by_win[str(r["win"])].append(r)

    cum_net = 0.0
    peak_net = 0.0
    max_dd_usd = 0.0
    pos_wins_ver = 0
    pos_wins_l0 = 0
    zero_fill_wins = 0
    total_hours = 0.0
    win_net_ver_series: list[float] = []
    win_records: list[dict[str, Any]] = []
    worst_win: dict[str, Any] | None = None
    max_port_gross_exp = 0.0
    max_sym_gross_exp = max((float(r.get("max_gross_exposure_usd", 0.0)) for r in grp), default=0.0)

    for win_id in sorted(by_win.keys(), key=_window_sort_key):
        wgrp = by_win[win_id]
        w_mtm_gross = sum(float(x["mtm_gross_usd"]) for x in wgrp)
        w_real_gross = sum(float(x["gross_usd"]) for x in wgrp)
        w_ntl = sum(float(x["ntl"]) for x in wgrp)
        w_mvol = sum(float(x["maker_volume_usd"]) for x in wgrp)
        w_tvol = sum(float(x["taker_volume_usd"]) for x in wgrp)
        w_ds = w_mvol + w_tvol
        w_fills = sum(int(x.get("fills", 0)) for x in wgrp)
        w_res = sum(float(x.get("terminal_residual_notional_usd", 0.0)) for x in wgrp)
        w_ver = compute_dual_volume_costs(
            realized_gross_usd=w_real_gross,
            mtm_gross_usd=w_mtm_gross,
            ntl=w_ntl,
            maker_volume_usd=w_mvol,
            taker_volume_usd=w_tvol,
            maker_fee_bps=0.0,
            taker_fee_bps=ARCUS_FEE_SCHEDULES["verified"]["taker_fee_bps"],
        )
        w_l0 = compute_dual_volume_costs(
            realized_gross_usd=w_real_gross,
            mtm_gross_usd=w_mtm_gross,
            ntl=w_ntl,
            maker_volume_usd=w_mvol,
            taker_volume_usd=w_tvol,
            maker_fee_bps=0.0,
            taker_fee_bps=ARCUS_FEE_SCHEDULES["public_l0"]["taker_fee_bps"],
        )

        timelines = [x.get("_exposure_timeline") or [] for x in wgrp]
        if any(timelines):
            w_port_exp = compute_portfolio_max_gross_exposure(timelines)
        else:
            w_port_exp = sum(float(x.get("max_gross_exposure_usd", 0.0)) for x in wgrp)
        max_port_gross_exp = max(max_port_gross_exp, w_port_exp)

        if w_ds <= 0.0 or w_fills == 0:
            zero_fill_wins += 1
        total_hours += max(float((x.get("continuity") or {}).get("hours", 0.0)) for x in wgrp)
        w_net_v = float(w_ver["mtm_net_usd"])
        w_net_l0 = float(w_l0["mtm_net_usd"])
        win_net_ver_series.append(w_net_v)
        cum_net += w_net_v
        if cum_net > peak_net:
            peak_net = cum_net
        max_dd_usd = max(max_dd_usd, peak_net - cum_net)
        if w_net_v > 0.0:
            pos_wins_ver += 1
        if w_net_l0 > 0.0:
            pos_wins_l0 += 1

        w_rec = {
            "win": win_id,
            "fills": w_fills,
            "mtm_gross_usd": round(w_mtm_gross, 6),
            "mtm_net_usd": round(w_net_v, 6),
            "public_l0_mtm_net_usd": round(w_net_l0, 6),
            "ntl": round(w_ntl, 4),
            "maker_volume_usd": round(w_mvol, 4),
            "taker_volume_usd": round(w_tvol, 4),
            "double_sided_exec_volume_usd": round(w_ds, 4),
            "single_sided_exec_volume_usd": round(0.5 * w_ds, 4),
            "verified_double_sided_usd_per_m": w_ver["double_sided_exec_mtm_usd_per_m"],
            "verified_single_sided_usd_per_m": w_ver["single_sided_exec_mtm_usd_per_m"],
            "public_l0_double_sided_usd_per_m": w_l0["double_sided_exec_mtm_usd_per_m"],
            "public_l0_single_sided_usd_per_m": w_l0["single_sided_exec_mtm_usd_per_m"],
            "terminal_residual_notional_usd": round(w_res, 4),
            "portfolio_max_gross_exposure_usd": w_port_exp,
        }
        win_records.append(w_rec)
        if worst_win is None or w_net_v < worst_win["mtm_net_usd"]:
            worst_win = w_rec

    psr_zero: float | None = None
    if len(win_net_ver_series) >= 3:
        mom = series_moments(win_net_ver_series)
        psr_zero = round(probabilistic_sharpe_ratio(mom, 0.0), 6)

    def _agg_mk(bps_key: str, den_key: str) -> float | None:
        num = 0.0
        den = 0.0
        for r in grp:
            val = r.get(bps_key)
            d = float(r.get(den_key) or 0.0)
            if val is not None and d > 0.0:
                num += float(val) * d
                den += d
        return round(num / den, 6) if den > 0.0 else None

    cif_fills = sum(int(r.get("cancel_in_flight_fills", 0)) for r in grp)
    cif_notl = sum(float(r.get("cancel_in_flight_notional_usd", 0.0)) for r in grp)
    cif_share = (cif_notl / maker_vol) if maker_vol > 0.0 else 0.0
    n_wins = len(by_win)

    out: dict[str, Any] = {
        "spec_id": first["spec_id"],
        "section": first["section"],
        "arm": first["arm"],
        "latency_label": first["latency_label"],
        "order_delay_ms": first["order_delay_ms"],
        "cancel_delay_ms": first["cancel_delay_ms"],
        "exit_delay_ms": first["exit_delay_ms"],
        "quote_usd": first["quote_usd"],
        "max_pos_usd": first["max_pos_usd"],
        "scenario_id": first["scenario_id"],
        "book_cross_mode": first["book_cross_mode"],
        "book_cross_toxic_fill_frac": first["book_cross_toxic_fill_frac"],
        "windows_covered": n_wins,
        "zero_fill_windows": zero_fill_wins,
        "hours_covered": round(total_hours, 2),
        "rows": len(grp),
        "cont_rows": sum(1 for r in grp if r.get("cont")),
        "cyc": sum(int(r["cyc"]) for r in grp),
        "orders_placed": sum(int(r.get("orders_placed", 0)) for r in grp),
        "fills": sum(int(r["fills"]) for r in grp),
        "maker_fills": sum(int(r["maker_fills"]) for r in grp),
        "taker_exits": sum(int(r["taker_exits"]) for r in grp),
        "spr": round(spr, 6),
        "drft": round(drft, 6),
        "pex": round(pex, 6),
        "tslp": round(tslp, 6),
        "realized_gross_usd": round(realized_gross_usd, 6),
        "unrealized_mtm_usd": round(unrealized_mtm_usd, 6),
        "mtm_gross_usd": round(mtm_gross_usd, 6),
        "ntl": round(ntl, 4),
        "maker_volume_usd": round(maker_vol, 4),
        "taker_volume_usd": round(taker_vol, 4),
        "double_sided_exec_volume_usd": round(exec_ds_notl, 4),
        "single_sided_exec_volume_usd": round(exec_ss_notl, 4),
        "double_sided_exec_per_10h_usd": round(exec_ds_notl / n_wins, 2) if n_wins > 0 else 0.0,
        "single_sided_exec_per_10h_usd": round(exec_ss_notl / n_wins, 2) if n_wins > 0 else 0.0,
        "double_sided_exec_per_24h_usd": round((exec_ds_notl / n_wins) * 2.4, 2) if n_wins > 0 else 0.0,
        "single_sided_exec_per_24h_usd": round((exec_ss_notl / n_wins) * 2.4, 2) if n_wins > 0 else 0.0,
        "taker_exit_share_s": round(s_share, 6),
        "natural_passive_exit_share": round(1.0 - s_share, 6),
        "verified": ver,
        "public_l0": pub,
        "terminal_residual_notional_usd": round(res_notl, 4),
        "terminal_residual_per_window_usd": round(res_notl / n_wins, 4) if n_wins > 0 else 0.0,
        "max_gross_exposure_usd": round(max_port_gross_exp, 4),
        "max_symbol_gross_exposure_usd": round(max_sym_gross_exp, 4),
        "avg_hold_ms": round(hold_wsum / ntl, 2) if ntl > 0.0 else 0.0,
        "hold_p50_ms": hold_q["p50"],
        "hold_p95_ms": hold_q["p95"],
        "hold_p99_ms": hold_q["p99"],
        "hold_max_ms": hold_max_ms,
        "depth_clipped_exits": depth_clipped,
        "stale_book_cancels": stale_cancels,
        "stale_exit_blocks": stale_blocks,
        "toxicity_pauses": tox_pauses,
        "unprinted_book_cross_fills": ubx_fills,
        "unprinted_book_cross_notional_usd": round(ubx_notl, 4),
        "markout_1s_bps": _agg_mk("markout_1s_bps", "markout_1s_den"),
        "markout_5s_bps": _agg_mk("markout_5s_bps", "markout_5s_den"),
        "markout_10s_bps": _agg_mk("markout_10s_bps", "markout_10s_den"),
        "cancel_in_flight_fills": cif_fills,
        "cancel_in_flight_share": round(cif_share, 6),
        "positive_window_ratio_verified": round(pos_wins_ver / n_wins, 4) if n_wins > 0 else 0.0,
        "positive_window_ratio_public_l0": round(pos_wins_l0 / n_wins, 4) if n_wins > 0 else 0.0,
        "psr_zero_verified": psr_zero,
        "max_drawdown_usd_verified": round(max_dd_usd, 4),
        "worst_10h_window_verified": worst_win,
        "by_window": win_records,
    }
    if include_symbol:
        out["symbol"] = first["symbol"]
    return out


def stationary_bootstrap_single_spec(
    win_records: Sequence[dict[str, Any]],
    *,
    expected_block_len: float = DEFAULT_PREREG_BLOCK_LEN,
    n_boot: int = DEFAULT_PREREG_N_BOOT,
    seed: int = DEFAULT_PREREG_BOOTSTRAP_SEED,
) -> dict[str, Any]:
    """Circular stationary block bootstrap 95% CI on window net PnL and verified double/single-sided $/M."""
    t_len = len(win_records)
    if t_len < 3:
        return {"error": "need_at_least_3_windows"}
    nets = [float(w["mtm_net_usd"]) for w in win_records]
    ds_vols = [float(w["double_sided_exec_volume_usd"]) for w in win_records]
    rng = random.Random(seed)
    p_restart = 1.0 / float(expected_block_len)

    boot_mean_net: list[float] = []
    boot_ds_per_m: list[float] = []
    boot_ss_per_m: list[float] = []
    for _ in range(n_boot):
        idx = rng.randrange(t_len)
        s_net = 0.0
        s_ds = 0.0
        for step in range(t_len):
            if step > 0:
                idx = rng.randrange(t_len) if rng.random() < p_restart else (idx + 1) % t_len
            s_net += nets[idx]
            s_ds += ds_vols[idx]
        boot_mean_net.append(s_net / t_len)
        if s_ds > 0.0:
            ds_m = (-s_net / s_ds) * 1e6
            boot_ds_per_m.append(ds_m)
            boot_ss_per_m.append(2.0 * ds_m)

    def _summ(vals: Sequence[float]) -> dict[str, float] | None:
        if not vals:
            return None
        s = sorted(vals)
        m = len(s)
        return {
            "median": round(s[int(0.500 * (m - 1))], 4),
            "ci95_low": round(s[int(0.025 * (m - 1))], 4),
            "ci95_high": round(s[int(0.975 * (m - 1))], 4),
        }

    return {
        "windows": t_len,
        "expected_block_len": expected_block_len,
        "n_boot": n_boot,
        "seed": seed,
        "mean_mtm_net_usd_per_win": _summ(boot_mean_net),
        "verified_double_sided_usd_per_m": _summ(boot_ds_per_m),
        "verified_single_sided_usd_per_m": _summ(boot_ss_per_m),
    }


def _to_prereg_win_format(win_records: Sequence[dict[str, Any]]) -> list[dict[str, Any]]:
    return [
        {
            "win": w["win"],
            "mtm_net_usd": w["mtm_net_usd"],
            "net_cash_cost_usd": -float(w["mtm_net_usd"]),
            "target_executed_fill_notional_usd": w["double_sided_exec_volume_usd"],
            "ntl": w["ntl"],
        }
        for w in win_records
    ]


def compute_symmetry_and_beta(
    rows: Sequence[dict[str, Any]],
    *,
    symbols: Sequence[str],
    primary_spec_id: str,
    arm_spec_ids: Sequence[str],
) -> tuple[dict[str, Any], dict[str, Any]]:
    """Compute UP/DOWN window symmetry and OLS beta vs RWA return for Arcus M1."""
    active = [r for r in rows if not r.get("skipped") and r.get("symbol") in set(symbols)]

    # Underlying window returns from primary spec (same stream across specs)
    sym_win_ret: dict[tuple[str, str], float] = {}
    for r in active:
        if r["spec_id"] == primary_spec_id and r.get("underlying_return_bps") is not None:
            sym_win_ret[(str(r["win"]), str(r["symbol"]))] = float(r["underlying_return_bps"])
    win_ids = sorted({str(r["win"]) for r in active}, key=_window_sort_key)
    win_ew_ret: dict[str, float] = {}
    for w in win_ids:
        vals = [sym_win_ret[(w, s)] for s in symbols if (w, s) in sym_win_ret]
        if vals:
            win_ew_ret[w] = sum(vals) / len(vals)

    symmetry: dict[str, Any] = {"by_spec": {}, "primary_pooled_symbol_windows": {}}
    beta_reg: dict[str, Any] = {"by_spec": {}, "primary_pooled_symbol_windows": {}, "primary_per_symbol": {}}

    for sid in arm_spec_ids:
        up_list: list[dict[str, Any]] = []
        dn_list: list[dict[str, Any]] = []
        xs_pct: list[float] = []
        ys_usd: list[float] = []
        for w in win_ids:
            if w not in win_ew_ret:
                continue
            wr = [r for r in active if r["spec_id"] == sid and str(r["win"]) == w]
            if not wr:
                continue
            mtm_net = sum(float(x["verified_schedule"]["mtm_net_usd"]) for x in wr)
            exec_ds = sum(float(x["target_executed_fill_notional_usd"]) for x in wr)
            rec = {
                "win": w,
                "ret_bps": win_ew_ret[w],
                "mtm_net_usd": mtm_net,
                "target_executed_fill_notional_usd": exec_ds,
                "spr": sum(float(x["spr"]) for x in wr),
                "drft": sum(float(x["drft"]) for x in wr),
                "pex": sum(float(x["pex"]) for x in wr),
                "tslp": sum(float(x["tslp"]) for x in wr),
            }
            if win_ew_ret[w] > 0.0:
                up_list.append(rec)
            else:
                dn_list.append(rec)
            xs_pct.append(win_ew_ret[w] / 100.0)
            ys_usd.append(mtm_net)

        symmetry["by_spec"][sid] = compute_regime_symmetry(up_list, dn_list)
        beta_reg["by_spec"][sid] = {
            "x_unit": "equal_weight_18_rwa_return_pct",
            "y_unit": "window_verified_mtm_net_usd",
            **compute_ols_beta_regression(xs_pct, ys_usd),
        }

    pooled_up: list[dict[str, Any]] = []
    pooled_dn: list[dict[str, Any]] = []
    pooled_x: list[float] = []
    pooled_y: list[float] = []
    for sym in symbols:
        sx: list[float] = []
        sy: list[float] = []
        for w in win_ids:
            if (w, sym) not in sym_win_ret:
                continue
            matches = [r for r in active if r["spec_id"] == primary_spec_id and str(r["win"]) == w and r["symbol"] == sym]
            if not matches:
                continue
            r = matches[0]
            ret_bp = sym_win_ret[(w, sym)]
            net_v = float(r["verified_schedule"]["mtm_net_usd"])
            rec = {
                "win": w,
                "symbol": sym,
                "ret_bps": ret_bp,
                "mtm_net_usd": net_v,
                "target_executed_fill_notional_usd": float(r["target_executed_fill_notional_usd"]),
                "spr": float(r["spr"]),
                "drft": float(r["drft"]),
                "pex": float(r["pex"]),
                "tslp": float(r["tslp"]),
            }
            if ret_bp > 0.0:
                pooled_up.append(rec)
            else:
                pooled_dn.append(rec)
            sx.append(ret_bp / 100.0)
            sy.append(net_v)
            pooled_x.append(ret_bp / 100.0)
            pooled_y.append(net_v)
        beta_reg["primary_per_symbol"][sym] = {
            "x_unit": f"{sym}_window_return_pct",
            "y_unit": "symbol_window_verified_mtm_net_usd",
            **compute_ols_beta_regression(sx, sy),
        }

    symmetry["primary_pooled_symbol_windows"] = compute_regime_symmetry(pooled_up, pooled_dn)
    beta_reg["primary_pooled_symbol_windows"] = {
        "x_unit": "symbol_window_return_pct",
        "y_unit": "symbol_window_verified_mtm_net_usd",
        **compute_ols_beta_regression(pooled_x, pooled_y),
    }
    return symmetry, beta_reg


def summarize_arcus_m1_results(
    rows: Sequence[dict[str, Any]],
    *,
    symbols: Sequence[str],
    specs: Sequence[SimSpec],
) -> dict[str, Any]:
    """Build split summaries, per-symbol league tables, toxic stress matrix, symmetry, beta, and multiple testing."""
    active = [r for r in rows if not r.get("skipped")]

    def _win_num(w: str) -> int:
        m = re.search(r"(\d+)$", w)
        return int(m.group(1)) if m else 0

    dev_rows = [r for r in active if 1 <= _win_num(str(r["win"])) <= 20]
    hold_rows = [r for r in active if 21 <= _win_num(str(r["win"])) <= 25]
    oos_rows = [r for r in active if _win_num(str(r["win"])) in (26, 27)]

    # Causally select top-5 lowest-cost symbols on selection_dev for S4_hold600s_only @ o100_c150_e150 q200
    primary_sid = "S4_hold600s_only|o100_c150_e150|q200|baseline_prints_only"
    if not any(r["spec_id"] == primary_sid for r in active):
        primary_sid = specs[0].spec_id

    dev_sym_buckets: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for r in dev_rows:
        if r["spec_id"] == primary_sid:
            dev_sym_buckets[str(r["symbol"])].append(r)
    dev_league: list[dict[str, Any]] = []
    for sym, grp in sorted(dev_sym_buckets.items()):
        agg = aggregate_arcus_bucket(grp, include_symbol=True)
        agg.pop("by_window", None)
        dev_league.append(agg)
    dev_league.sort(
        key=lambda x: (
            x["verified"]["double_sided_exec_mtm_usd_per_m"]
            if x["verified"]["double_sided_exec_mtm_usd_per_m"] is not None
            else 1e9
        )
    )
    top5_dev_symbols = [x["symbol"] for x in dev_league if x["double_sided_exec_volume_usd"] > 0.0][:5]

    cohorts: dict[str, set[str]] = {
        "all_18_rwa": set(symbols),
        "ex_nvda_17_rwa": set(symbols) - {"NVDA"},
        "core_4_rwa": set(CORE_4_SYMBOLS) & set(symbols),
    }
    if top5_dev_symbols:
        cohorts["top5_dev_selected_rwa"] = set(top5_dev_symbols)

    def _agg_split(srows: Sequence[dict[str, Any]], *, keep_by_window: bool = False) -> dict[str, Any]:
        out: dict[str, Any] = {}
        for cname, cset in cohorts.items():
            buckets: dict[str, list[dict[str, Any]]] = defaultdict(list)
            for r in srows:
                if r["symbol"] in cset:
                    buckets[str(r["spec_id"])].append(r)
            c_out: dict[str, Any] = {}
            for sid, grp in sorted(buckets.items()):
                agg = aggregate_arcus_bucket(grp)
                if not keep_by_window:
                    agg.pop("by_window", None)
                c_out[sid] = agg
            out[cname] = c_out
        return out

    splits: dict[str, Any] = {
        "selection_dev_w01_w20": _agg_split(dev_rows),
        "holdout_w21_w25": _agg_split(hold_rows),
        "w26_w27_oos": _agg_split(oos_rows),
        "all_w01_w27": _agg_split(active, keep_by_window=True),
    }

    # Per-symbol table across all_w01_w27 for primary spec and S4_hold15s baseline
    per_symbol_all: dict[str, list[dict[str, Any]]] = {}
    for target_sid in sorted(
        {
            sp.spec_id
            for sp in specs
            if sp.scenario_id == "baseline_prints_only"
            and sp.quote_usd == 200.0
            and (sp.order_delay_ms, sp.cancel_delay_ms, sp.exit_delay_ms) == PRIMARY_LATENCY_TRIPLE
        }
    ):
        sbuckets: dict[str, list[dict[str, Any]]] = defaultdict(list)
        for r in active:
            if r["spec_id"] == target_sid:
                sbuckets[str(r["symbol"])].append(r)
        slist: list[dict[str, Any]] = []
        for sym, grp in sorted(sbuckets.items()):
            agg = aggregate_arcus_bucket(grp, include_symbol=True)
            agg.pop("by_window", None)
            slist.append(agg)
        slist.sort(
            key=lambda x: (
                x["verified"]["double_sided_exec_mtm_usd_per_m"]
                if x["verified"]["double_sided_exec_mtm_usd_per_m"] is not None
                else 1e9
            )
        )
        per_symbol_all[target_sid] = slist

    # Bootstrap CIs on all_18_rwa for primary cadence arms & vs S4_hold15s
    all18 = splits["all_w01_w27"].get("all_18_rwa", {})
    bootstrap: dict[str, Any] = {"single_arm_ci": {}, "paired_vs_s4_hold15s": {}}
    base_sid = "S4_hold15s|o100_c150_e150|q200|baseline_prints_only"
    base_wins = _to_prereg_win_format(all18[base_sid]["by_window"]) if base_sid in all18 else []
    for sid, agg in sorted(all18.items()):
        if agg["section"] in ("arm_cadence", "clip_size") and agg["windows_covered"] >= 3:
            bootstrap["single_arm_ci"][sid] = stationary_bootstrap_single_spec(agg["by_window"])
        if base_wins and sid != base_sid and agg["section"] == "arm_cadence" and agg["latency_label"] == "o100_c150_e150":
            if agg["windows_covered"] >= 3:
                bootstrap["paired_vs_s4_hold15s"][sid] = politis_romano_stationary_bootstrap_paired(
                    _to_prereg_win_format(agg["by_window"]), base_wins
                )

    # Multiple testing (DSR & CSCV PBO) across baseline $200 arms
    mt_arm_win: dict[str, dict[str, float]] = defaultdict(dict)
    for r in active:
        if r["scenario_id"] == "baseline_prints_only" and r["quote_usd"] == 200.0:
            mt_arm_win[str(r["spec_id"])][str(r["win"])] = (
                mt_arm_win[str(r["spec_id"])].get(str(r["win"]), 0.0) + float(r["verified_schedule"]["mtm_net_usd"])
            )
    multiple_testing: dict[str, Any] = {}
    if len(mt_arm_win) >= 2:
        arms_sorted = sorted(mt_arm_win.keys())
        common_wins = sorted(set.intersection(*(set(mt_arm_win[a].keys()) for a in arms_sorted)), key=_window_sort_key)
        if len(common_wins) >= 4:
            series = {a: [mt_arm_win[a][w] for w in common_wins] for a in arms_sorted}
            varying = {a: v for a, v in series.items() if len(set(v)) > 1}
            if len(varying) >= 2:
                dsr_rep = deflated_sharpe_report(varying)
                n_blocks = 8 if len(common_wins) >= 16 else 4
                pbo_rep = cscv_pbo(varying, num_blocks=n_blocks) if len(common_wins) >= n_blocks * 2 else None
                multiple_testing["baseline_arm_cadence_grid"] = {
                    "windows_aligned": len(common_wins),
                    "trials_count": len(varying),
                    "dsr": dsr_rep,
                    "cscv_pbo": pbo_rep,
                }

    # Symmetry & OLS Beta across the 5 primary-cadence arms
    primary_arm_sids = [
        f"{arm}|o100_c150_e150|q200|baseline_prints_only"
        for arm in M1_ARM_SPECS
        if f"{arm}|o100_c150_e150|q200|baseline_prints_only" in all18
    ]
    symmetry, beta_reg = compute_symmetry_and_beta(
        active,
        symbols=symbols,
        primary_spec_id=primary_sid,
        arm_spec_ids=primary_arm_sids if primary_arm_sids else [primary_sid],
    )

    # Strip large per-window dicts except for core arm_cadence/clip_size/toxic_stress summaries to keep JSON crisp
    for sid, agg in list(all18.items()):
        if agg["section"] not in ("arm_cadence",):
            agg.pop("by_window", None)

    return {
        "primary_spec_id": primary_sid,
        "top5_dev_selected_symbols": top5_dev_symbols,
        "dev_symbol_league": dev_league,
        "splits": splits,
        "per_symbol_all_w01_w27": per_symbol_all,
        "bootstrap": bootstrap,
        "multiple_testing": multiple_testing,
        "up_down_symmetry": symmetry,
        "ols_beta_vs_rwa_return": beta_reg,
    }


def run_duckdb_arcus_m1(
    lakehouse_dir: Path,
    *,
    split: str = "all",
    window_ids: Sequence[str] | None = None,
    symbols: Sequence[str] | None = None,
    fee_evidence_path: Path | None = None,
    max_workers: int = 8,
    compact: bool = False,
    verify_parity: bool = False,
) -> dict[str, Any]:
    """Run empirical W01-W27 (`NOT -w28`) Method 1 benchmark on Arcus RWA markets."""
    selected_windows = discover_arcus_rwa_campaigns(lakehouse_dir, split=split, window_ids=window_ids)
    if not selected_windows:
        raise ValueError(f"No Arcus RWA campaigns found under {lakehouse_dir} for split={split!r}")

    sym_list = tuple(s.upper() for s in symbols) if symbols else ARCUS_RWA_SYMBOLS
    for s in sym_list:
        if s not in ARCUS_RWA_MARKET_MAP:
            raise ValueError(f"Unsupported Arcus RWA symbol {s!r}")

    _, _, _, _, fee_prov = resolve_arcus_fee_provenance(fee_evidence_path)
    specs = build_benchmark_specs(compact=compact)

    rows_out: list[dict[str, Any]] = []
    market_stats_all: dict[str, Any] = {}
    max_gross_err = 0.0
    max_net_err = 0.0
    max_mtm_cash_err = 0.0
    non_flat_count = 0

    # Shard across (win_id, 6-symbol chunk) so 12-16 workers stay saturated without stragglers
    chunk_size = 6 if len(sym_list) > 6 else len(sym_list)
    sym_chunks = [sym_list[i : i + chunk_size] for i in range(0, len(sym_list), chunk_size)]
    work_items = [(win_id, c_info, sc) for win_id, c_info in selected_windows.items() for sc in sym_chunks]
    eff_workers = max(1, min(int(max_workers), len(work_items)))
    total_tasks = len(work_items)
    done_tasks = 0
    if eff_workers == 1:
        for win_id, c_info, sc in work_items:
            w_rows, w_ms, w_ge, w_ne, w_me, w_nf = _eval_arcus_window(
                lakehouse_dir, win_id, c_info, sc, specs, verify_parity=verify_parity
            )
            rows_out.extend(w_rows)
            market_stats_all.update(w_ms)
            max_gross_err = max(max_gross_err, w_ge)
            max_net_err = max(max_net_err, w_ne)
            max_mtm_cash_err = max(max_mtm_cash_err, w_me)
            non_flat_count += w_nf
            done_tasks += 1
            print(f"[m1-arcus] progress {done_tasks}/{total_tasks} ({win_id} {len(sc)} syms)", file=sys.stderr, flush=True)
    else:
        with concurrent.futures.ProcessPoolExecutor(max_workers=eff_workers) as pool:
            futs = {
                pool.submit(
                    _eval_arcus_window,
                    lakehouse_dir,
                    win_id,
                    c_info,
                    sc,
                    specs,
                    verify_parity=verify_parity,
                ): (win_id, len(sc))
                for win_id, c_info, sc in work_items
            }
            for fut in concurrent.futures.as_completed(futs):
                wid, n_sc = futs[fut]
                w_rows, w_ms, w_ge, w_ne, w_me, w_nf = fut.result()
                rows_out.extend(w_rows)
                market_stats_all.update(w_ms)
                max_gross_err = max(max_gross_err, w_ge)
                max_net_err = max(max_net_err, w_ne)
                max_mtm_cash_err = max(max_mtm_cash_err, w_me)
                non_flat_count += w_nf
                done_tasks += 1
                print(f"[m1-arcus] progress {done_tasks}/{total_tasks} ({wid} {n_sc} syms)", file=sys.stderr, flush=True)
        rows_out.sort(
            key=lambda r: (
                _window_sort_key(str(r["win"])),
                str(r["symbol"]),
                str(r["spec_id"]),
            )
        )

    assert_no_w28_leak(list(selected_windows.keys()), rows_out)
    summary = summarize_arcus_m1_results(rows_out, symbols=sym_list, specs=specs)

    return {
        "schema": "arcus_m1_single_venue_turnover_w01_w27_v1",
        "method": "M1_single_venue_arcus_rwa_mm",
        "venue_capability_snapshot_version": VENUE_CAPABILITY_SNAPSHOT_VERSION,
        "w28_excluded": True,
        "target_venue": TARGET_VENUE,
        "target_settlement": TARGET_SETTLEMENT,
        "ref_venues": list(REF_VENUES),
        "zero_points": True,
        "rebate_bps": 0.0,
        "fee_schedules": ARCUS_FEE_SCHEDULES,
        "fee_provenance": fee_prov,
        "symbols": list(sym_list),
        "windows": selected_windows,
        "specs_count": len(specs),
        "arm_specs": M1_ARM_SPECS,
        "invariants": {
            "total_rows": len(rows_out),
            "windows_count": len(selected_windows),
            "non_flat_count": non_flat_count,
            "max_gross_err": max_gross_err,
            "max_net_err": max_net_err,
            "max_mtm_cash_err": max_mtm_cash_err,
        },
        "market_continuity_stats": market_stats_all,
        **summary,
    }


def run_self_test() -> dict[str, Any]:
    """Verify P0 depth conservation, declared_end_ms, dual fee/volume $/M, and toxic-fill monotonicity."""
    tick = 0.01
    events: list[dict[str, Any]] = [
        {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 0, "kind": "ref", "venue": "lighter_robinhood", "bid": 100.00, "ask": 100.06},
        {"ts": 0, "kind": "ref", "venue": "trade_xyz", "bid": 100.00, "ask": 100.06},
        {"ts": 300, "kind": "trade", "side": "SELL", "price": 100.00, "size": 8.0},
        {"ts": 2_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 6_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 10_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 14_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 16_000, "kind": "book", "bids": [(99.98, 0.5)], "asks": [(100.04, 5.0)]},
        {"ts": 18_000, "kind": "book", "bids": [(99.98, 5.0)], "asks": [(100.04, 5.0)]},
        {"ts": 25_000, "kind": "trade", "side": "BUY", "price": 100.06, "size": 8.0},
        {"ts": 30_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 90_000, "kind": "ref", "venue": "trade_xyz", "bid": 102.00, "ask": 102.06},
    ]
    stream = prepare_replay_stream(events)

    arm15 = build_arm_config("S4_hold15s", order_delay_ms=100, cancel_delay_ms=150, exit_delay_ms=150, taker_fee_bps=2.1375)
    arm600 = build_arm_config("S4_hold600s_only", order_delay_ms=100, cancel_delay_ms=150, exit_delay_ms=150, taker_fee_bps=2.1375)
    r15 = simulate_core4_arm(stream, tick, arm15, declared_end_ms=30_000)
    r600 = simulate_core4_arm(stream, tick, arm600, declared_end_ms=30_000)
    assert abs(r15["_exact_mtm_gross_usd"] - r15["_exact_cash_ex_fee_usd"]) <= 1e-6
    assert abs(r600["_exact_mtm_gross_usd"] - r600["_exact_cash_ex_fee_usd"]) <= 1e-6
    assert r15["taker_exits"] >= 1
    assert r600["taker_exits"] == 0

    dual_ver = compute_dual_volume_costs(
        realized_gross_usd=float(r15["gross_usd"]),
        mtm_gross_usd=float(r15["mtm_gross_usd"]),
        ntl=float(r15["ntl"]),
        maker_volume_usd=float(r15["maker_volume_usd"]),
        taker_volume_usd=float(r15["taker_volume_usd"]),
        maker_fee_bps=0.0,
        taker_fee_bps=2.1375,
    )
    assert math.isclose(
        dual_ver["single_sided_exec_mtm_usd_per_m"],
        2.0 * dual_ver["double_sided_exec_mtm_usd_per_m"],
        abs_tol=1e-3,
    )

    return {
        "self_test": "ok",
        "specs_full_count": len(build_benchmark_specs(compact=False)),
        "specs_compact_count": len(build_benchmark_specs(compact=True)),
        "s4_hold15s_net_usd": r15["mtm_net_usd"],
        "s4_hold600s_net_usd": r600["mtm_net_usd"],
        "dual_ver_ds_usd_per_m": dual_ver["double_sided_exec_mtm_usd_per_m"],
        "dual_ver_ss_usd_per_m": dual_ver["single_sided_exec_mtm_usd_per_m"],
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Method 1 Single-Venue Arcus RWA High-Frequency MM Benchmark (W01-W27)")
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument("--split", choices=("selection_dev", "holdout", "w26_w27_oos", "all"), default="all")
    ap.add_argument("--windows", type=str, default=None)
    ap.add_argument("--symbols", type=str, default=None)
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument("--max-workers", type=int, default=8)
    ap.add_argument("--compact", action="store_true")
    ap.add_argument("--verify-parity", action="store_true")
    ap.add_argument("--output", type=Path, default=None)
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args(argv)

    if args.self_test:
        rendered = json.dumps(run_self_test(), indent=2, allow_nan=False)
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(rendered + "\n", encoding="utf-8")
        else:
            print(rendered)
        return 0

    win_list = [w.strip() for w in args.windows.split(",") if w.strip()] if args.windows else None
    sym_list = [s.strip() for s in args.symbols.split(",") if s.strip()] if args.symbols else None
    result = run_duckdb_arcus_m1(
        args.lakehouse,
        split=args.split,
        window_ids=win_list,
        symbols=sym_list,
        fee_evidence_path=args.fee_evidence,
        max_workers=args.max_workers,
        compact=args.compact,
        verify_parity=args.verify_parity,
    )
    rendered = json.dumps(result, indent=2, allow_nan=False)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered + "\n", encoding="utf-8")
        sha = hashlib.sha256(args.output.read_bytes()).hexdigest()
        print(
            json.dumps(
                {
                    "output": str(args.output),
                    "sha256": sha,
                    "invariants": result["invariants"],
                    "windows": sorted(result["windows"].keys(), key=_window_sort_key),
                }
            )
        )
    else:
        print(rendered)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
