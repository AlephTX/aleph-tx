#!/usr/bin/env python3
"""Unit-safe Scenario 1 (03_incentive_turnover) cost-per-$1M evaluator (stdlib-only).

Implements audited double-sided, maker-only, and unknown-eligibility ($/M) cost accounting
without silently dropping non-zero maker fees or conflating funding with execution fees.
Also summarizes existing 10.25h Track 03 post-trade RWA markout/drift evidence alongside
pipeline bookdiff holdout/selection results.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from collections import defaultdict
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any, Sequence

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from track_fee_evidence import (  # noqa: E402
    VENUE_CAPABILITY_SNAPSHOT_VERSION,
    _confirmed_account_pair,
    _parse_verified_domain_overrides,
)

BPS_TO_DECIMAL = 1e-4
USD_PER_MILLION_SCALE = 100.0  # 1 bp per $1 = $100 per $1,000,000


def _require_finite(val: Any, name: str) -> float:
    if isinstance(val, bool) or val is None:
        raise ValueError(f"{name} must be a finite number, got {val!r}")
    try:
        out = float(val)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{name} must be numeric, got {val!r}") from exc
    if not math.isfinite(out):
        raise ValueError(f"{name} must be finite, got {val!r}")
    return out


def _require_probability(val: Any, name: str) -> float:
    out = _require_finite(val, name)
    if out < 0.0 or out > 1.0:
        raise ValueError(f"{name} must be in [0.0, 1.0], got {out}")
    return out


@dataclass(frozen=True)
class VenueFeeProfile:
    """Explicit venue fee schedule with provenance status (`OPT-01`)."""

    key: str
    venue: str
    scope: str
    maker_fee_bps: float
    taker_fee_bps: float
    account_fee_status: str
    eligible_verified: bool = False
    note: str = ""

    def __post_init__(self) -> None:
        _require_finite(self.maker_fee_bps, f"{self.key}.maker_fee_bps")
        taker = _require_finite(self.taker_fee_bps, f"{self.key}.taker_fee_bps")
        if taker < 0.0:
            raise ValueError(f"{self.key}.taker_fee_bps must be >= 0, got {taker}")


@dataclass(frozen=True)
class VolumeFarmingCostBreakdown:
    """Audited cost-per-$1M decomposition across explicit volume denominators."""

    gross_ex_fee_bps: float
    funding_cash_bps: float
    taker_exit_share_s: float
    maker_fee_bps: float
    taker_fee_bps: float
    rebate_bps: float
    effective_maker_fee_bps: float
    fee_cost_per_cycle_bps: float
    net_cost_per_cycle_bps: float
    double_sided_usd_per_m: float
    maker_volume_usd_per_m: float
    s_zero_double_sided_floor_usd_per_m: float
    qualified_usd_per_m: float | None


def compute_volume_farming_cost(
    gross_ex_fee_bps: float,
    taker_exit_share_s: float,
    taker_fee_bps: float,
    *,
    maker_fee_bps: float = 0.0,
    rebate_bps: float = 0.0,
    funding_cash_bps: float = 0.0,
    qualified_volume_ratio: float | None = None,
) -> VolumeFarmingCostBreakdown:
    """Compute USD/$1M volume-farming cost for a cycle with 1 maker entry + 1 (passive/taker) exit.

    Conventions (per cycle single-sided notional Q, double-sided notional 2*Q):
    - Maker volume per cycle = (2 - s) * Q  [1 entry + (1 - s) passive exit]
    - Taker volume per cycle = s * Q        [s taker/IOC exit]
    - Fee cost per Q (bps)   = (maker_fee_bps - rebate_bps) * (2 - s) + taker_fee_bps * s
    - Net cost per Q (bps)   = fee_cost_per_cycle_bps - gross_ex_fee_bps - funding_cash_bps
      (positive = net loss / cost; negative = net profit)
    - Double-sided USD/$1M   = (net_cost_per_cycle_bps / 2) * 100
    - Maker-only USD/$1M     = (net_cost_per_cycle_bps / (2 - s)) * 100
    """
    gross_bp = _require_finite(gross_ex_fee_bps, "gross_ex_fee_bps")
    s = _require_probability(taker_exit_share_s, "taker_exit_share_s")
    taker_bp = _require_finite(taker_fee_bps, "taker_fee_bps")
    if taker_bp < 0.0:
        raise ValueError(f"taker_fee_bps must be >= 0, got {taker_bp}")
    maker_bp = _require_finite(maker_fee_bps, "maker_fee_bps")
    rebate_bp = _require_finite(rebate_bps, "rebate_bps")
    if rebate_bp < 0.0:
        raise ValueError(f"rebate_bps must be >= 0, got {rebate_bp}")
    funding_bp = _require_finite(funding_cash_bps, "funding_cash_bps")

    eff_maker_bp = maker_bp - rebate_bp
    maker_legs = 2.0 - s
    fee_cycle_bp = eff_maker_bp * maker_legs + taker_bp * s
    net_cost_cycle_bp = fee_cycle_bp - gross_bp - funding_bp

    double_sided_usd_per_m = (net_cost_cycle_bp / 2.0) * USD_PER_MILLION_SCALE
    maker_vol_usd_per_m = (net_cost_cycle_bp / maker_legs) * USD_PER_MILLION_SCALE
    floor_s0_usd_per_m = ((2.0 * eff_maker_bp - gross_bp - funding_bp) / 2.0) * USD_PER_MILLION_SCALE

    qualified_usd_per_m: float | None = None
    if qualified_volume_ratio is not None:
        q_ratio = _require_finite(qualified_volume_ratio, "qualified_volume_ratio")
        if q_ratio <= 0.0:
            raise ValueError(f"qualified_volume_ratio must be > 0, got {q_ratio}")
        qualified_usd_per_m = (net_cost_cycle_bp / q_ratio) * USD_PER_MILLION_SCALE

    return VolumeFarmingCostBreakdown(
        gross_ex_fee_bps=round(gross_bp, 6),
        funding_cash_bps=round(funding_bp, 6),
        taker_exit_share_s=round(s, 6),
        maker_fee_bps=round(maker_bp, 6),
        taker_fee_bps=round(taker_bp, 6),
        rebate_bps=round(rebate_bp, 6),
        effective_maker_fee_bps=round(eff_maker_bp, 6),
        fee_cost_per_cycle_bps=round(fee_cycle_bp, 6),
        net_cost_per_cycle_bps=round(net_cost_cycle_bp, 6),
        double_sided_usd_per_m=round(double_sided_usd_per_m, 4),
        maker_volume_usd_per_m=round(maker_vol_usd_per_m, 4),
        s_zero_double_sided_floor_usd_per_m=round(floor_s0_usd_per_m, 4),
        qualified_usd_per_m=round(qualified_usd_per_m, 4) if qualified_usd_per_m is not None else None,
    )


def _find_tier0_profile(
    profiles: Sequence[dict[str, Any]],
    *,
    maker_bps: float | None = None,
    taker_bps: float | None = None,
) -> dict[str, Any] | None:
    for p in profiles:
        m = _require_finite(p.get("maker_fee_bps"), "tier0.maker_fee_bps")
        t = _require_finite(p.get("taker_fee_bps"), "tier0.taker_fee_bps")
        if maker_bps is not None and not math.isclose(m, maker_bps, abs_tol=1e-9):
            continue
        if taker_bps is not None and not math.isclose(t, taker_bps, abs_tol=1e-9):
            continue
        return p
    return None


def load_fee_profiles_from_evidence(evidence_path: Path) -> list[VenueFeeProfile]:
    """Load audited venue fee profiles from `account_fee_evidence_*.json` (fail-closed on unverified overrides)."""
    data = json.loads(evidence_path.read_text(encoding="utf-8"))
    venues = data.get("venues")
    if not isinstance(venues, dict):
        raise ValueError(f"Expected dict 'venues' in {evidence_path}")

    for venue_name, payload in venues.items():
        if not isinstance(payload, dict):
            raise ValueError(f"Expected dict venue record for {venue_name!r} in {evidence_path}")
        vname = str(venue_name)
        _confirmed_account_pair(payload, vname)
        _parse_verified_domain_overrides(payload, vname)

    out: list[VenueFeeProfile] = []

    if "arcus" in venues:
        arc = venues["arcus"]
        status = str(arc.get("account_fee_status") or "unknown")
        is_verified = status == "verified_account_tier"
        v_crypto, v_rwa, v_pm = _parse_verified_domain_overrides(arc, "arcus")
        if is_verified and (v_crypto is not None or v_rwa is not None or v_pm):
            if "ANT-USD" in v_pm:
                ant_mk, ant_tk = v_pm["ANT-USD"]
                out.append(
                    VenueFeeProfile(
                        key="arcus_ant_verified",
                        venue="arcus",
                        scope=f"ANT-USD (verified custom schedule: {ant_mk:g} / {ant_tk:g} bps)",
                        maker_fee_bps=ant_mk,
                        taker_fee_bps=ant_tk,
                        account_fee_status="verified_account_tier",
                        eligible_verified=True,
                    )
                )
            if v_rwa is not None:
                out.append(
                    VenueFeeProfile(
                        key="arcus_rwa_verified",
                        venue="arcus",
                        scope=f"RWA (verified account tier: {v_rwa[0]:g} / {v_rwa[1]:g} bps)",
                        maker_fee_bps=v_rwa[0],
                        taker_fee_bps=v_rwa[1],
                        account_fee_status="verified_account_tier",
                        eligible_verified=True,
                    )
                )
            if v_crypto is not None:
                out.append(
                    VenueFeeProfile(
                        key="arcus_crypto_verified",
                        venue="arcus",
                        scope=f"Crypto/default (verified account tier: {v_crypto[0]:g} / {v_crypto[1]:g} bps)",
                        maker_fee_bps=v_crypto[0],
                        taker_fee_bps=v_crypto[1],
                        account_fee_status="verified_account_tier",
                        eligible_verified=True,
                    )
                )
        else:
            pub = arc.get("public_tier0_schedule_summary") or {}
            tiers = pub.get("tier0_fee_profiles") or []
            custom = pub.get("custom_schedule_tier0_bps") or {}
            pub_status = "public_tier0_schedule_only" if is_verified else status
            if "ANT-USD" in custom:
                ant = custom["ANT-USD"]
                out.append(
                    VenueFeeProfile(
                        key="arcus_ant_tier0",
                        venue="arcus",
                        scope="ANT-USD (public tier0 custom schedule)",
                        maker_fee_bps=_require_finite(ant["maker_fee_bps"], "arcus.ANT.maker"),
                        taker_fee_bps=_require_finite(ant["taker_fee_bps"], "arcus.ANT.taker"),
                        account_fee_status=pub_status,
                    )
                )
            rwa_tier = _find_tier0_profile(tiers, maker_bps=0.0, taker_bps=2.25)
            if rwa_tier is not None:
                out.append(
                    VenueFeeProfile(
                        key="arcus_rwa_tier0",
                        venue="arcus",
                        scope="RWA custom tier0 (0 / 2.25 bps)",
                        maker_fee_bps=float(rwa_tier["maker_fee_bps"]),
                        taker_fee_bps=float(rwa_tier["taker_fee_bps"]),
                        account_fee_status=pub_status,
                    )
                )
            crypto_tier = _find_tier0_profile(tiers, maker_bps=1.5, taker_bps=4.5)
            if crypto_tier is not None:
                out.append(
                    VenueFeeProfile(
                        key="arcus_crypto_tier0",
                        venue="arcus",
                        scope="Crypto/default tier0 (1.5 / 4.5 bps)",
                        maker_fee_bps=float(crypto_tier["maker_fee_bps"]),
                        taker_fee_bps=float(crypto_tier["taker_fee_bps"]),
                        account_fee_status=pub_status,
                    )
                )

    if "trade_xyz" in venues:
        xyz = venues["trade_xyz"]
        status = str(xyz.get("account_fee_status") or "unknown")
        is_verified = status == "verified_account_tier"
        if xyz.get("standard_maker_fee_bps") is not None and xyz.get("standard_taker_fee_bps") is not None:
            out.append(
                VenueFeeProfile(
                    key="trade_xyz_standard",
                    venue="trade_xyz",
                    scope="HIP-3 standard 2x (3.0 / 9.0 bps)",
                    maker_fee_bps=_require_finite(xyz["standard_maker_fee_bps"], "xyz.std_maker"),
                    taker_fee_bps=_require_finite(xyz["standard_taker_fee_bps"], "xyz.std_taker"),
                    account_fee_status=status,
                    eligible_verified=is_verified,
                )
            )
        if xyz.get("growth_mode_maker_fee_bps") is not None and xyz.get("growth_mode_taker_fee_bps") is not None:
            out.append(
                VenueFeeProfile(
                    key="trade_xyz_growth",
                    venue="trade_xyz",
                    scope="HIP-3 growthMode 0.1x (0.3 / 0.9 bps)",
                    maker_fee_bps=_require_finite(xyz["growth_mode_maker_fee_bps"], "xyz.gr_maker"),
                    taker_fee_bps=_require_finite(xyz["growth_mode_taker_fee_bps"], "xyz.gr_taker"),
                    account_fee_status=status,
                    eligible_verified=is_verified,
                )
            )

    if "extended" in venues:
        ext = venues["extended"]
        if str(ext.get("account_fee_status") or "unknown") == "verified_account_tier":
            ext_crypto, ext_rwa, _ = _parse_verified_domain_overrides(ext, "extended")
            ext_pair = _confirmed_account_pair(ext, "extended")
            if ext_rwa is not None:
                out.append(
                    VenueFeeProfile(
                        key="extended_rwa_verified",
                        venue="extended",
                        scope=f"RWA (verified account tier: {ext_rwa[0]:g} / {ext_rwa[1]:g} bps)",
                        maker_fee_bps=ext_rwa[0],
                        taker_fee_bps=ext_rwa[1],
                        account_fee_status="verified_account_tier",
                        eligible_verified=True,
                    )
                )
            if ext_crypto is not None:
                out.append(
                    VenueFeeProfile(
                        key="extended_crypto_verified",
                        venue="extended",
                        scope=f"Crypto (verified account tier: {ext_crypto[0]:g} / {ext_crypto[1]:g} bps)",
                        maker_fee_bps=ext_crypto[0],
                        taker_fee_bps=ext_crypto[1],
                        account_fee_status="verified_account_tier",
                        eligible_verified=True,
                    )
                )
            elif ext_rwa is None and ext_pair is not None:
                out.append(
                    VenueFeeProfile(
                        key="extended_verified",
                        venue="extended",
                        scope=f"Verified account ({ext_pair[0]:g} / {ext_pair[1]:g} bps)",
                        maker_fee_bps=ext_pair[0],
                        taker_fee_bps=ext_pair[1],
                        account_fee_status="verified_account_tier",
                        eligible_verified=True,
                    )
                )

    for vname in ("hyperliquid", "backpack", "okx", "binance"):
        if vname not in venues:
            continue
        pair = _confirmed_account_pair(venues[vname], vname)
        if pair is not None:
            out.append(
                VenueFeeProfile(
                    key=f"{vname}_verified",
                    venue=vname,
                    scope=f"Verified account ({pair[0]:g} / {pair[1]:g} bps)",
                    maker_fee_bps=pair[0],
                    taker_fee_bps=pair[1],
                    account_fee_status="verified_account_tier",
                    eligible_verified=True,
                )
            )

    for lt_venue, pub_key in (
        ("lighter", "lighter_public_tier0"),
        ("lighter_robinhood", "lighter_robinhood_public_tier0"),
    ):
        if lt_venue not in venues:
            continue
        lt = venues[lt_venue]
        lt_pair = _confirmed_account_pair(lt, lt_venue)
        if lt_pair is not None:
            out.append(
                VenueFeeProfile(
                    key=f"{lt_venue}_verified",
                    venue=lt_venue,
                    scope=f"Verified account tier ({lt_pair[0]:g} / {lt_pair[1]:g} bps)",
                    maker_fee_bps=lt_pair[0],
                    taker_fee_bps=lt_pair[1],
                    account_fee_status="verified_account_tier",
                    eligible_verified=True,
                )
            )
        elif (
            lt.get("public_schedule_maker_fee_bps") is not None
            and lt.get("public_schedule_taker_fee_bps") is not None
        ):
            pub_mk = _require_finite(lt["public_schedule_maker_fee_bps"], f"{lt_venue}.pub_maker")
            pub_tk = _require_finite(lt["public_schedule_taker_fee_bps"], f"{lt_venue}.pub_taker")
            lt_status = str(lt.get("account_fee_status") or "public_schedule_only")
            if lt_status == "verified_account_tier":
                lt_status = "public_schedule_only_unverified_account_tier"
            out.append(
                VenueFeeProfile(
                    key=pub_key,
                    venue=lt_venue,
                    scope=f"Public standard_type_0 unverified ({pub_mk:g} / {pub_tk:g} bps)",
                    maker_fee_bps=pub_mk,
                    taker_fee_bps=pub_tk,
                    account_fee_status=lt_status,
                    note="Unverified account tier: requires signed /api/v1/accountLimits.",
                )
            )

    if not out:
        raise ValueError(f"No usable venue fee profiles parsed from {evidence_path}")
    return out


def infer_taker_exit_share(
    residual_cost_bps: float,
    *,
    sim_maker_fee_bps: float = 0.0,
    sim_taker_fee_bps: float = 3.5,
) -> tuple[float, bool]:
    """Invert `fee_bps = maker * (2 - s) + taker * s` when `maker != taker`.

    Returns `(clamped_s, out_of_bounds_warning)`. If `residual_cost_bps` includes non-zero
    funding or falls outside `[2 * maker, maker + taker]`, the warning flag is `True`.
    """
    res = _require_finite(residual_cost_bps, "residual_cost_bps")
    m = _require_finite(sim_maker_fee_bps, "sim_maker_fee_bps")
    t = _require_finite(sim_taker_fee_bps, "sim_taker_fee_bps")
    denom = t - m
    if denom <= 0.0:
        raise ValueError(f"Expected sim_taker_fee_bps > sim_maker_fee_bps, got {t} <= {m}")
    raw_s = (res - 2.0 * m) / denom
    out_of_bounds = raw_s < -1e-6 or raw_s > 1.0 + 1e-6
    return min(1.0, max(0.0, raw_s)), out_of_bounds


def evaluate_bookdiff_arms(
    bookdiff_path: Path,
    fee_profiles: Sequence[VenueFeeProfile],
    *,
    sim_maker_fee_bps: float = 0.0,
    sim_taker_fee_bps: float = 3.5,
    rebate_bps: float = 0.0,
    cont_only: bool = True,
) -> dict[str, Any]:
    """Evaluate `p1_pipeline_bookdiff_*.json` arms across audited venue fee schedules."""
    raw = json.loads(bookdiff_path.read_text(encoding="utf-8"))
    rows = raw.get("pipeline")
    if not isinstance(rows, list):
        raise ValueError(f"Expected list 'pipeline' in {bookdiff_path}")

    agg: dict[str, dict[str, float]] = defaultdict(
        lambda: {"rows": 0.0, "net": 0.0, "spr": 0.0, "drft": 0.0, "pex": 0.0, "tslp": 0.0, "ntl": 0.0}
    )
    for idx, row in enumerate(rows):
        if cont_only and not bool(row.get("cont", False)):
            continue
        arm = str(row.get("arm") or f"row_{idx}")
        a = agg[arm]
        a["rows"] += 1.0
        for fld in ("net", "spr", "drft", "pex", "tslp", "ntl"):
            a[fld] += _require_finite(row.get(fld), f"pipeline[{idx}].{fld}")

    arms_out: dict[str, Any] = {}
    for arm, a in sorted(agg.items()):
        ntl = a["ntl"]
        if ntl <= 0.0:
            raise ValueError(f"Arm {arm} has non-positive notional {ntl}")
        gross_usd = a["spr"] + a["drft"] + a["pex"] + a["tslp"]
        gross_bp = (gross_usd / ntl) * 1e4
        net_bp = (a["net"] / ntl) * 1e4
        # Note: p1_pipeline_bookdiff omits separate `funding_cash_usd` & `paired_cycle_fees_usd`.
        residual_cost_bp = ((gross_usd - a["net"]) / ntl) * 1e4
        s_est, s_oob = infer_taker_exit_share(
            residual_cost_bp,
            sim_maker_fee_bps=sim_maker_fee_bps,
            sim_taker_fee_bps=sim_taker_fee_bps,
        )
        venues_scored = {}
        for vp in fee_profiles:
            bd = compute_volume_farming_cost(
                gross_ex_fee_bps=gross_bp,
                taker_exit_share_s=s_est,
                taker_fee_bps=vp.taker_fee_bps,
                maker_fee_bps=vp.maker_fee_bps,
                rebate_bps=rebate_bps,
                funding_cash_bps=0.0,
                qualified_volume_ratio=None,
            )
            venues_scored[vp.key] = {
                "venue": vp.venue,
                "scope": vp.scope,
                "account_fee_status": vp.account_fee_status,
                **asdict(bd),
            }
        arms_out[arm] = {
            "rows": int(a["rows"]),
            "exit_notional_single_sided_usd": round(ntl, 2),
            "simulated_net_usd": round(a["net"], 4),
            "simulated_net_bp_per_exit_ntl": round(net_bp, 4),
            "gross_ex_fee_bps": round(gross_bp, 4),
            "residual_less_gross_bps": round(residual_cost_bp, 4),
            "inferred_taker_exit_share_s": round(s_est, 6),
            "s_out_of_bounds_warning": s_oob,
            "funding_and_fees_separated": False,
            "venues": venues_scored,
        }

    return {
        "source_path": str(bookdiff_path),
        "cont_only": cont_only,
        "assumed_sim_maker_fee_bps": sim_maker_fee_bps,
        "assumed_sim_taker_fee_bps": sim_taker_fee_bps,
        "rebate_bps": rebate_bps,
        "arms": arms_out,
    }


def summarize_multivenue_track3(mv_paths: Sequence[Path]) -> dict[str, Any]:
    """Summarize existing 10.25h Track 03 post-trade RWA markout & $/M evidence on disk."""
    windows: dict[str, Any] = {}
    for path in mv_paths:
        data = json.loads(path.read_text(encoding="utf-8"))
        split = str(data.get("split") or path.stem)
        t3 = data.get("track3_incentive_turnover") or []
        t1 = data.get("track1_quant_profit") or []
        t3_symbols = {(r.get("venue"), r.get("symbol")) for r in t3 if isinstance(r, dict)}
        dram_in_t1 = [
            {
                "venue": r.get("venue"),
                "symbol": r.get("symbol"),
                "half_spread_minus_fee_bps": r.get("half_spread_minus_fee_bps"),
                "median_l1_min_usd": r.get("median_l1_min_usd"),
            }
            for r in t1
            if isinstance(r, dict) and r.get("symbol") == "DRAM"
        ]
        entries = []
        for row in t3:
            if not isinstance(row, dict):
                continue
            venue = str(row.get("venue") or "")
            sym = str(row.get("symbol") or "")
            if sym in ("BTC", "ETH") or venue not in ("arcus", "lighter_robinhood", "lighter"):
                continue
            cont = row.get("continuity") or {}
            arms = []
            for a in row.get("arms") or []:
                if not isinstance(a, dict):
                    continue
                if a.get("arm") not in (
                    "A_baseline_back_of_queue",
                    "E_v3_validated_lead_qprio",
                    "F_v4_adaptive_shield",
                ):
                    continue
                arms.append(
                    {
                        "arm": a.get("arm"),
                        "fill_evidence_mode": a.get("fill_evidence_mode"),
                        "inferred_book_diff_sweeps": a.get("inferred_book_diff_sweeps"),
                        "fills": a.get("fills"),
                        "maker_volume_usd": a.get("maker_volume_usd"),
                        "spread_capture_bps": a.get("spread_capture_bps"),
                        "markouts": a.get("markouts"),
                        "equity_usd": a.get("equity_usd"),
                        "cost_per_m_usd": a.get("cost_per_m_usd"),
                        "combined_cost_per_m_usd": a.get("combined_cost_per_m_usd"),
                    }
                )
            entries.append(
                {
                    "venue": venue,
                    "symbol": sym,
                    "hours": cont.get("hours") if isinstance(cont, dict) else None,
                    "maker_fee_bps": row.get("maker_fee_bps"),
                    "fee_audit_status": row.get("fee_audit_status"),
                    "arms": arms,
                }
            )
        windows[split] = {
            "source_path": str(path),
            "rwa_track3_entries": entries,
            "dram_present_in_track3": any(s == "DRAM" for _, s in t3_symbols),
            "dram_static_track1": dram_in_t1,
        }
    return {"windows": windows}


def run_self_test() -> dict[str, Any]:
    """Verify exact identities and unit conversions with synthetic fixtures (zero external files)."""
    # 1. Zero maker fee + zero rebate reproduces (taker * s - gross) / 2 * 100
    c1 = compute_volume_farming_cost(-0.4057, 0.1845, 1.12, maker_fee_bps=0.0, rebate_bps=0.0)
    assert math.isclose(c1.double_sided_usd_per_m, (1.12 * 0.1845 + 0.4057) / 2.0 * 100.0, rel_tol=1e-9)
    assert c1.qualified_usd_per_m is None

    # 2. Verified HL Tier 0 MUST include maker_fee_bps = 1.5 across (2 - s) maker legs
    c_hl = compute_volume_farming_cost(-0.4057, 0.1845, 4.5, maker_fee_bps=1.5)
    expected_hl = (1.5 * (2.0 - 0.1845) + 4.5 * 0.1845 + 0.4057) / 2.0 * 100.0
    assert math.isclose(c_hl.double_sided_usd_per_m, expected_hl, abs_tol=1e-4)
    assert c_hl.s_zero_double_sided_floor_usd_per_m > 170.0

    # 3. 100% passive with equal fee=0 yields maker-only == double-sided
    c_pas = compute_volume_farming_cost(0.5, 0.0, 2.25, maker_fee_bps=0.0)
    assert math.isclose(c_pas.double_sided_usd_per_m, -25.0, abs_tol=1e-9)
    assert math.isclose(c_pas.maker_volume_usd_per_m, -25.0, abs_tol=1e-9)

    return {
        "self_test": "ok",
        "c1_arcus_ant_usd_per_m": c1.double_sided_usd_per_m,
        "c_hl_verified_usd_per_m": c_hl.double_sided_usd_per_m,
        "c_passive_profit_usd_per_m": c_pas.double_sided_usd_per_m,
    }


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="Audited Scenario 1 volume-farming USD/$1M evaluator")
    ap.add_argument("--fee-evidence", type=Path, help="Path to account_fee_evidence_20261123.json")
    ap.add_argument("--bookdiff", type=Path, help="Path to p1_pipeline_bookdiff_*.json")
    ap.add_argument("--multivenue", type=Path, nargs="*", help="Paths to multivenue_three_tracks_*.json")
    ap.add_argument("--rebate-bps", type=float, default=0.0, help="Explicit maker rebate assumption (default 0.0)")
    ap.add_argument("--sim-maker-fee-bps", type=float, default=0.0)
    ap.add_argument("--sim-taker-fee-bps", type=float, default=3.5)
    ap.add_argument("--self-test", action="store_true", help="Run built-in algebraic invariant checks")
    args = ap.parse_args(argv)

    if args.self_test:
        print(json.dumps(run_self_test(), indent=2))
        return 0

    if not args.fee_evidence or not args.bookdiff:
        ap.error("--fee-evidence and --bookdiff are required unless --self-test is set")

    profiles = load_fee_profiles_from_evidence(args.fee_evidence)
    out: dict[str, Any] = {
        "schema": "volume_farming_cost_audit_v1",
        "venue_capability_snapshot_version": VENUE_CAPABILITY_SNAPSHOT_VERSION,
        "fee_profiles": [asdict(p) for p in profiles],
        "bookdiff": evaluate_bookdiff_arms(
            args.bookdiff,
            profiles,
            sim_maker_fee_bps=args.sim_maker_fee_bps,
            sim_taker_fee_bps=args.sim_taker_fee_bps,
            rebate_bps=args.rebate_bps,
        ),
    }
    if args.multivenue:
        out["multivenue_track3_rwa"] = summarize_multivenue_track3(args.multivenue)
    print(json.dumps(out, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
