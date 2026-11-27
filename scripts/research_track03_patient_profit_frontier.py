#!/usr/bin/env python3
"""Track 03 Untouched W27+W26 Paired Validation & Patient Profit Frontier (`W01-W27`, `NOT -w28`).

Strictly stdlib-only module top level (`duckdb` imported lazily inside lakehouse runner).
Reuses P0-fixed simulator primitives from `research_track03_full_universe_speedbump_mm.py`:
  - Explicit `declared_end_ms` cutoff immunity to late `ref` ticks.
  - Strict `stale_exit_blocks` + `exit_delay_ms` (`330 ms`) re-queueing when expired IOCs hit `!book_is_fresh`.
  - Displayed L1 depth clamping, `<= 5,000 ms` book freshness, `realized_only` vs `mtm_incl_residual`.
  - `OPT-01` fee provenance (`track_fee_evidence.py`), `eval_volume_farming_cost.py`, and
    `audit_multiple_testing_overfitting.py` (`PSR`, `N_eff`, `DSR`, `CSCV PBO`).
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
from research_track03_full_universe_speedbump_mm import (  # noqa: E402
    CRYPTO_SYMBOLS,
    FROZEN_STAGE_B_PARAMS,
    G5_BASE_PARAMS,
    LEGACY_5_RWA_EX_NVDA,
    LEGACY_5_RWA_SYMBOLS,
    MAX_CONTINUOUS_GAP_MS,
    TEN_HOURS_MS,
    VENUE_SYMBOL_MAP,
    SpeedbumpArmConfig,
    _campaign_parquet_files,
    _infer_tick_from_prices,
    _window_sort_key,
    prepare_replay_stream,
    simulate_prepared_arm,
)
from track_fee_evidence import (  # noqa: E402
    VENUE_CAPABILITY_SNAPSHOT_VERSION,
    build_fee_provenance,
    load_track_fee_evidence,
    resolve_venue_fee,
)

_CAMPAIGN_RE = re.compile(r"^tokyo-(crypto|rwa)-.+-w(\d+)$")

TOP_TIER_7_RWA: tuple[str, ...] = ("AAPL", "AMZN", "META", "MSFT", "QQQ", "SPY", "TSLA")
POSITIVE_MARKOUT_CORE_4: tuple[str, ...] = ("AAPL", "AMZN", "META", "MSFT")
INDEX_ETF_2: tuple[str, ...] = ("QQQ", "SPY")

FRONTIER_ARMS_SPEC: dict[str, dict[str, Any]] = {
    "G4_baseline": dict(FROZEN_STAGE_B_PARAMS["G4_baseline"]),
    "G5_microprice_tfi": dict(FROZEN_STAGE_B_PARAMS["G5_microprice_tfi"]),
    "S4_speedbump_combined": dict(FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"]),
    "P1_patient_60s": {
        **FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"],
        "max_hold_ms": 60_000,
        "reduce_only_penny_ticks": 1,
        "inventory_skew_bps_per_full_limit": 3.0,
    },
    "P2_patient_180s": {
        **FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"],
        "max_hold_ms": 180_000,
        "reduce_only_penny_ticks": 1,
        "inventory_skew_bps_per_full_limit": 4.0,
    },
    "P3_patient_600s": {
        **FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"],
        "max_hold_ms": 600_000,
        "reduce_only_penny_ticks": 1,
        "inventory_skew_bps_per_full_limit": 5.0,
    },
    "W1_wide_10bp_60s": {
        **FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"],
        "max_hold_ms": 60_000,
        "min_entry_edge_bps": 1.00,
        "latency_vol_spread_mult": 2.00,
        "reduce_only_penny_ticks": 1,
        "inventory_skew_bps_per_full_limit": 3.0,
    },
    "W2_wide_15bp_180s": {
        **FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"],
        "max_hold_ms": 180_000,
        "min_entry_edge_bps": 1.50,
        "latency_vol_spread_mult": 2.50,
        "vol_spread_mult": 1.00,
        "reduce_only_penny_ticks": 1,
        "inventory_skew_bps_per_full_limit": 4.0,
    },
    "W3_wide_25bp_300s": {
        **FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"],
        "max_hold_ms": 300_000,
        "min_entry_edge_bps": 2.50,
        "latency_vol_spread_mult": 3.00,
        "vol_spread_mult": 1.25,
        "reduce_only_penny_ticks": 1,
        "inventory_skew_bps_per_full_limit": 5.0,
    },
    "W4_ultra_40bp_600s": {
        **FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"],
        "max_hold_ms": 600_000,
        "min_entry_edge_bps": 4.00,
        "latency_vol_spread_mult": 4.00,
        "vol_spread_mult": 1.50,
        "reduce_only_penny_ticks": 1,
        "inventory_skew_bps_per_full_limit": 6.0,
        "quiet_min_spread_vol_ratio": 1.20,
    },
    "W5_nopenn_15bp_180s": {
        **FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"],
        "max_hold_ms": 180_000,
        "min_entry_edge_bps": 1.50,
        "latency_vol_spread_mult": 2.50,
        "vol_spread_mult": 1.00,
        "reduce_only_penny_ticks": 0,
        "inventory_skew_bps_per_full_limit": 4.0,
    },
    "W6_nopenn_25bp_600s": {
        **FROZEN_STAGE_B_PARAMS["S4_speedbump_combined"],
        "max_hold_ms": 600_000,
        "min_entry_edge_bps": 2.50,
        "latency_vol_spread_mult": 3.00,
        "vol_spread_mult": 1.25,
        "reduce_only_penny_ticks": 0,
        "inventory_skew_bps_per_full_limit": 5.0,
    },
}

DEFAULT_VENUE_LATENCIES: dict[str, tuple[tuple[int, int, int], ...]] = {
    "lighter_robinhood": ((200, 300, 330), (300, 300, 330)),
    "lighter_std": ((200, 300, 330), (300, 300, 330)),
    "lighter_prem": ((200, 300, 330), (300, 300, 330)),
    "arcus": ((36, 33, 40), (200, 300, 330)),
}

PRIMARY_VENUE_LATENCIES: dict[str, tuple[tuple[int, int, int], ...]] = {
    "lighter_robinhood": ((200, 300, 330),),
    "lighter_std": ((200, 300, 330),),
    "lighter_prem": ((200, 300, 330),),
    "arcus": ((36, 33, 40),),
}


@dataclasses.dataclass(frozen=True)
class FrontierTarget:
    """Target `(venue_profile, data_venue, domain, symbol)` spec with explicit fee tier & latency schedule."""

    venue_profile: str
    data_venue: str
    domain: str
    symbol: str
    raw_market: str
    settlement: str
    maker_fee_bps: float
    taker_fee_bps: float
    account_fee_status: str
    fee_source: str
    ref_venues: tuple[str, ...]
    latency_triples: tuple[tuple[int, int, int], ...]


def compute_breakeven_rebate_bps(
    gross_ex_fee_bps: float,
    taker_exit_share_s: float,
    maker_fee_bps: float,
    taker_fee_bps: float,
) -> tuple[float, float]:
    """Return `(breakeven_rebate_clamped_bps, signed_required_rebate_bps)` from cycle accounting.

    Cycle net cost bps = (maker_fee_bps - rebate_bps) * (2 - s) + taker_fee_bps * s - gross_ex_fee_bps.
    Setting net_cost = 0 yields required_rebate_bps = maker_fee_bps + (taker_fee_bps * s - gross_ex_fee_bps) / (2 - s).
    """
    s = min(1.0, max(0.0, float(taker_exit_share_s)))
    denom = max(1e-9, 2.0 - s)
    signed_req = float(maker_fee_bps) + (float(taker_fee_bps) * s - float(gross_ex_fee_bps)) / denom
    return round(max(0.0, signed_req), 6), round(signed_req, 6)


def build_frontier_targets(
    *,
    fee_evidence_path: Path | None = None,
    domains: Sequence[str] = ("crypto", "rwa"),
    venue_profiles: Sequence[str] = ("lighter_robinhood", "lighter_std", "lighter_prem", "arcus"),
    symbols: Sequence[str] | None = None,
    venue_latencies: dict[str, tuple[tuple[int, int, int], ...]] | None = None,
) -> tuple[list[FrontierTarget], dict[str, Any]]:
    """Build target matrix across LighterRH (0/0), Lighter Std (0/0), Lighter Prem (0.40/2.80), and Arcus RWA (0/2.1375)."""
    ev = load_track_fee_evidence(fee_evidence_path)
    domain_set = set(domains)
    sym_filter = {s.upper() for s in symbols} if symbols else None
    lat_map = venue_latencies or DEFAULT_VENUE_LATENCIES
    targets: list[FrontierTarget] = []

    for vp in venue_profiles:
        if vp not in lat_map:
            raise ValueError(f"Unsupported venue_profile {vp!r}; expected one of {sorted(lat_map)}")
        lats = lat_map[vp]

        if vp == "lighter_robinhood":
            data_venue = "lighter_robinhood"
            settlement = "USDG"
            mk, tk, st, src = resolve_venue_fee(
                "lighter_robinhood",
                "rwa",
                default_maker_bps=0.0,
                default_taker_bps=0.0,
                default_status="verified_account_tier" if ev.get("loaded") else "public_tier0_unverified",
                evidence=ev,
            )
            sym_map = VENUE_SYMBOL_MAP["lighter_robinhood"]
            allowed_domains = domain_set
        elif vp == "lighter_std":
            data_venue = "lighter"
            settlement = "USDC"
            mk, tk = 0.0, 0.0
            st = "lighter_standard_zero_scenario"
            src = "counterfactual_lighter_standard_zero"
            sym_map = VENUE_SYMBOL_MAP["lighter"]
            allowed_domains = domain_set
        elif vp == "lighter_prem":
            data_venue = "lighter"
            settlement = "USDC"
            mk, tk, st, src = resolve_venue_fee(
                "lighter",
                "rwa",
                default_maker_bps=0.40,
                default_taker_bps=2.80,
                default_status="verified_account_tier" if ev.get("loaded") else "public_tier0_schedule_only",
                evidence=ev,
            )
            sym_map = VENUE_SYMBOL_MAP["lighter"]
            allowed_domains = domain_set
        elif vp == "arcus":
            data_venue = "arcus"
            settlement = "USDC"
            sym_map = VENUE_SYMBOL_MAP["arcus"]
            # Arcus RWA only (`0.0 / 2.1375 bps` verified level_0 w/ 5% discount; exclude crypto 1.425/4.275).
            allowed_domains = domain_set & {"rwa"}
            mk, tk, st, src = resolve_venue_fee(
                "arcus",
                "rwa",
                default_maker_bps=0.0,
                default_taker_bps=2.1375,
                default_status="verified_account_tier" if ev.get("loaded") else "public_tier0_schedule_only",
                evidence=ev,
            )
        else:
            raise ValueError(f"Unsupported venue_profile {vp!r}")

        for sym, raw_mkt in sym_map.items():
            if sym_filter and sym not in sym_filter:
                continue
            dom = "crypto" if sym in CRYPTO_SYMBOLS else "rwa"
            if dom not in allowed_domains:
                continue
            if data_venue == "arcus":
                ref_vs: tuple[str, ...] = ("lighter_robinhood", "lighter", "trade_xyz")
            elif dom == "crypto":
                ref_vs = ("binance", "arcus")
            else:
                ref_vs = ("arcus", "trade_xyz")
            targets.append(
                FrontierTarget(
                    venue_profile=vp,
                    data_venue=data_venue,
                    domain=dom,
                    symbol=sym,
                    raw_market=raw_mkt,
                    settlement=settlement,
                    maker_fee_bps=float(mk),
                    taker_fee_bps=float(tk),
                    account_fee_status=st,
                    fee_source=src,
                    ref_venues=ref_vs,
                    latency_triples=tuple(lats),
                )
            )

    prov = build_fee_provenance(ev, venues_in_scope=("lighter_robinhood", "lighter", "arcus", "trade_xyz"))
    prov["lighter_standard_scenario"] = True
    prov["lighter_premium_reference_bps"] = [0.40, 2.80]
    prov["arcus_rwa_reference_bps"] = [0.0, 2.1375]
    return targets, prov


def discover_campaigns_w27(
    lakehouse_dir: Path,
    split: str,
    window_ids: Sequence[str] | None = None,
) -> dict[str, dict[str, dict[str, str]]]:
    """Dynamically discover `tokyo-{crypto,rwa}-*-w*` Hive campaigns strictly within W01-W27 (never W28)."""
    root = lakehouse_dir / "parquet" / "bbo_quotes" / "collector_region=tokyo"
    discovered: dict[str, dict[str, dict[str, Any]]] = defaultdict(dict)
    for d_str in glob.glob(str(root / "venue=*" / "utc_date=*" / "campaign_id=tokyo-*")):
        p = Path(d_str)
        cid = p.name.split("campaign_id=", 1)[-1]
        if cid.endswith("-w28"):
            continue
        m = _CAMPAIGN_RE.match(cid)
        if not m:
            continue
        stream_kind, win_num_str = m.group(1), m.group(2)
        win_num = int(win_num_str)
        if win_num < 1 or win_num > 27:
            # Hard guard: strictly exclude W28+!
            continue
        win_id = f"w{win_num:02d}"
        utc_date = p.parent.name.split("utc_date=", 1)[-1]
        entry = discovered[win_id].get(stream_kind)
        if entry is None or utc_date < entry["utc_date"]:
            discovered[win_id][stream_kind] = {"campaign_id": cid, "utc_date": utc_date, "win_num": win_num}

    def _keep_win(win_num: int) -> bool:
        if window_ids:
            return f"w{win_num:02d}" in set(window_ids)
        if split == "selection_dev":
            return 1 <= win_num <= 20
        if split == "holdout":
            return 21 <= win_num <= 25
        if split == "w26_w27_oos":
            return win_num in (26, 27)
        if split == "w26_oos":
            return win_num == 26
        if split == "w27_oos":
            return win_num == 27
        if split == "all":
            return 1 <= win_num <= 27
        raise ValueError(f"Unsupported split {split!r}")

    out: dict[str, dict[str, dict[str, str]]] = {}
    for win_id in sorted(discovered.keys(), key=_window_sort_key):
        kinds_out: dict[str, dict[str, str]] = {}
        for skind, info in discovered[win_id].items():
            if _keep_win(int(info["win_num"])):
                kinds_out[skind] = {"campaign_id": info["campaign_id"], "utc_date": info["utc_date"]}
        if kinds_out:
            out[win_id] = kinds_out
    return out


def build_frontier_arms(
    latency_triples: Sequence[tuple[int, int, int]],
    *,
    maker_fee_bps: float = 0.0,
    taker_fee_bps: float = 0.0,
    rebate_bps: float = 0.0,
    arm_names: Sequence[str] | None = None,
    arm_specs: dict[str, dict[str, Any]] | None = None,
) -> list[SpeedbumpArmConfig]:
    """Build patient passive unwind + wider speed-bump spread arm configurations."""
    spec_map = arm_specs if arm_specs is not None else FRONTIER_ARMS_SPEC
    selected_names = list(arm_names) if arm_names is not None else list(spec_map.keys())
    unknown = sorted(set(selected_names) - set(spec_map.keys()))
    if unknown:
        raise ValueError(f"Unknown frontier arm(s): {unknown}; valid: {sorted(spec_map.keys())}")

    arms: list[SpeedbumpArmConfig] = []
    for o_ms, c_ms, e_ms in latency_triples:
        for name in selected_names:
            raw_spec = dict(spec_map[name])
            use_sb_cancel = bool(raw_spec.pop("use_speed_bump_cancel_ms", False))
            if use_sb_cancel:
                raw_spec["speed_bump_cancel_ms"] = float(c_ms)
            arms.append(
                SpeedbumpArmConfig(
                    name=name,
                    order_delay_ms=o_ms,
                    cancel_delay_ms=c_ms,
                    exit_delay_ms=e_ms,
                    maker_fee_bps=maker_fee_bps,
                    taker_fee_bps=taker_fee_bps,
                    rebate_bps=rebate_bps,
                    **raw_spec,
                )
            )
    return arms


def aggregate_frontier_bucket(grp: Sequence[dict[str, Any]], *, include_symbol: bool = False) -> dict[str, Any]:
    """Aggregate simulation rows into full profitability, cost, capacity, hold-time, and P0 safety metrics."""
    first = grp[0]
    spr = sum(float(r["spr"]) for r in grp)
    drft = sum(float(r["drft"]) for r in grp)
    pex = sum(float(r["pex"]) for r in grp)
    tslp = sum(float(r["tslp"]) for r in grp)
    realized_gross_usd = spr + drft + pex + tslp
    unrealized_mtm_usd = sum(float(r.get("unrealized_mtm_usd", 0.0)) for r in grp)
    mtm_gross_usd = realized_gross_usd + unrealized_mtm_usd
    fee_usd = sum(float(r["fee_usd"]) for r in grp)
    realized_net_usd = realized_gross_usd - fee_usd
    mtm_net_usd = mtm_gross_usd - fee_usd
    ntl = sum(float(r["ntl"]) for r in grp)
    maker_vol = sum(float(r["maker_volume_usd"]) for r in grp)
    taker_vol = sum(float(r["taker_volume_usd"]) for r in grp)
    res_notl = sum(float(r.get("terminal_residual_notional_usd", 0.0)) for r in grp)
    depth_clipped = sum(int(r.get("depth_clipped_exits", 0)) for r in grp)
    stale_cancels = sum(int(r.get("stale_book_cancels", 0)) for r in grp)
    stale_blocks = sum(int(r.get("stale_exit_blocks", 0)) for r in grp)
    tox_pauses = sum(int(r.get("toxicity_pauses", 0)) for r in grp)
    hold_wsum = sum(float(r.get("hold_weighted_sum_ms", 0.0)) for r in grp)
    max_hold_obs = max((int(r.get("max_hold_observed_ms", 0)) for r in grp), default=0)

    realized_gbp = (realized_gross_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    mtm_gbp = (mtm_gross_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    net_bp = (mtm_net_usd / ntl) * 1e4 if ntl > 0.0 else 0.0
    s_share = min(1.0, max(0.0, taker_vol / ntl)) if ntl > 0.0 else 0.0

    mk_fee = float(first["maker_fee_bps"])
    tk_fee = float(first["taker_fee_bps"])
    reb_bp = float(first["rebate_bps"])

    realized_cost = compute_volume_farming_cost(
        realized_gbp, s_share, tk_fee, maker_fee_bps=mk_fee, rebate_bps=reb_bp
    )
    mtm_cost = compute_volume_farming_cost(
        mtm_gbp, s_share, tk_fee, maker_fee_bps=mk_fee, rebate_bps=reb_bp
    )
    zero_rebate_mtm_cost = compute_volume_farming_cost(
        mtm_gbp, s_share, tk_fee, maker_fee_bps=mk_fee, rebate_bps=0.0
    )
    be_rebate_clamped, be_rebate_signed = compute_breakeven_rebate_bps(mtm_gbp, s_share, mk_fee, tk_fee)

    # Counterfactual Lighter Premium (0.40/2.80) cost sensitivity on same gross & s.
    prem_cost = compute_volume_farming_cost(mtm_gbp, s_share, 2.80, maker_fee_bps=0.40, rebate_bps=0.0)

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

    by_win: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for r in grp:
        by_win[str(r["win"])].append(r)

    cum_net = 0.0
    peak_net = 0.0
    max_dd_usd = 0.0
    pos_wins = 0
    total_hours = 0.0
    win_net_series: list[float] = []
    worst_win: dict[str, Any] | None = None
    for win_id in sorted(by_win.keys(), key=_window_sort_key):
        wgrp = by_win[win_id]
        w_mtm_net = sum(float(x["mtm_net_usd"]) for x in wgrp)
        w_mtm_gross = sum(float(x["mtm_gross_usd"]) for x in wgrp)
        w_ntl = sum(float(x["ntl"]) for x in wgrp)
        w_tvol = sum(float(x["taker_volume_usd"]) for x in wgrp)
        w_gbp = (w_mtm_gross / w_ntl) * 1e4 if w_ntl > 0.0 else 0.0
        w_s = min(1.0, max(0.0, w_tvol / w_ntl)) if w_ntl > 0.0 else 0.0
        w_cost = compute_volume_farming_cost(w_gbp, w_s, tk_fee, maker_fee_bps=mk_fee, rebate_bps=reb_bp)
        total_hours += max(float((x.get("continuity") or {}).get("hours", 0.0)) for x in wgrp)
        win_net_series.append(w_mtm_net)
        cum_net += w_mtm_net
        if cum_net > peak_net:
            peak_net = cum_net
        max_dd_usd = max(max_dd_usd, peak_net - cum_net)
        if w_mtm_net > 0.0:
            pos_wins += 1
        if worst_win is None or w_mtm_net < worst_win["net_usd"]:
            worst_win = {
                "win": win_id,
                "net_usd": round(w_mtm_net, 4),
                "ntl": round(w_ntl, 2),
                "double_sided_usd_per_m": w_cost.double_sided_usd_per_m,
            }

    psr_zero: float | None = None
    if len(win_net_series) >= 3:
        mom = series_moments(win_net_series)
        psr_zero = round(probabilistic_sharpe_ratio(mom, 0.0), 6)

    n_wins = len(by_win)
    out: dict[str, Any] = {
        "arm": first["arm"],
        "latency_label": first["latency_label"],
        "order_delay_ms": first["order_delay_ms"],
        "cancel_delay_ms": first["cancel_delay_ms"],
        "exit_delay_ms": first["exit_delay_ms"],
        "venue_profile": first["venue_profile"],
        "venue": first["venue"],
        "settlement": first["settlement"],
        "maker_fee_bps": mk_fee,
        "taker_fee_bps": tk_fee,
        "account_fee_status": first["account_fee_status"],
        "windows_covered": n_wins,
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
        "fee_usd": round(fee_usd, 6),
        "realized_net_usd": round(realized_net_usd, 6),
        "mtm_net_usd": round(mtm_net_usd, 6),
        "ntl": round(ntl, 4),
        "ntl_per_window_usd": round(ntl / n_wins, 2) if n_wins > 0 else 0.0,
        "maker_volume_usd": round(maker_vol, 4),
        "taker_volume_usd": round(taker_vol, 4),
        "gross_ex_fee_bps": round(mtm_gbp, 6),
        "net_per_ntl_bp": round(net_bp, 6),
        "taker_exit_share_s": round(s_share, 6),
        "natural_passive_exit_share": round(1.0 - s_share, 6),
        "realized_only_usd_per_m": realized_cost.double_sided_usd_per_m,
        "mtm_incl_residual_usd_per_m": mtm_cost.double_sided_usd_per_m,
        "zero_rebate_usd_per_m": zero_rebate_mtm_cost.double_sided_usd_per_m,
        "breakeven_rebate_bps": be_rebate_clamped,
        "required_rebate_signed_bps": be_rebate_signed,
        "premium_04_28_usd_per_m": prem_cost.double_sided_usd_per_m,
        "maker_volume_usd_per_m": mtm_cost.maker_volume_usd_per_m,
        "terminal_residual_notional_usd": round(res_notl, 4),
        "avg_hold_ms": round(hold_wsum / ntl, 2) if ntl > 0.0 else 0.0,
        "avg_hold_s": round((hold_wsum / ntl) / 1_000.0, 3) if ntl > 0.0 else 0.0,
        "max_hold_observed_ms": max_hold_obs,
        "depth_clipped_exits": depth_clipped,
        "stale_book_cancels": stale_cancels,
        "stale_exit_blocks": stale_blocks,
        "toxicity_pauses": tox_pauses,
        "markout_1s_bps": _agg_mk("markout_1s_bps", "markout_1s_den"),
        "markout_5s_bps": _agg_mk("markout_5s_bps", "markout_5s_den"),
        "markout_10s_bps": _agg_mk("markout_10s_bps", "markout_10s_den"),
        "cancel_in_flight_fills": cif_fills,
        "cancel_in_flight_share": round(cif_share, 6),
        "positive_window_ratio": round(pos_wins / n_wins, 4) if n_wins > 0 else 0.0,
        "psr_zero": psr_zero,
        "max_drawdown_usd": round(max_dd_usd, 4),
        "worst_10h_window": worst_win,
    }
    if include_symbol:
        out["symbol"] = first["symbol"]
        out["domain"] = first["domain"]
    return out


def derive_positive_markout_subtier_from_dev(
    dev_rows: Sequence[dict[str, Any]],
    *,
    arm_name: str = "S4_speedbump_combined",
    min_windows: int = 10,
    min_markout_1s_bps: float = 0.0,
    exclude_symbols: Sequence[str] = ("NVDA",),
) -> dict[str, dict[str, Any]]:
    """Causally freeze positive-1s-markout RWA sub-tiers strictly from `selection_dev` (`w01-w20`)."""
    excl = set(exclude_symbols)
    by_vs: dict[tuple[str, str], list[dict[str, Any]]] = defaultdict(list)
    for r in dev_rows:
        if r.get("skipped") or r.get("domain") != "rwa" or r.get("arm") != arm_name:
            continue
        # Use primary latency per venue: o200_c300_e330 for Lighter, o36_c33_e40 for Arcus.
        vp = str(r["venue_profile"])
        expected_lat = "o36_c33_e40" if vp == "arcus" else "o200_c300_e330"
        if r.get("latency_label") != expected_lat:
            continue
        by_vs[(vp, str(r["symbol"]))].append(r)

    out: dict[str, dict[str, Any]] = {}
    for vp in sorted({k[0] for k in by_vs}):
        league: list[dict[str, Any]] = []
        selected: list[str] = []
        for (v_p, sym), grp in sorted(by_vs.items()):
            if v_p != vp:
                continue
            agg = aggregate_frontier_bucket(grp, include_symbol=True)
            league.append(agg)
            mk1 = agg.get("markout_1s_bps")
            if (
                sym not in excl
                and agg["windows_covered"] >= min_windows
                and mk1 is not None
                and float(mk1) > min_markout_1s_bps
                and agg["ntl"] > 0.0
            ):
                selected.append(sym)
        league.sort(key=lambda x: -(x["markout_1s_bps"] if x["markout_1s_bps"] is not None else -999.0))
        out[vp] = {
            "rule": {
                "arm": arm_name,
                "min_windows": min_windows,
                "min_markout_1s_bps": min_markout_1s_bps,
                "exclude_symbols": list(exclude_symbols),
            },
            "selected_symbols": sorted(selected),
            "league_table": league,
        }
    return out


def compute_frontier_multiple_testing(
    rows: Sequence[dict[str, Any]],
    *,
    cohort_filter: Sequence[str] | None = None,
    domain_filter: str | None = None,
    venue_profile_filter: str | None = None,
) -> dict[str, dict[str, Any]]:
    """Compute PSR / N_eff / DSR / CSCV PBO across arms per `(venue_profile, latency_label)` on aligned windows."""
    active = [r for r in rows if not r.get("skipped")]
    if domain_filter:
        active = [r for r in active if r.get("domain") == domain_filter]
    if venue_profile_filter:
        active = [r for r in active if r.get("venue_profile") == venue_profile_filter]
    if cohort_filter is not None:
        allowed = set(cohort_filter)
        active = [r for r in active if r.get("symbol") in allowed]

    by_vl: dict[tuple[str, str], dict[str, dict[str, float]]] = defaultdict(lambda: defaultdict(dict))
    for r in active:
        vl = (str(r["venue_profile"]), str(r["latency_label"]))
        arm = str(r["arm"])
        win = str(r["win"])
        by_vl[vl][arm][win] = by_vl[vl][arm].get(win, 0.0) + float(r["mtm_net_usd"])

    out: dict[str, dict[str, Any]] = {}
    for (vp, lat), arm_map in sorted(by_vl.items()):
        if len(arm_map) < 2:
            continue
        arms_sorted = sorted(arm_map.keys())
        common_wins = sorted(
            set.intersection(*(set(arm_map[a].keys()) for a in arms_sorted)),
            key=_window_sort_key,
        )
        if len(common_wins) < 4:
            continue
        series = {a: [arm_map[a][w] for w in common_wins] for a in arms_sorted}
        varying = {a: v for a, v in series.items() if len(set(v)) > 1}
        if len(varying) < 2:
            continue
        dsr_rep = deflated_sharpe_report(varying)
        n_blocks = 8 if len(common_wins) >= 16 else 4
        pbo_rep = cscv_pbo(varying, num_blocks=n_blocks) if len(common_wins) >= n_blocks * 2 else None
        out[f"{vp}|{lat}"] = {
            "windows_aligned": len(common_wins),
            "window_ids": common_wins,
            "dsr": dsr_rep,
            "cscv_pbo": pbo_rep,
        }
    return out


def summarize_frontier_cohorts(
    rows: Sequence[dict[str, Any]],
    *,
    pos_markout_by_vp: dict[str, Sequence[str]] | None = None,
) -> dict[str, Any]:
    """Summarize Top-Tier 7-RWA, Positive-Markout Core 4, Dev-Selected Pos-Markout, Index ETF, Full RWA, and Crypto."""
    active = [r for r in rows if not r.get("skipped")]

    def _agg_map(pred: Any, *, include_symbol: bool = False) -> dict[str, Any]:
        buckets: dict[tuple[Any, ...], list[dict[str, Any]]] = defaultdict(list)
        for r in active:
            if not pred(r):
                continue
            key = (
                (r["arm"], r["venue_profile"], r["latency_label"], r["symbol"])
                if include_symbol
                else (r["arm"], r["venue_profile"], r["latency_label"])
            )
            buckets[key].append(r)
        res: dict[str, Any] = {}
        for k, grp in sorted(buckets.items()):
            res["|".join(str(x) for x in k)] = aggregate_frontier_bucket(grp, include_symbol=include_symbol)
        return res

    tt7_set = set(TOP_TIER_7_RWA)
    pm4_set = set(POSITIVE_MARKOUT_CORE_4)
    idx_set = set(INDEX_ETF_2)
    leg5_set = set(LEGACY_5_RWA_SYMBOLS)
    leg5_ex_set = set(LEGACY_5_RWA_EX_NVDA)
    crypto_set = set(CRYPTO_SYMBOLS)

    cohorts: dict[str, Any] = {
        "top_tier_7_rwa": _agg_map(lambda r: r["symbol"] in tt7_set),
        "positive_markout_core_4_rwa": _agg_map(lambda r: r["symbol"] in pm4_set),
        "index_etf_2_rwa": _agg_map(lambda r: r["symbol"] in idx_set),
        "legacy_5_rwa_incl_nvda": _agg_map(lambda r: r["symbol"] in leg5_set),
        "legacy_5_rwa_ex_nvda": _agg_map(lambda r: r["symbol"] in leg5_ex_set),
        "full_universe_rwa": _agg_map(lambda r: r["domain"] == "rwa"),
        "full_universe_rwa_ex_nvda": _agg_map(lambda r: r["domain"] == "rwa" and r["symbol"] != "NVDA"),
        "crypto": _agg_map(lambda r: r["symbol"] in crypto_set),
        "full_universe_all": _agg_map(lambda r: True),
        "by_symbol": _agg_map(lambda r: True, include_symbol=True),
    }
    if pos_markout_by_vp:
        pm_sets = {vp: set(syms) for vp, syms in pos_markout_by_vp.items()}
        cohorts["pos_markout_dev_selected_rwa"] = _agg_map(
            lambda r: r["symbol"] in pm_sets.get(str(r["venue_profile"]), set())
        )
    return cohorts


def _eval_frontier_window(
    lakehouse_dir: Path,
    win_id: str,
    win_map: dict[str, dict[str, str]],
    targets: Sequence[FrontierTarget],
    arm_names: Sequence[str] | None,
    rebate_bps: float,
    arm_specs: dict[str, dict[str, Any]] | None,
) -> tuple[list[dict[str, Any]], dict[str, Any], float, float, float, int]:
    """Evaluate a single 10h window with dual-venue continuity/freshness auditing and P0-fixed replay."""
    import duckdb  # Lazy import so stdlib-only CI never requires duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    rows_out: list[dict[str, Any]] = []
    freshness_records: dict[str, Any] = {}
    max_gross_err = 0.0
    max_net_err = 0.0
    max_mtm_cash_err = 0.0
    non_flat_count = 0

    bbo_cache: dict[tuple[str, str], tuple[dict[str, list[tuple[int, float, float, float, float]]], dict[str, dict[str, Any]]]] = {}
    trade_cache: dict[tuple[str, str], dict[str, list[tuple[int, str, float, float]]]] = {}

    def load_venue_bbo_batch(
        venue: str,
        c_info: dict[str, str],
        markets: Sequence[str],
        bucket_ms: int = 250,
    ) -> tuple[dict[str, list[tuple[int, float, float, float, float]]], dict[str, dict[str, Any]]]:
        ckey = (venue, c_info["campaign_id"])
        if ckey in bbo_cache:
            return bbo_cache[ckey]
        files = _campaign_parquet_files(
            lakehouse_dir, "bbo_quotes", venue, c_info["utc_date"], c_info["campaign_id"]
        )
        if not files or not markets:
            bbo_cache[ckey] = ({}, {})
            return bbo_cache[ckey]

        f_list_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        mkt_list_sql = ", ".join(f"'{m}'" for m in sorted(set(markets)))

        stat_rows = con.execute(
            f"""
            WITH s AS (
                SELECT market,
                       available_at_ms,
                       available_at_ms - LAG(available_at_ms) OVER (PARTITION BY market ORDER BY available_at_ms) AS dt_ms
                FROM read_parquet({f_list_sql})
                WHERE market IN ({mkt_list_sql})
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
                   COALESCE(SUM(CASE WHEN dt_ms > 5000 THEN dt_ms - 5000 ELSE 0 END), 0) AS stale_excess_ms
            FROM s
            GROUP BY market
            """
        ).fetchall()

        cont_map: dict[str, dict[str, Any]] = {}
        for mkt, cnt, t0, t1, mgap, med_dt, p95_dt, gt5s, stale_ms in stat_rows:
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
                    "is_continuous_10h": dur >= TEN_HOURS_MS and int(mgap) <= MAX_CONTINUOUS_GAP_MS,
                }

        bucket_rows = con.execute(
            f"""
            SELECT market,
                   MIN(available_at_ms) AS ts_ms,
                   arg_min(bid_px_f64, available_at_ms) AS bid_px,
                   arg_min(CAST(bid_sz AS DOUBLE), available_at_ms) AS bid_sz,
                   arg_min(ask_px_f64, available_at_ms) AS ask_px,
                   arg_min(CAST(ask_sz AS DOUBLE), available_at_ms) AS ask_sz
            FROM read_parquet({f_list_sql})
            WHERE market IN ({mkt_list_sql})
              AND tradable = true
              AND bid_px_f64 > 0
              AND ask_px_f64 > bid_px_f64
            GROUP BY market, (available_at_ms // {int(bucket_ms)})
            ORDER BY market ASC, ts_ms ASC
            """
        ).fetchall()

        by_mkt: dict[str, list[tuple[int, float, float, float, float]]] = defaultdict(list)
        for mkt, ts_ms, bb, bsz, ba, asz in bucket_rows:
            by_mkt[str(mkt)].append((int(ts_ms), float(bb), float(bsz), float(ba), float(asz)))

        bbo_cache[ckey] = (by_mkt, cont_map)
        return bbo_cache[ckey]

    def load_venue_trades_batch(
        venue: str,
        c_info: dict[str, str],
        markets: Sequence[str],
    ) -> dict[str, list[tuple[int, str, float, float]]]:
        ckey = (venue, c_info["campaign_id"])
        if ckey in trade_cache:
            return trade_cache[ckey]
        files = _campaign_parquet_files(
            lakehouse_dir, "public_trades", venue, c_info["utc_date"], c_info["campaign_id"]
        )
        if not files or not markets:
            trade_cache[ckey] = {}
            return trade_cache[ckey]

        f_list_sql = "[" + ", ".join(f"'{p}'" for p in files) + "]"
        mkt_list_sql = ", ".join(f"'{m}'" for m in sorted(set(markets)))
        t_rows = con.execute(
            f"""
            SELECT market, available_at_ms, aggressor_side, price_f64, size_f64
            FROM read_parquet({f_list_sql})
            WHERE market IN ({mkt_list_sql})
              AND NOT is_zero_price_transfer
              AND price_f64 > 0
              AND size_f64 > 0
            ORDER BY market ASC, available_at_ms ASC
            """
        ).fetchall()

        by_mkt: dict[str, list[tuple[int, str, float, float]]] = defaultdict(list)
        for mkt, ts_ms, agg_side, px, sz in t_rows:
            by_mkt[str(mkt)].append((int(ts_ms), str(agg_side), float(px), float(sz)))
        trade_cache[ckey] = by_mkt
        return trade_cache[ckey]

    markets_needed: dict[tuple[str, str], set[str]] = defaultdict(set)
    for t in targets:
        if t.domain not in win_map:
            continue
        markets_needed[(t.domain, t.data_venue)].add(t.raw_market)
        for rv in t.ref_venues:
            if rv in VENUE_SYMBOL_MAP and t.symbol in VENUE_SYMBOL_MAP[rv]:
                markets_needed[(t.domain, rv)].add(VENUE_SYMBOL_MAP[rv][t.symbol])

    for (dom, v), mkts in sorted(markets_needed.items()):
        c_info = win_map[dom]
        _, cmap = load_venue_bbo_batch(v, c_info, sorted(mkts), bucket_ms=250)
        if v in ("lighter", "lighter_robinhood", "arcus"):
            load_venue_trades_batch(v, c_info, sorted(mkts))
        for mkt, cstats in cmap.items():
            freshness_records[f"{win_id}|{dom}|{v}|{mkt}"] = cstats

    # Group targets by `(domain, data_venue, symbol)` so `lighter_std` & `lighter_prem` share `PreparedStream`
    # and we immediately free `stream` after simulating each symbol (keeps worker RSS < 500 MB).
    grouped_targets: dict[tuple[str, str, str], list[FrontierTarget]] = defaultdict(list)
    for target in targets:
        if target.domain in win_map:
            grouped_targets[(target.domain, target.data_venue, target.symbol)].append(target)

    for (dom, data_venue, sym), t_group in sorted(grouped_targets.items()):
        c_info = win_map[dom]
        rep = t_group[0]
        bbo_map, cont_map = load_venue_bbo_batch(data_venue, c_info, [rep.raw_market])
        b_rows = bbo_map.get(rep.raw_market, [])
        cont = cont_map.get(
            rep.raw_market,
            {
                "raw_events": 0,
                "duration_ms": 0,
                "hours": 0.0,
                "max_gap_ms": 0,
                "median_dt_ms": 0.0,
                "p95_dt_ms": 0.0,
                "gaps_gt_5s": 0,
                "stale_time_share": 1.0,
                "is_continuous_10h": False,
            },
        )
        if len(b_rows) < 200:
            for target in t_group:
                rows_out.append(
                    {
                        "win": win_id,
                        "campaign_id": c_info["campaign_id"],
                        "domain": target.domain,
                        "venue_profile": target.venue_profile,
                        "venue": target.data_venue,
                        "symbol": target.symbol,
                        "raw_market": target.raw_market,
                        "settlement": target.settlement,
                        "account_fee_status": target.account_fee_status,
                        "cont": False,
                        "continuity": cont,
                        "ref_continuity": {},
                        "skipped": f"too_few_books:{cont.get('raw_events', 0)}",
                    }
                )
            continue

        events: list[dict[str, Any]] = [
            {
                "ts": r[0],
                "kind": "book",
                "bb": r[1],
                "bsz": max(r[2], 1e-9),
                "ba": r[3],
                "asz": max(r[4], 1e-9),
            }
            for r in b_rows
        ]
        refs_used: list[str] = []
        ref_cont: dict[str, Any] = {}
        for ref_v in rep.ref_venues:
            if ref_v not in VENUE_SYMBOL_MAP or sym not in VENUE_SYMBOL_MAP[ref_v]:
                continue
            ref_raw = VENUE_SYMBOL_MAP[ref_v][sym]
            r_bbo_map, r_cont_map = load_venue_bbo_batch(ref_v, c_info, [ref_raw])
            r_rows = r_bbo_map.get(ref_raw, [])
            if ref_raw in r_cont_map:
                ref_cont[ref_v] = r_cont_map[ref_raw]
            if len(r_rows) >= 200:
                refs_used.append(ref_v)
                for rr in r_rows:
                    events.append({"ts": rr[0], "kind": "ref", "venue": ref_v, "bid": rr[1], "ask": rr[3]})

        tr_map = load_venue_trades_batch(data_venue, c_info, [rep.raw_market])
        for ts_ms, agg_side, px, sz in tr_map.get(rep.raw_market, []):
            s_up = agg_side.upper()
            side_str = "BUY" if s_up in ("BUY", "B", "BID") else "SELL"
            events.append({"ts": ts_ms, "kind": "trade", "side": side_str, "price": px, "size": sz})

        stream = prepare_replay_stream(events)
        tick = _infer_tick_from_prices([r[1] for r in b_rows])
        declared_end = int(b_rows[-1][0])
        del events

        for target in t_group:
            arms = build_frontier_arms(
                target.latency_triples,
                maker_fee_bps=target.maker_fee_bps,
                taker_fee_bps=target.taker_fee_bps,
                rebate_bps=rebate_bps,
                arm_names=arm_names,
                arm_specs=arm_specs,
            )
            for arm_cfg in arms:
                sim_res = simulate_prepared_arm(stream, tick, arm_cfg, declared_end_ms=declared_end)
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
                rows_out.append(
                    {
                        "win": win_id,
                        "campaign_id": c_info["campaign_id"],
                        "domain": target.domain,
                        "venue_profile": target.venue_profile,
                        "venue": target.data_venue,
                        "symbol": target.symbol,
                        "raw_market": target.raw_market,
                        "settlement": target.settlement,
                        "account_fee_status": target.account_fee_status,
                        "fee_source": target.fee_source,
                        "maker_fee_bps": target.maker_fee_bps,
                        "taker_fee_bps": target.taker_fee_bps,
                        "rebate_bps": rebate_bps,
                        "tick_inferred": tick,
                        "declared_end_ms": declared_end,
                        "cont": bool(cont["is_continuous_10h"]),
                        "continuity": cont,
                        "ref_continuity": ref_cont,
                        "refs_used": refs_used,
                        **sim_res,
                    }
                )
        del stream

    bbo_cache.clear()
    trade_cache.clear()
    con.close()
    gc.collect()
    return rows_out, freshness_records, max_gross_err, max_net_err, max_mtm_cash_err, non_flat_count


def assert_no_w28_leak(windows: Sequence[str], rows: Sequence[dict[str, Any]]) -> None:
    """Fail-closed guard ensuring W28 never enters discovery, simulation, or summary."""
    for w in windows:
        if "28" in str(w) and str(w).lower().endswith("w28"):
            raise ValueError(f"Forbidden W28 window leaked: {w!r}")
    for r in rows:
        cid = str(r.get("campaign_id", ""))
        wid = str(r.get("win", ""))
        if cid.endswith("-w28") or wid == "w28":
            raise ValueError(f"Forbidden W28 row leaked: {wid!r} / {cid!r}")


def summarize_paired_freshness(
    freshness_map: dict[str, Any],
    windows: Sequence[str],
) -> dict[str, Any]:
    """Summarize target and ref venue continuity, gaps, and stale exposure across windows."""
    by_win_venue: dict[str, dict[str, Any]] = {}
    target_set = set(windows)
    grouped: dict[tuple[str, str, str], list[dict[str, Any]]] = defaultdict(list)
    for key, stats in freshness_map.items():
        win_id, dom, venue, _mkt = key.split("|", 3)
        if win_id not in target_set:
            continue
        grouped[(win_id, dom, venue)].append(stats)

    for (win_id, dom, venue), items in sorted(grouped.items()):
        by_win_venue[f"{win_id}|{dom}|{venue}"] = {
            "markets_observed": len(items),
            "continuous_10h_markets": sum(1 for x in items if x.get("is_continuous_10h")),
            "min_hours": round(min(float(x.get("hours", 0.0)) for x in items), 3),
            "max_gap_ms": max(int(x.get("max_gap_ms", 0)) for x in items),
            "median_dt_ms": round(
                sorted(float(x.get("median_dt_ms", 0.0)) for x in items)[len(items) // 2], 2
            ),
            "max_p95_dt_ms": round(max(float(x.get("p95_dt_ms", 0.0)) for x in items), 2),
            "max_stale_time_share": round(max(float(x.get("stale_time_share", 0.0)) for x in items), 6),
        }
    return by_win_venue


def run_duckdb_frontier(
    lakehouse_dir: Path,
    *,
    split: str = "all",
    window_ids: Sequence[str] | None = None,
    domains: Sequence[str] = ("crypto", "rwa"),
    venue_profiles: Sequence[str] = ("lighter_robinhood", "lighter_std", "lighter_prem", "arcus"),
    symbols: Sequence[str] | None = None,
    arm_names: Sequence[str] | None = None,
    rebate_bps: float = 0.0,
    fee_evidence_path: Path | None = None,
    venue_latencies: dict[str, tuple[tuple[int, int, int], ...]] | None = None,
    arm_specs: dict[str, dict[str, Any]] | None = None,
    max_workers: int = 1,
    include_rows: bool = False,
) -> dict[str, Any]:
    """Run causal W01-W27 evaluation across splits (`selection_dev`, `holdout`, `w26_w27_oos`) with P0 fixes."""
    selected_windows = discover_campaigns_w27(lakehouse_dir, split, window_ids)
    if not selected_windows:
        raise ValueError(f"No campaigns discovered under {lakehouse_dir} for split={split!r}")

    targets, fee_prov = build_frontier_targets(
        fee_evidence_path=fee_evidence_path,
        domains=domains,
        venue_profiles=venue_profiles,
        symbols=symbols,
        venue_latencies=venue_latencies,
    )

    rows_out: list[dict[str, Any]] = []
    freshness_all: dict[str, Any] = {}
    max_gross_err = 0.0
    max_net_err = 0.0
    max_mtm_cash_err = 0.0
    non_flat_count = 0

    # Shard work across `(win_id, data_venue)` so even 2-window runs saturate workers while sharing streams.
    targets_by_dv: dict[str, list[FrontierTarget]] = defaultdict(list)
    for t in targets:
        targets_by_dv[t.data_venue].append(t)
    work_items = [
        (win_id, win_map, dv_targets)
        for win_id, win_map in selected_windows.items()
        for _dv, dv_targets in sorted(targets_by_dv.items())
    ]

    eff_workers = max(1, min(int(max_workers), len(work_items)))
    if eff_workers == 1:
        for win_id, win_map, dv_targets in work_items:
            w_rows, w_fresh, w_ge, w_ne, w_me, w_nf = _eval_frontier_window(
                lakehouse_dir, win_id, win_map, dv_targets, arm_names, rebate_bps, arm_specs
            )
            rows_out.extend(w_rows)
            freshness_all.update(w_fresh)
            max_gross_err = max(max_gross_err, w_ge)
            max_net_err = max(max_net_err, w_ne)
            max_mtm_cash_err = max(max_mtm_cash_err, w_me)
            non_flat_count += w_nf
    else:
        with concurrent.futures.ProcessPoolExecutor(max_workers=eff_workers) as pool:
            futs = [
                pool.submit(
                    _eval_frontier_window,
                    lakehouse_dir,
                    win_id,
                    win_map,
                    dv_targets,
                    arm_names,
                    rebate_bps,
                    arm_specs,
                )
                for win_id, win_map, dv_targets in work_items
            ]
            for fut in concurrent.futures.as_completed(futs):
                w_rows, w_fresh, w_ge, w_ne, w_me, w_nf = fut.result()
                rows_out.extend(w_rows)
                freshness_all.update(w_fresh)
                max_gross_err = max(max_gross_err, w_ge)
                max_net_err = max(max_net_err, w_ne)
                max_mtm_cash_err = max(max_mtm_cash_err, w_me)
                non_flat_count += w_nf
        rows_out.sort(
            key=lambda r: (
                _window_sort_key(str(r["win"])),
                str(r.get("domain", "")),
                str(r.get("venue_profile", "")),
                str(r.get("symbol", "")),
                str(r.get("latency_label", "")),
                str(r.get("arm", "")),
            )
        )

    assert_no_w28_leak(list(selected_windows.keys()), rows_out)

    # Split rows strictly by window number for causal selection_dev -> holdout -> w26_w27_oos reporting.
    def _win_num(w: str) -> int:
        m = re.search(r"(\d+)$", w)
        return int(m.group(1)) if m else 0

    dev_rows = [r for r in rows_out if 1 <= _win_num(str(r["win"])) <= 20]
    hold_rows = [r for r in rows_out if 21 <= _win_num(str(r["win"])) <= 25]
    oos_rows = [r for r in rows_out if _win_num(str(r["win"])) in (26, 27)]
    w26_rows = [r for r in rows_out if _win_num(str(r["win"])) == 26]
    w27_rows = [r for r in rows_out if _win_num(str(r["win"])) == 27]

    pos_mk_discovery = derive_positive_markout_subtier_from_dev(dev_rows) if dev_rows else {}
    pos_mk_by_vp = {vp: spec["selected_symbols"] for vp, spec in pos_mk_discovery.items()}

    splits_summary: dict[str, Any] = {}
    for sname, srows in (
        ("selection_dev_w01_w20", dev_rows),
        ("holdout_w21_w25", hold_rows),
        ("w26_w27_oos", oos_rows),
        ("w26_oos", w26_rows),
        ("w27_oos", w27_rows),
        ("all_w01_w27", rows_out),
    ):
        if srows:
            splits_summary[sname] = summarize_frontier_cohorts(srows, pos_markout_by_vp=pos_mk_by_vp)

    multiple_testing: dict[str, Any] = {}
    if rows_out:
        multiple_testing["top_tier_7_rwa_w01_w27"] = compute_frontier_multiple_testing(
            rows_out, cohort_filter=TOP_TIER_7_RWA
        )
        multiple_testing["positive_markout_core_4_rwa_w01_w27"] = compute_frontier_multiple_testing(
            rows_out, cohort_filter=POSITIVE_MARKOUT_CORE_4
        )
        multiple_testing["full_universe_rwa_w01_w27"] = compute_frontier_multiple_testing(
            rows_out, domain_filter="rwa"
        )
        multiple_testing["crypto_w01_w27"] = compute_frontier_multiple_testing(
            rows_out, domain_filter="crypto"
        )
        if dev_rows:
            multiple_testing["top_tier_7_rwa_dev"] = compute_frontier_multiple_testing(
                dev_rows, cohort_filter=TOP_TIER_7_RWA
            )
            multiple_testing["positive_markout_core_4_rwa_dev"] = compute_frontier_multiple_testing(
                dev_rows, cohort_filter=POSITIVE_MARKOUT_CORE_4
            )
            if pos_mk_by_vp:
                for vp, syms in pos_mk_by_vp.items():
                    if syms:
                        multiple_testing[f"pos_markout_dev_selected_{vp}_w01_w27"] = compute_frontier_multiple_testing(
                            rows_out, cohort_filter=syms, venue_profile_filter=vp
                        )

    payload: dict[str, Any] = {
        "schema": "track03_patient_profit_frontier_v1",
        "venue_capability_snapshot_version": VENUE_CAPABILITY_SNAPSHOT_VERSION,
        "w28_excluded": True,
        "fee_provenance": fee_prov,
        "split": split,
        "rebate_bps": rebate_bps,
        "arm_specs": FRONTIER_ARMS_SPEC if arm_specs is None else arm_specs,
        "windows": selected_windows,
        "invariants": {
            "total_rows": len(rows_out),
            "non_flat_count": non_flat_count,
            "max_gross_err": max_gross_err,
            "max_net_err": max_net_err,
            "max_mtm_cash_err": max_mtm_cash_err,
        },
        "paired_venue_freshness_w26_w27": summarize_paired_freshness(
            freshness_all, [w for w in selected_windows if w in ("w26", "w27")]
        ),
        "paired_venue_freshness_all": summarize_paired_freshness(freshness_all, list(selected_windows.keys())),
        "positive_markout_discovery_dev": pos_mk_discovery,
        "splits": splits_summary,
        "multiple_testing": multiple_testing,
    }
    if include_rows:
        payload["rows"] = rows_out
    return payload


def run_self_test() -> dict[str, Any]:
    """Verify P0 fixes, patient unwind vs S4, breakeven rebate, and pure stdlib frontier invariants."""
    tick = 0.01
    # Synthetic stream: resting bid fills at 300ms, price stays near mid so 15s S4 takes IOC at 15_300ms,
    # whereas 60s patient P1 unwinds passively on BUY trade at 25_000ms!
    events: list[dict[str, Any]] = [
        {"ts": 0, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 0, "kind": "ref", "venue": "arcus", "bid": 100.00, "ask": 100.06},
        {"ts": 300, "kind": "trade", "side": "SELL", "price": 100.00, "size": 8.0},
        {"ts": 4_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 8_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 12_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 16_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 16_500, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 20_500, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 25_000, "kind": "trade", "side": "BUY", "price": 100.06, "size": 8.0},
        {"ts": 29_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        {"ts": 31_000, "kind": "book", "bids": [(100.00, 10.0)], "asks": [(100.06, 10.0)]},
        # Late ref tick that must be ignored via declared_end_ms=31_000!
        {"ts": 90_000, "kind": "ref", "venue": "arcus", "bid": 101.00, "ask": 101.06},
    ]
    stream = prepare_replay_stream(events)
    arms = build_frontier_arms(
        [(200, 300, 330)],
        maker_fee_bps=0.0,
        taker_fee_bps=2.1375,
        arm_names=["G4_baseline", "S4_speedbump_combined", "P1_patient_60s", "W2_wide_15bp_180s"],
    )
    results: dict[str, Any] = {}
    for arm in arms:
        res = simulate_prepared_arm(stream, tick, arm, declared_end_ms=31_000)
        assert abs(res["_exact_mtm_gross_usd"] - res["_exact_cash_ex_fee_usd"]) <= 1e-6
        assert abs(res["_exact_net_usd"] - (res["_exact_mtm_gross_usd"] - res["_exact_fee_usd"])) <= 1e-9
        results[arm.name] = res

    assert results["S4_speedbump_combined"]["taker_exits"] >= 1
    assert results["P1_patient_60s"]["taker_exits"] == 0
    assert results["P1_patient_60s"]["mtm_net_usd"] > results["S4_speedbump_combined"]["mtm_net_usd"]

    be_pos, be_signed = compute_breakeven_rebate_bps(-0.10, 0.25, 0.0, 0.0)
    assert math.isclose(be_pos, 0.10 / 1.75, abs_tol=1e-6)
    assert math.isclose(be_signed, 0.10 / 1.75, abs_tol=1e-6)

    return {
        "self_test": "ok",
        "arms_count": len(FRONTIER_ARMS_SPEC),
        "s4_net_usd": results["S4_speedbump_combined"]["mtm_net_usd"],
        "p1_net_usd": results["P1_patient_60s"]["mtm_net_usd"],
        "p1_avg_hold_ms": results["P1_patient_60s"]["avg_hold_ms"],
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Track 03 Untouched W27+W26 Paired Validation & Patient Profit Frontier")
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument(
        "--split",
        choices=("selection_dev", "holdout", "w26_w27_oos", "w26_oos", "w27_oos", "all"),
        default="all",
    )
    ap.add_argument("--windows", type=str, default=None)
    ap.add_argument("--domains", type=str, default="crypto,rwa")
    ap.add_argument("--venue-profiles", type=str, default="lighter_robinhood,lighter_std,lighter_prem,arcus")
    ap.add_argument("--symbols", type=str, default=None)
    ap.add_argument("--arms", type=str, default=None)
    ap.add_argument("--rebate-bps", type=float, default=0.0)
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument("--max-workers", type=int, default=4)
    ap.add_argument("--primary-latency-only", action="store_true")
    ap.add_argument("--include-rows", action="store_true")
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

    if args.rebate_bps < 0.0 or not math.isfinite(args.rebate_bps):
        ap.error("--rebate-bps must be finite and >= 0")

    win_list = [w.strip() for w in args.windows.split(",") if w.strip()] if args.windows else None
    dom_list = [d.strip() for d in args.domains.split(",") if d.strip()]
    vp_list = [v.strip() for v in args.venue_profiles.split(",") if v.strip()]
    sym_list = [s.strip() for s in args.symbols.split(",") if s.strip()] if args.symbols else None
    arm_list = [a.strip() for a in args.arms.split(",") if a.strip()] if args.arms else None

    result = run_duckdb_frontier(
        args.lakehouse,
        split=args.split,
        window_ids=win_list,
        domains=dom_list,
        venue_profiles=vp_list,
        symbols=sym_list,
        arm_names=arm_list,
        rebate_bps=args.rebate_bps,
        fee_evidence_path=args.fee_evidence,
        venue_latencies=PRIMARY_VENUE_LATENCIES if args.primary_latency_only else None,
        max_workers=args.max_workers,
        include_rows=args.include_rows,
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
