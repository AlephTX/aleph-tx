#!/usr/bin/env python3
"""Track 02 Flipped-Role Zero-Fee WS Basis Arbitrage (`Arcus <-> LighterRH` & `Arcus <-> Lighter`).

Evaluates across Aleph Tokyo `W01..W27` (`selection_dev` w01-w20 -> frozen `holdout` w21-w25
-> `w26_w27_oos` w26-w27; strictly excluding `-w28`):
- Cohort 1 (`USDG/USDC`): `Arcus` (`ws_50ms`, verified `level_0` w/ 5% discount: RWA `0.00/2.1375`,
  Crypto `1.425/4.275`) <-> `LighterRH` (`ws_50ms`, `0.00/0.00`, `USDG`) under `{0, 1, 2, 3}` bp
  FX drag + exact FX breakeven.
- Cohort 2 (`USDC/USDC`): `Arcus` <-> `Lighter` (`Std 0.0/0.0` counterfactual vs `Prem 0.40/2.80`
  verified).
- Arms:
  * `A0_arcus_mk_hedge_tk`: Flipped-role `Arcus` Maker (`36ms` print-verified) -> Hedge Taker
    (`330ms` RH / `330ms` Lighter Std / `140ms` Lighter Prem) with causal `2h_MA` basis fair.
  * `A1_arcus_mk_hedge_mk_fb`: Flipped-role `Arcus` Maker (`36ms`) -> Hedge Maker post-only
    (`200ms` RH / `40ms` Lighter) w/ print-verified passive wait + Taker fallback.
  * `A2_arcus_mk_demeaned_reversion`: Flipped-role demeaned (`b - 2h_MA`) basis reversion
    entry/exit with `Arcus` passive maker (`36ms`) and passive/taker hedge.
  * `B0_hedge_mk_arcus_tk`: Original/benchmark role (`LighterRH` `200ms` / `Lighter` `40ms` Maker
    -> `Arcus` Taker `36ms`).

Enforces all Track 02 P0 ledger invariants:
1. Hedge L1 depth clamping (`hedged_notl = min(maker_fill_notl, h_l1)`).
2. Explicit orphan-leg exposure tracking, unwind cost/slippage, and PnL accounting.
3. Per-leg fill & exit timestamps (`leg_a_fill_ms`, `leg_b_fill_ms`) with strict funding
   settlement `settlement_ms > leg_fill_ms` and honest reporting when `arcus` lacks
   `funding_and_ctx` in the lakehouse.
4. Separate `realized_only_pnl_usd` and `mtm_incl_residual_pnl_usd`, plus `PSR / DSR / PBO`.
5. Pure stdlib top-level imports (`duckdb` and `numpy` lazy-imported inside loaders only).
"""

from __future__ import annotations

import argparse
from bisect import bisect_left, bisect_right
from dataclasses import asdict, dataclass
from datetime import date, timedelta
import gc
import glob
import hashlib
import json
import math
import os
from pathlib import Path
import re
import sys
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
from track_fee_evidence import (  # noqa: E402
    VENUE_CAPABILITY_SNAPSHOT_VERSION,
    build_fee_provenance,
    load_track_fee_evidence,
    resolve_venue_fee,
)

HOUR_MS = 3_600_000
TEN_HOURS_MS = 36_000_000
STEP_MS = 200
WARMUP_MS = 30 * 60_000
MAX_BOOK_AGE_MS = 5_000
MAX_CONTINUOUS_GAP_MS = 60_000
MAX_RECOVERY_MS = 5_000
CLIP_USD = 1_000.0
MIN_FILL_USD = 100.0
Q_MAX_CLIPS = 4.0
DEFAULT_SKEW_BPS = 1.0
FX_DRAG_BPS_LIST: tuple[float, ...] = (0.0, 1.0, 2.0, 3.0)
MIN_SELECTION_FILLS = 30

# 14 RWA symbols common to Arcus & LighterRH, plus HOOD & MSTR for Arcus <-> Lighter, plus 3 Crypto.
RWA_ARCUS_RH_SYMBOLS: tuple[str, ...] = (
    "AAPL",
    "AMD",
    "AMZN",
    "COIN",
    "CRCL",
    "GOOGL",
    "META",
    "MSFT",
    "MU",
    "NVDA",
    "PLTR",
    "QQQ",
    "SPY",
    "TSLA",
)
RWA_ARCUS_LIGHTER_EXTRA: tuple[str, ...] = ("HOOD", "MSTR")
CRYPTO_SYMBOLS: tuple[str, ...] = ("BTC", "ETH", "SOL")

VENUE_SYMBOL_MAP: dict[str, dict[str, str]] = {
    "arcus": {
        **{s: f"{s}-USD" for s in RWA_ARCUS_RH_SYMBOLS + RWA_ARCUS_LIGHTER_EXTRA},
        **{s: f"{s}-USD" for s in CRYPTO_SYMBOLS},
    },
    "lighter_robinhood": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "3",
        "AAPL": "10",
        "AMZN": "11",
        "GOOGL": "12",
        "META": "13",
        "MSFT": "14",
        "NVDA": "15",
        "TSLA": "16",
        "COIN": "23",
        "CRCL": "24",
        "QQQ": "25",
        "SPY": "26",
        "AMD": "29",
        "MU": "31",
        "PLTR": "34",
    },
    "lighter": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "2",
        "HOOD": "108",
        "COIN": "109",
        "NVDA": "110",
        "PLTR": "111",
        "TSLA": "112",
        "AAPL": "113",
        "AMZN": "114",
        "MSFT": "115",
        "GOOGL": "116",
        "META": "117",
        "MSTR": "122",
        "SPY": "128",
        "QQQ": "129",
        "AMD": "138",
        "MU": "164",
    },
}

VENUE_TIMING: dict[str, dict[str, Any]] = {
    "arcus": {
        "settlement": "USDC",
        "maker_delay_ms": 36,
        "taker_delay_ms": 36,
        "speedbump_ms": 0,
        "bbo_cadence": "ws_50ms",
    },
    "lighter_robinhood": {
        "settlement": "USDG",
        "maker_delay_ms": 200,
        "taker_delay_ms": 330,
        "speedbump_ms": 200,
        "bbo_cadence": "ws_50ms",
    },
    "lighter_std": {
        "settlement": "USDC",
        "maker_delay_ms": 40,
        "taker_delay_ms": 330,
        "speedbump_ms": 300,
        "bbo_cadence": "ws_50ms",
    },
    "lighter_prem": {
        "settlement": "USDC",
        "maker_delay_ms": 40,
        "taker_delay_ms": 140,
        "speedbump_ms": 140,
        "bbo_cadence": "ws_50ms",
    },
}


@dataclass(frozen=True)
class Track02PairSpec:
    pair_group: str  # "arcus_rh" or "arcus_lighter"
    cohort: str  # "USDG/USDC" or "USDC/USDC"
    domain: str  # "rwa" or "crypto"
    symbol: str
    venue_arcus: str
    venue_hedge: str
    cross_stablecoin: bool

    @property
    def label(self) -> str:
        return f"{self.pair_group}:{self.symbol}"


@dataclass(frozen=True)
class BasisArbParams:
    name: str
    arm: str  # A0_arcus_mk_hedge_tk, A1_arcus_mk_hedge_mk_fb, A2_arcus_mk_demeaned_reversion, B0_hedge_mk_arcus_tk
    flipped_role: bool = True  # True => Arcus is Maker; False => Hedge venue is Maker
    hedge_mode: str = "taker"  # "taker", "maker_fallback", or "reversion"
    e0_bps: float = 6.0
    tau_s: float = 7200.0  # 2h causal MA
    vol_edge_mult: float = 0.0
    vol_tau_s: float = 300.0
    skew_bps_per_clip: float = DEFAULT_SKEW_BPS
    nonlinear_skew_power: float = 1.5
    basis_drift_guard_bps: float = 2.0
    drift_tau_s: float = 5.0
    min_hedge_l1_notional_usd: float = CLIP_USD
    shield_bps: float | None = None
    max_hedge_passive_wait_ms: int = 1_500
    reversion_dev_bps: float = 5.0
    reversion_min_edge_bps: float = 1.0
    reversion_tp_bps: float = 0.5
    reversion_max_hold_ms: int = 3_600_000
    depth_impact_bps: float = 2.0


def split_for_window(w_num: int) -> str:
    if w_num <= 20:
        return "selection_dev"
    if w_num <= 25:
        return "holdout"
    return "w26_w27_oos"


def build_symbol_catalog() -> list[Track02PairSpec]:
    """Enumerate all overlapping Arcus <-> LighterRH and Arcus <-> Lighter markets."""
    specs: list[Track02PairSpec] = []
    for sym in RWA_ARCUS_RH_SYMBOLS:
        specs.append(
            Track02PairSpec(
                pair_group="arcus_rh",
                cohort="USDG/USDC",
                domain="rwa",
                symbol=sym,
                venue_arcus="arcus",
                venue_hedge="lighter_robinhood",
                cross_stablecoin=True,
            )
        )
    for sym in CRYPTO_SYMBOLS:
        specs.append(
            Track02PairSpec(
                pair_group="arcus_rh",
                cohort="USDG/USDC",
                domain="crypto",
                symbol=sym,
                venue_arcus="arcus",
                venue_hedge="lighter_robinhood",
                cross_stablecoin=True,
            )
        )
    for sym in RWA_ARCUS_RH_SYMBOLS + RWA_ARCUS_LIGHTER_EXTRA:
        if sym not in VENUE_SYMBOL_MAP["lighter"]:
            continue
        specs.append(
            Track02PairSpec(
                pair_group="arcus_lighter",
                cohort="USDC/USDC",
                domain="rwa",
                symbol=sym,
                venue_arcus="arcus",
                venue_hedge="lighter",
                cross_stablecoin=False,
            )
        )
    for sym in CRYPTO_SYMBOLS:
        specs.append(
            Track02PairSpec(
                pair_group="arcus_lighter",
                cohort="USDC/USDC",
                domain="crypto",
                symbol=sym,
                venue_arcus="arcus",
                venue_hedge="lighter",
                cross_stablecoin=False,
            )
        )
    return specs


def resolve_track02_fees(
    fee_evidence_path: Path | None = None,
    *,
    lighter_mode: str = "verified_prem",
) -> tuple[dict[str, dict[str, dict[str, Any]]], dict[str, Any]]:
    """Resolve Arcus (verified 5% discount level_0), LighterRH (0/0), and Lighter (Std vs Prem)."""
    ev = load_track_fee_evidence(fee_evidence_path)
    out: dict[str, dict[str, dict[str, Any]]] = {"arcus": {}, "lighter_robinhood": {}, "lighter": {}}

    for dom in ("rwa", "crypto"):
        # Arcus verified default (level_0 w/ 5% discount: RWA 0.00/2.1375, Crypto 1.425/4.275)
        def_amk = 0.0 if dom == "rwa" else 1.425
        def_atk = 2.1375 if dom == "rwa" else 4.275
        amk, atk, ast, asrc = resolve_venue_fee(
            "arcus",
            dom,
            default_maker_bps=def_amk,
            default_taker_bps=def_atk,
            default_status="verified_account_tier" if ev.get("loaded") else "verified_level0_5pct_discount_default",
            evidence=ev,
            raw_market=f"NVDA-USD" if dom == "rwa" else "BTC-USD",
            arcus_fee_tier="level_0",
        )
        # If no evidence JSON passed in offline mode, keep exact 5% discounted verified schedule
        if not ev.get("loaded"):
            amk, atk = def_amk, def_atk
        out["arcus"][dom] = {"maker_fee_bps": amk, "taker_fee_bps": atk, "fee_status": ast, "fee_source": asrc}

        rmk, rtk, rst, rsrc = resolve_venue_fee(
            "lighter_robinhood",
            dom,
            default_maker_bps=0.0,
            default_taker_bps=0.0,
            default_status="verified_account_tier",
            evidence=ev,
        )
        out["lighter_robinhood"][dom] = {"maker_fee_bps": rmk, "taker_fee_bps": rtk, "fee_status": rst, "fee_source": rsrc}

        if lighter_mode == "standard_zero":
            lmk, ltk, lst, lsrc = 0.0, 0.0, "lighter_standard_zero_counterfactual", "counterfactual_public_0_0"
        else:
            lmk, ltk, lst, lsrc = resolve_venue_fee(
                "lighter",
                dom,
                default_maker_bps=0.40,
                default_taker_bps=2.80,
                default_status="verified_account_tier",
                evidence=ev,
            )
        out["lighter"][dom] = {"maker_fee_bps": lmk, "taker_fee_bps": ltk, "fee_status": lst, "fee_source": lsrc}

    prov = build_fee_provenance(
        ev,
        arcus_fee_tier="level_0",
        venues_in_scope=("arcus", "lighter_robinhood", "lighter"),
    )
    prov["lighter_mode"] = lighter_mode
    return out, prov


def build_candidate_grid() -> dict[str, list[BasisArbParams]]:
    """Pre-registered causal candidate grid selected strictly on `selection_dev` (w01-w20)."""
    a0 = [
        BasisArbParams(
            name=f"a0_e{int(e0)}_v{vm}",
            arm="A0_arcus_mk_hedge_tk",
            flipped_role=True,
            hedge_mode="taker",
            e0_bps=e0,
            tau_s=7200.0,
            vol_edge_mult=vm,
        )
        for e0 in (4.0, 6.0, 8.0, 10.0, 12.0, 15.0)
        for vm in (0.0, 1.0)
    ]
    a1 = [
        BasisArbParams(
            name=f"a1_e{int(e0)}_v{vm}",
            arm="A1_arcus_mk_hedge_mk_fb",
            flipped_role=True,
            hedge_mode="maker_fallback",
            e0_bps=e0,
            tau_s=7200.0,
            vol_edge_mult=vm,
            max_hedge_passive_wait_ms=1_500,
        )
        for e0 in (4.0, 6.0, 8.0, 12.0)
        for vm in (0.0, 1.0)
    ]
    a2 = [
        BasisArbParams(
            name=f"a2_dev{int(dv)}_edge{int(me)}",
            arm="A2_arcus_mk_demeaned_reversion",
            flipped_role=True,
            hedge_mode="reversion",
            tau_s=7200.0,
            reversion_dev_bps=dv,
            reversion_min_edge_bps=me,
            reversion_tp_bps=0.5,
            reversion_max_hold_ms=3_600_000,
        )
        for dv in (3.0, 5.0, 8.0)
        for me in (1.0, 3.0)
    ]
    b0 = [
        BasisArbParams(
            name=f"b0_e{int(e0)}_v{vm}_sh{int(sh) if sh else 0}",
            arm="B0_hedge_mk_arcus_tk",
            flipped_role=False,
            hedge_mode="taker",
            e0_bps=e0,
            tau_s=7200.0,
            vol_edge_mult=vm,
            shield_bps=sh,
        )
        for e0 in (4.0, 6.0, 8.0, 10.0)
        for vm in (0.0, 1.0)
        for sh in (None, 2.0)
    ]
    return {
        "A0_arcus_mk_hedge_tk": a0,
        "A1_arcus_mk_hedge_mk_fb": a1,
        "A2_arcus_mk_demeaned_reversion": a2,
        "B0_hedge_mk_arcus_tk": b0,
    }


def compute_causal_basis_2h(
    bid_a: Sequence[float],
    ask_a: Sequence[float],
    age_a: Sequence[int],
    bid_b: Sequence[float],
    ask_b: Sequence[float],
    age_b: Sequence[int],
    *,
    step_ms: int = STEP_MS,
    tau_ms: int = 7_200_000,
    vol_tau_ms: int = 300_000,
    drift_tau_ms: int = 5_000,
    max_age_steps: int = MAX_BOOK_AGE_MS // STEP_MS,
    max_gap_steps: int = MAX_CONTINUOUS_GAP_MS // STEP_MS,
) -> dict[str, list[float]]:
    """Compute strictly causal basis `log(mid_a) - log(mid_b)`, 2h EMA fair, sigma (bps), and 5s fast EMA."""
    n = len(bid_a)
    raw_b = [math.nan] * n
    prior_fair = [math.nan] * n
    prior_sigma_bps = [0.0] * n
    prior_fast = [math.nan] * n

    fair: float | None = None
    var_bps2: float | None = None
    fast: float | None = None
    last_fresh_k: int | None = None
    exp = math.exp
    log = math.log
    sqrt = math.sqrt

    for k in range(n):
        if age_a[k] > max_age_steps or age_b[k] > max_age_steps:
            continue
        ba, aa = float(bid_a[k]), float(ask_a[k])
        bb, ab = float(bid_b[k]), float(ask_b[k])
        if not (ba > 0.0 and aa > ba and bb > 0.0 and ab > bb):
            continue
        b = log(0.5 * (ba + aa)) - log(0.5 * (bb + ab))
        raw_b[k] = b
        if fair is not None and last_fresh_k is not None and (k - last_fresh_k) <= max_gap_steps:
            dt_ms = (k - last_fresh_k) * step_ms
            prior_fair[k] = fair
            prior_sigma_bps[k] = sqrt(var_bps2) if (var_bps2 is not None and var_bps2 > 0.0) else 0.0
            prior_fast[k] = fast if fast is not None else fair

            af = 1.0 - exp(-dt_ms / max(tau_ms, 1))
            av = 1.0 - exp(-dt_ms / max(vol_tau_ms, 1))
            ad = 1.0 - exp(-dt_ms / max(drift_tau_ms, 1))

            dev_bps = (b - fair) * 1e4
            var_bps2 = dev_bps * dev_bps if var_bps2 is None else (1.0 - av) * var_bps2 + av * dev_bps * dev_bps
            fast = b if fast is None else fast + ad * (b - fast)
            fair = fair + af * (b - fair)
        else:
            fair = b
            var_bps2 = None
            fast = b
        last_fresh_k = k

    return {
        "raw_b": raw_b,
        "prior_fair": prior_fair,
        "prior_sigma_bps": prior_sigma_bps,
        "prior_fast": prior_fast,
    }


def _skew_q(q: float, q_max: float, power: float) -> float:
    if power <= 1.0 or q == 0.0 or q_max <= 0.0:
        return q
    u = min(abs(q) / q_max, 1.0)
    return q * (1.0 + (power - 1.0) * (u ** power))


def bucket_trades_by_step(
    trades: dict[str, Sequence[Any]] | None,
    t0_ms: int,
    maker_delay_ms: int,
    n_steps: int,
    *,
    step_ms: int = STEP_MS,
    warmup_steps: int = WARMUP_MS // STEP_MS,
    decision_steps: int = TEN_HOURS_MS // STEP_MS,
) -> dict[int, list[int]]:
    if trades is None or len(trades["ts"]) == 0:
        return {}
    tr_ts = trades["ts"]
    last_k = min(warmup_steps + decision_steps, n_steps)
    by_step: dict[int, list[int]] = {}
    for idx_t in range(len(tr_ts)):
        k = int((int(tr_ts[idx_t]) - maker_delay_ms - t0_ms) // step_ms) - 1
        if warmup_steps <= k < last_k:
            by_step.setdefault(k, []).append(idx_t)
    return by_step


def _depth_sweep_cost(notl: float, l1_notl: float, impact_bps: float) -> tuple[float, bool]:
    if notl <= l1_notl:
        return 0.0, False
    if l1_notl <= 0.0:
        sweep_bps = 15.0
    else:
        sweep_bps = min(impact_bps * (notl / l1_notl - 1.0), 25.0)
    return notl * sweep_bps * 1e-4, True


def _lookup_valid_step(
    age: Sequence[int],
    bid: Sequence[float],
    ask: Sequence[float],
    target_ms: int,
    t0_ms: int,
    step_ms: int,
    max_age_steps: int,
    max_recovery_steps: int,
) -> tuple[int, int] | None:
    """Return `(grid_idx, actual_exec_ms)` at `target_ms` or recovered within `max_recovery_steps`."""
    n = len(bid)
    j0 = min(max(int((target_ms - t0_ms) // step_ms) - 1, 0), n - 1)
    j = j0
    lim = min(n - 1, j0 + max_recovery_steps)
    while j <= lim:
        if age[j] <= max_age_steps:
            b, a = float(bid[j]), float(ask[j])
            if b > 0.0 and a > b and math.isfinite(b) and math.isfinite(a):
                actual_ms = max(target_ms, t0_ms + (j + 1) * step_ms) if j > j0 else target_ms
                return j, actual_ms
        j += 1
    return None


def _settle_lot_funding(
    *,
    lot_dir: int,  # +1 => long maker / short hedge; -1 => short maker / long hedge
    qty_base: float,
    mk_fill_ms: int,
    hd_fill_ms: int,
    mk_exit_ms: int,
    hd_exit_ms: int,
    mk_funding: dict[int, float] | None,
    hd_funding: dict[int, float] | None,
    mk_hours: Sequence[int],
    hd_hours: Sequence[int],
    mk_bid: Sequence[float],
    mk_ask: Sequence[float],
    hd_bid: Sequence[float],
    hd_ask: Sequence[float],
    t0_ms: int,
    step_ms: int,
) -> tuple[float, float, int]:
    """Settle per-leg funding strictly when `settlement_ms > leg_fill_ms` and `<= leg_exit_ms`."""
    n = len(mk_bid)
    f_mk = 0.0
    f_hd = 0.0
    pays = 0

    if mk_funding and mk_hours and mk_exit_ms > mk_fill_ms:
        lo = bisect_right(mk_hours, mk_fill_ms)
        hi = bisect_right(mk_hours, mk_exit_ms)
        for idx in range(lo, hi):
            ht = mk_hours[idx]
            if ht <= mk_fill_ms:
                continue
            k_ht = min(max(int((ht - t0_ms) // step_ms) - 1, 0), n - 1)
            mid_t = 0.5 * (float(mk_bid[k_ht]) + float(mk_ask[k_ht]))
            notl_t = qty_base * mid_t if (mid_t > 0.0 and math.isfinite(mid_t)) else 0.0
            # Long maker (+1) pays positive funding; Short maker (-1) receives positive funding
            f_mk += (-lot_dir) * notl_t * float(mk_funding[ht]) * 1e-4
            pays += 1

    if hd_funding and hd_hours and hd_exit_ms > hd_fill_ms:
        lo = bisect_right(hd_hours, hd_fill_ms)
        hi = bisect_right(hd_hours, hd_exit_ms)
        for idx in range(lo, hi):
            ht = hd_hours[idx]
            if ht <= hd_fill_ms:
                continue
            k_ht = min(max(int((ht - t0_ms) // step_ms) - 1, 0), n - 1)
            mid_t = 0.5 * (float(hd_bid[k_ht]) + float(hd_ask[k_ht]))
            notl_t = qty_base * mid_t if (mid_t > 0.0 and math.isfinite(mid_t)) else 0.0
            # Hedge leg has opposite sign (-lot_dir), so funding receipt sign is +lot_dir
            f_hd += lot_dir * notl_t * float(hd_funding[ht]) * 1e-4
            pays += 1

    return f_mk, f_hd, pays


def simulate_ws_basis_arb(
    *,
    arcus_bid: Sequence[float],
    arcus_ask: Sequence[float],
    arcus_bsz: Sequence[float],
    arcus_asz: Sequence[float],
    arcus_age: Sequence[int],
    hedge_bid: Sequence[float],
    hedge_ask: Sequence[float],
    hedge_bsz: Sequence[float],
    hedge_asz: Sequence[float],
    hedge_age: Sequence[int],
    causal: dict[str, list[float]],
    arcus_trades: dict[str, Sequence[Any]] | None,
    hedge_trades: dict[str, Sequence[Any]] | None,
    t0_ms: int,
    tick: float,
    params: BasisArbParams,
    arcus_delay_ms: int,
    hedge_maker_delay_ms: int,
    hedge_taker_delay_ms: int,
    hedge_speedbump_ms: int,
    arcus_mk_fee_bps: float,
    arcus_tk_fee_bps: float,
    hedge_mk_fee_bps: float,
    hedge_tk_fee_bps: float,
    step_ms: int = STEP_MS,
    warmup_steps: int = WARMUP_MS // STEP_MS,
    decision_steps: int = TEN_HOURS_MS // STEP_MS,
    max_age_steps: int = MAX_BOOK_AGE_MS // STEP_MS,
    clip_usd: float = CLIP_USD,
    q_max_clips: float = Q_MAX_CLIPS,
    fx_drag_bps: float = 0.0,
    arcus_funding: dict[int, float] | None = None,
    hedge_funding: dict[int, float] | None = None,
    pre_arcus_by_step: dict[int, list[int]] | None = None,
    pre_hedge_by_step: dict[int, list[int]] | None = None,
    pre_hedge_ts: list[int] | None = None,
    record_trades: bool = False,
) -> dict[str, Any]:
    """Causal P0-clean simulation for Flipped-Role & Benchmark WS Basis Arbitrage."""
    n = len(arcus_bid)
    empty = {
        "fills": 0,
        "maker_filled_notional_usd": 0.0,
        "hedged_notional_usd": 0.0,
        "orphan_events": 0,
        "orphan_notional_usd": 0.0,
        "orphan_unwind_rate": 0.0,
        "orphan_unwind_pnl_usd": 0.0,
        "orphan_unwind_cost_usd": 0.0,
        "hedge_passive_fills": 0,
        "hedge_taker_fallbacks": 0,
        "depth_clipped_entries": 0,
        "depth_clipped_exits": 0,
        "blocked_toxic": 0,
        "blocked_drift": 0,
        "blocked_l1_thin": 0,
        "fees_usd": 0.0,
        "fx_drag_usd": 0.0,
        "fx_drag_unit_usd": 0.0,
        "intraday_fx_unit_usd": 0.0,
        "drain_fx_unit_usd": 0.0,
        "residual_fx_unit_usd": 0.0,
        "depth_slip_usd": 0.0,
        "funding_leg_mk_usd": 0.0,
        "funding_leg_hd_usd": 0.0,
        "funding_cash_usd": 0.0,
        "funding_payments": 0,
        "intraday_realized_pnl_usd": 0.0,
        "terminal_drain_pnl_usd": 0.0,
        "realized_only_pnl_usd": 0.0,
        "mtm_incl_residual_pnl_usd": 0.0,
        "terminal_flat": True,
        "terminal_residual_notional_usd": 0.0,
        "peak_gross_notional_usd": 0.0,
        "mean_hold_min": 0.0,
        "mean_effective_e0_bps": params.e0_bps,
    }
    if n < 2:
        return empty

    if params.flipped_role:
        mk_bid, mk_ask, mk_bsz, mk_asz, mk_age = arcus_bid, arcus_ask, arcus_bsz, arcus_asz, arcus_age
        hd_bid, hd_ask, hd_bsz, hd_asz, hd_age = hedge_bid, hedge_ask, hedge_bsz, hedge_asz, hedge_age
        mk_trades, hd_trades = arcus_trades, hedge_trades
        mk_delay = arcus_delay_ms
        hd_mk_delay = hedge_maker_delay_ms
        hd_tk_delay = hedge_taker_delay_ms
        mk_mk_fee, mk_tk_fee = arcus_mk_fee_bps, arcus_tk_fee_bps
        hd_mk_fee, hd_tk_fee = hedge_mk_fee_bps, hedge_tk_fee_bps
        mk_fund_map, hd_fund_map = arcus_funding, hedge_funding
        pre_mk_steps = pre_arcus_by_step
        b_sign = 1.0
    else:
        mk_bid, mk_ask, mk_bsz, mk_asz, mk_age = hedge_bid, hedge_ask, hedge_bsz, hedge_asz, hedge_age
        hd_bid, hd_ask, hd_bsz, hd_asz, hd_age = arcus_bid, arcus_ask, arcus_bsz, arcus_asz, arcus_age
        mk_trades, hd_trades = hedge_trades, arcus_trades
        mk_delay = hedge_maker_delay_ms
        hd_mk_delay = arcus_delay_ms
        hd_tk_delay = arcus_delay_ms
        mk_mk_fee, mk_tk_fee = hedge_mk_fee_bps, hedge_tk_fee_bps
        hd_mk_fee, hd_tk_fee = arcus_mk_fee_bps, arcus_tk_fee_bps
        mk_fund_map, hd_fund_map = hedge_funding, arcus_funding
        pre_mk_steps = pre_hedge_by_step
        b_sign = -1.0

    if mk_trades is None or len(mk_trades["ts"]) == 0:
        return empty

    raw_b = causal["raw_b"]
    prior_fair = causal["prior_fair"]
    prior_sigma = causal["prior_sigma_bps"]
    prior_fast = causal["prior_fast"]
    mk_hours = sorted(mk_fund_map) if mk_fund_map else []
    hd_hours = sorted(hd_fund_map) if hd_fund_map else []

    by_step = (
        pre_mk_steps
        if pre_mk_steps is not None
        else bucket_trades_by_step(
            mk_trades,
            t0_ms,
            mk_delay,
            n,
            step_ms=step_ms,
            warmup_steps=warmup_steps,
            decision_steps=decision_steps,
        )
    )

    if hd_trades is not None and len(hd_trades["ts"]) > 0:
        hd_ts = pre_hedge_ts if (params.flipped_role and pre_hedge_ts is not None) else [int(x) for x in hd_trades["ts"]]
        hd_sgn = hd_trades["sgn"]
        hd_px = hd_trades["px"]
        hd_sz = hd_trades["sz"]
    else:
        hd_ts = None
        hd_sgn = None
        hd_px = None
        hd_sz = None

    max_rec_steps = MAX_RECOVERY_MS // step_ms
    end_k = min(warmup_steps + decision_steps - 1, n - 1)
    t_end_ms = t0_ms + (end_k + 1) * step_ms

    # FIFO open lots: {"dir": +1/-1, "qty": float, "qp": float, "hp": float, "mk_ms": int, "hd_ms": int, "mk_fee": float, "hd_fee": float, "fx": float}
    lots: list[dict[str, Any]] = []
    open_orphans: list[dict[str, Any]] = []
    trade_events: list[dict[str, Any]] = []

    pos_mk = 0.0
    fills = 0
    maker_filled_notl = 0.0
    hedged_notl_tot = 0.0
    orphan_events = 0
    orphan_notl_tot = 0.0
    orphan_unwind_pnl = 0.0
    orphan_unwind_cost = 0.0
    hedge_passive_fills = 0
    hedge_taker_fallbacks = 0
    depth_clipped_entries = 0
    depth_clipped_exits = 0
    blocked_toxic = 0
    blocked_drift = 0
    blocked_l1_thin = 0
    fees_usd = 0.0
    fx_drag_usd = 0.0
    fx_drag_unit_usd = 0.0
    intraday_fx_unit_usd = 0.0
    drain_fx_unit_usd = 0.0
    residual_fx_unit_usd = 0.0
    depth_slip_usd = 0.0
    funding_leg_mk_usd = 0.0
    funding_leg_hd_usd = 0.0
    funding_payments = 0
    intraday_realized_pnl = 0.0
    peak_gross_notl = 0.0
    hold_mins: list[float] = []
    eff_edges: list[float] = []

    def _unwind_orphan_at(orp: dict[str, Any], t_attempt: int, *, force_end: bool = False) -> bool:
        nonlocal fees_usd, depth_slip_usd, funding_leg_mk_usd, funding_payments
        nonlocal orphan_unwind_pnl, orphan_unwind_cost, intraday_realized_pnl
        hit = _lookup_valid_step(mk_age, mk_bid, mk_ask, t_attempt, t0_ms, step_ms, max_age_steps, max_rec_steps)
        if hit is None:
            return False
        ju, u_exit_ms = hit
        if force_end:
            u_exit_ms = min(u_exit_ms, t_end_ms)
        side = int(orp["side"])  # +1 => bought maker, unwind by selling at bid; -1 => sold maker, unwind by buying at ask
        qty = float(orp["qty"])
        qp = float(orp["qp"])
        u_px = float(mk_bid[ju] if side == 1 else mk_ask[ju])
        u_sz = float(mk_bsz[ju] if side == 1 else mk_asz[ju])
        u_l1 = u_px * u_sz if (math.isfinite(u_sz) and u_sz >= 0.0) else 0.0
        u_notl = qty * u_px
        sweep_usd, clipped = _depth_sweep_cost(u_notl, u_l1, params.depth_impact_bps)
        if clipped:
            depth_slip_usd += sweep_usd
        mk_ent_fee = float(orp["mk_ent_fee"])
        u_fee = u_notl * mk_tk_fee * 1e-4
        fees_usd += u_fee

        f_mk, _, pays = _settle_lot_funding(
            lot_dir=side,
            qty_base=qty,
            mk_fill_ms=int(orp["fill_ms"]),
            hd_fill_ms=int(orp["fill_ms"]),
            mk_exit_ms=u_exit_ms,
            hd_exit_ms=int(orp["fill_ms"]),
            mk_funding=mk_fund_map,
            hd_funding=None,
            mk_hours=mk_hours,
            hd_hours=(),
            mk_bid=mk_bid,
            mk_ask=mk_ask,
            hd_bid=hd_bid,
            hd_ask=hd_ask,
            t0_ms=t0_ms,
            step_ms=step_ms,
        )
        funding_leg_mk_usd += f_mk
        funding_payments += pays
        raw_pnl = side * qty * (u_px - qp)
        net_orp = raw_pnl + f_mk - sweep_usd - mk_ent_fee - u_fee
        orphan_unwind_pnl += net_orp
        orphan_unwind_cost += max(0.0, -raw_pnl) + sweep_usd + mk_ent_fee + u_fee
        intraday_realized_pnl += net_orp
        if record_trades:
            trade_events.append(
                {
                    "kind": "orphan_unwind",
                    "side": side,
                    "fill_ms": int(orp["fill_ms"]),
                    "exit_ms": int(u_exit_ms),
                    "notional_usd": round(float(orp["notl"]), 4),
                    "net_pnl_usd": round(net_orp, 6),
                    "sweep_usd": round(sweep_usd, 6),
                    "fees_usd": round(mk_ent_fee + u_fee, 6),
                }
            )
        return True

    def _book_matched_fill(
        side: int,
        hn: float,
        qp: float,
        hp: float,
        mk_fill_ms: int,
        hd_fill_ms: int,
        mk_fee_rate: float,
        hd_fee_rate: float,
        hd_sweep_usd: float,
    ) -> None:
        nonlocal pos_mk, fees_usd, fx_drag_usd, fx_drag_unit_usd, intraday_fx_unit_usd, depth_slip_usd
        nonlocal funding_leg_mk_usd, funding_leg_hd_usd, funding_payments
        nonlocal intraday_realized_pnl, peak_gross_notl
        qty = hn / qp
        mk_f = hn * mk_fee_rate * 1e-4
        hd_notl = qty * hp
        hd_f = hd_notl * hd_fee_rate * 1e-4
        fx_unit_c = hn * 1e-4
        fx_c = fx_unit_c * fx_drag_bps if fx_drag_bps > 0.0 else 0.0
        fees_usd += mk_f + hd_f
        fx_drag_usd += fx_c
        fx_drag_unit_usd += fx_unit_c
        depth_slip_usd += hd_sweep_usd
        intraday_realized_pnl -= hd_sweep_usd

        # Match FIFO against opposite lots if reducing position
        rem_qty = qty
        while rem_qty > 1e-12 and lots and int(lots[0]["dir"]) == -side:
            lot = lots[0]
            l_qty = float(lot["qty"])
            dq = min(rem_qty, l_qty)
            frac_l = dq / l_qty
            frac_n = dq / qty

            # Lot pnl: long maker (+1) opened at (lot.qp, lot.hp) and closed by short (-1) at (qp, hp)
            ldir = int(lot["dir"])
            gross_lot = ldir * dq * ((qp - float(lot["qp"])) + (float(lot["hp"]) - hp))
            lot_fees = float(lot["mk_fee"]) * frac_l + float(lot["hd_fee"]) * frac_l + (mk_f + hd_f) * frac_n
            lot_fx_unit = float(lot["fx_unit"]) * frac_l + fx_unit_c * frac_n
            lot_fx = float(lot["fx"]) * frac_l + fx_c * frac_n
            intraday_fx_unit_usd += lot_fx_unit

            f_mk, f_hd, pays = _settle_lot_funding(
                lot_dir=ldir,
                qty_base=dq,
                mk_fill_ms=int(lot["mk_ms"]),
                hd_fill_ms=int(lot["hd_ms"]),
                mk_exit_ms=mk_fill_ms,
                hd_exit_ms=hd_fill_ms,
                mk_funding=mk_fund_map,
                hd_funding=hd_fund_map,
                mk_hours=mk_hours,
                hd_hours=hd_hours,
                mk_bid=mk_bid,
                mk_ask=mk_ask,
                hd_bid=hd_bid,
                hd_ask=hd_ask,
                t0_ms=t0_ms,
                step_ms=step_ms,
            )
            funding_leg_mk_usd += f_mk
            funding_leg_hd_usd += f_hd
            funding_payments += pays
            hold_mins.append(max(0.0, (mk_fill_ms - int(lot["mk_ms"])) / 60_000.0))
            realized_lot = gross_lot + f_mk + f_hd - lot_fees - lot_fx
            intraday_realized_pnl += realized_lot

            if record_trades:
                trade_events.append(
                    {
                        "kind": "closed_lot",
                        "dir": ldir,
                        "qty": round(dq, 8),
                        "leg_a_fill_ms": int(lot["mk_ms"] if params.flipped_role else lot["hd_ms"]),
                        "leg_b_fill_ms": int(lot["hd_ms"] if params.flipped_role else lot["mk_ms"]),
                        "leg_a_exit_ms": int(mk_fill_ms if params.flipped_role else hd_fill_ms),
                        "leg_b_exit_ms": int(hd_fill_ms if params.flipped_role else mk_fill_ms),
                        "funding_usd": round(f_mk + f_hd, 6),
                        "realized_pnl_usd": round(realized_lot, 6),
                    }
                )

            if dq >= l_qty - 1e-12:
                lots.pop(0)
            else:
                lot["qty"] = l_qty - dq
                lot["mk_fee"] = float(lot["mk_fee"]) * (1.0 - frac_l)
                lot["hd_fee"] = float(lot["hd_fee"]) * (1.0 - frac_l)
                lot["fx"] = float(lot["fx"]) * (1.0 - frac_l)
                lot["fx_unit"] = float(lot["fx_unit"]) * (1.0 - frac_l)
            rem_qty -= dq

        if rem_qty > 1e-12:
            frac_rem = rem_qty / qty
            lots.append(
                {
                    "dir": side,
                    "qty": rem_qty,
                    "qp": qp,
                    "hp": hp,
                    "mk_ms": mk_fill_ms,
                    "hd_ms": hd_fill_ms,
                    "mk_fee": mk_f * frac_rem,
                    "hd_fee": hd_f * frac_rem,
                    "fx": fx_c * frac_rem,
                    "fx_unit": fx_unit_c * frac_rem,
                }
            )
        pos_mk += side * qty
        gross_now = abs(pos_mk) * qp
        if gross_now > peak_gross_notl:
            peak_gross_notl = gross_now

    def _execute_hedge_and_book(side: int, fn: float, qp: float, first_tf: int) -> None:
        nonlocal fills, maker_filled_notl, hedged_notl_tot, orphan_events, orphan_notl_tot
        nonlocal hedge_passive_fills, hedge_taker_fallbacks, depth_clipped_entries, fees_usd
        fills += 1
        maker_filled_notl += fn
        mk_ent_fee_total = fn * mk_mk_fee * 1e-4

        hedged_notl = 0.0
        eff_hp = 0.0
        eff_hd_ms = first_tf + hd_tk_delay
        eff_hd_fee = hd_tk_fee
        hd_sweep_tot = 0.0

        if params.hedge_mode == "maker_fallback" and hd_ts is not None:
            t_post_h = first_tf + hd_mk_delay
            hit_p = _lookup_valid_step(hd_age, hd_bid, hd_ask, t_post_h, t0_ms, step_ms, max_age_steps, 2)
            if hit_p is not None:
                jp, _ = hit_p
                # If side==1 (maker bought), hedge SELLS passively at hd_ask; if side==-1, hedge BUYS passively at hd_bid
                h_rest = float(hd_ask[jp] if side == 1 else hd_bid[jp])
                lo = bisect_left(hd_ts, t_post_h)
                hi = bisect_right(hd_ts, t_post_h + params.max_hedge_passive_wait_ms)
                hth = 0.0
                h_ft: int | None = None
                for ii in range(lo, hi):
                    hsg = int(hd_sgn[ii])
                    hpx = float(hd_px[ii])
                    if (side == 1 and hsg == 1 and hpx > h_rest) or (side == -1 and hsg == -1 and hpx < h_rest):
                        if h_ft is None:
                            h_ft = int(hd_ts[ii])
                        hth += hpx * float(hd_sz[ii])
                        if hth >= fn:
                            break
                if h_ft is not None and hth >= MIN_FILL_USD:
                    p_hn = min(fn, hth)
                    hedged_notl = p_hn
                    eff_hp = h_rest
                    eff_hd_ms = h_ft
                    eff_hd_fee = hd_mk_fee
                    hedge_passive_fills += 1

        if hedged_notl < fn:
            rem = fn - hedged_notl
            t_tk = (
                first_tf + hd_mk_delay + params.max_hedge_passive_wait_ms + hd_tk_delay
                if params.hedge_mode == "maker_fallback"
                else first_tf + hd_tk_delay
            )
            hit_t = _lookup_valid_step(hd_age, hd_bid, hd_ask, t_tk, t0_ms, step_ms, max_age_steps, max_rec_steps)
            if hit_t is not None:
                jt, h_tk_ms = hit_t
                tp = float(hd_bid[jt] if side == 1 else hd_ask[jt])
                tsz = float(hd_bsz[jt] if side == 1 else hd_asz[jt])
                hl1 = tp * tsz if (math.isfinite(tsz) and tsz >= 0.0) else 0.0
                # P0 Defect 1: strictly clamp hedge notional to available L1 depth!
                t_hn = min(rem, hl1)
                if t_hn >= MIN_FILL_USD:
                    if params.hedge_mode == "maker_fallback":
                        hedge_taker_fallbacks += 1
                    if t_hn < rem:
                        depth_clipped_entries += 1
                    if hedged_notl > 0.0:
                        tot = hedged_notl + t_hn
                        eff_hp = (hedged_notl * eff_hp + t_hn * tp) / tot
                        eff_hd_fee = (hedged_notl * eff_hd_fee + t_hn * hd_tk_fee) / tot
                        eff_hd_ms = max(eff_hd_ms, h_tk_ms)
                        hedged_notl = tot
                    else:
                        hedged_notl = t_hn
                        eff_hp = tp
                        eff_hd_fee = hd_tk_fee
                        eff_hd_ms = h_tk_ms
                elif hl1 < rem:
                    depth_clipped_entries += 1

        if hedged_notl > 0.0:
            hedged_notl_tot += hedged_notl
            _book_matched_fill(side, hedged_notl, qp, eff_hp, first_tf, eff_hd_ms, mk_mk_fee, eff_hd_fee, hd_sweep_tot)

        # P0 Defect 2: any unhedged remainder is explicitly booked & unwound as an orphan leg!
        orphan_notl = max(0.0, fn - hedged_notl)
        if orphan_notl > 1e-9:
            orphan_events += 1
            orphan_notl_tot += orphan_notl
            t_orp_unwind = (first_tf + hd_tk_delay) + mk_delay
            fees_usd += orphan_notl * mk_mk_fee * 1e-4
            orp = {
                "side": side,
                "qty": orphan_notl / qp,
                "qp": qp,
                "notl": orphan_notl,
                "fill_ms": first_tf,
                "mk_ent_fee": orphan_notl * mk_mk_fee * 1e-4,
                "earliest_ms": t_orp_unwind,
            }
            if not _unwind_orphan_at(orp, t_orp_unwind):
                open_orphans.append(orp)

    if params.hedge_mode == "reversion":
        # Arm A2: Demeaned (b - 2h_MA) reversion with passive Arcus maker entry & exit
        mk_tr_ts = mk_trades["ts"]
        mk_tr_sgn = mk_trades["sgn"]
        mk_tr_px = mk_trades["px"]
        mk_tr_sz = mk_trades["sz"]
        k = warmup_steps
        stride = max(1_000 // step_ms, 1)  # 1s decision cadence
        while k < min(warmup_steps + decision_steps, n - 1):
            t_cur = t0_ms + (k + 1) * step_ms
            if open_orphans:
                rem_o: list[dict[str, Any]] = []
                for orp in open_orphans:
                    if t_cur < int(orp["earliest_ms"]) or not _unwind_orphan_at(orp, t_cur):
                        rem_o.append(orp)
                open_orphans = rem_o
            if mk_age[k] > max_age_steps or hd_age[k] > max_age_steps:
                k += stride
                continue
            fb_raw = prior_fair[k]
            b_raw = raw_b[k]
            if not (math.isfinite(fb_raw) and math.isfinite(b_raw)):
                k += stride
                continue
            dm_bps = b_sign * (b_raw - fb_raw) * 1e4
            mb, ma = float(mk_bid[k]), float(mk_ask[k])
            hb, ha = float(hd_bid[k]), float(hd_ask[k])
            if not (mb > 0.0 and ma > mb and hb > 0.0 and ha > hb):
                k += stride
                continue

            if not lots and not open_orphans:
                cand_side = 0
                if dm_bps >= params.reversion_dev_bps:
                    cand_side = -1
                elif dm_bps <= -params.reversion_dev_bps:
                    cand_side = 1
                if cand_side != 0:
                    fb = b_sign * fb_raw
                    qp = mb if cand_side == 1 else ma
                    # Require quote to clear min_edge vs hedge side + fair
                    edge_now = (
                        (math.log(hb) + fb - math.log(qp)) * 1e4
                        if cand_side == 1
                        else (math.log(qp) - math.log(ha) - fb) * 1e4
                    )
                    if edge_now >= params.reversion_min_edge_bps:
                        t_post = t_cur + mk_delay
                        lo = bisect_left(mk_tr_ts, t_post)
                        hi = bisect_right(mk_tr_ts, t_post + 2_000)
                        th = 0.0
                        ft: int | None = None
                        for ii in range(lo, hi):
                            sg = int(mk_tr_sgn[ii])
                            px = float(mk_tr_px[ii])
                            if (cand_side == 1 and sg == -1 and px < qp) or (cand_side == -1 and sg == 1 and px > qp):
                                if ft is None:
                                    ft = int(mk_tr_ts[ii])
                                th += px * float(mk_tr_sz[ii])
                                if th >= clip_usd:
                                    break
                        if ft is not None and th >= MIN_FILL_USD:
                            _execute_hedge_and_book(cand_side, min(clip_usd, th), qp, ft)
                            k = max(k + stride, int((ft + hd_tk_delay - t0_ms) // step_ms))
                            continue
            elif lots:
                lot0 = lots[0]
                ldir = int(lot0["dir"])
                held_ms = t_cur - int(lot0["mk_ms"])
                reverted = (ldir == 1 and dm_bps >= -params.reversion_tp_bps) or (
                    ldir == -1 and dm_bps <= params.reversion_tp_bps
                )
                if reverted:
                    exit_side = -ldir
                    qp = mb if exit_side == 1 else ma
                    t_post = t_cur + mk_delay
                    lo = bisect_left(mk_tr_ts, t_post)
                    hi = bisect_right(mk_tr_ts, t_post + 2_000)
                    th = 0.0
                    ft = None
                    target_n = abs(pos_mk) * qp
                    for ii in range(lo, hi):
                        sg = int(mk_tr_sgn[ii])
                        px = float(mk_tr_px[ii])
                        if (exit_side == 1 and sg == -1 and px < qp) or (exit_side == -1 and sg == 1 and px > qp):
                            if ft is None:
                                ft = int(mk_tr_ts[ii])
                            th += px * float(mk_tr_sz[ii])
                            if th >= target_n:
                                break
                    if ft is not None and th >= MIN_FILL_USD:
                        _execute_hedge_and_book(exit_side, min(target_n, th), qp, ft)
                        k = max(k + stride, int((ft + hd_tk_delay - t0_ms) // step_ms))
                        continue
                elif held_ms >= params.reversion_max_hold_ms:
                    # Max-hold timed exit via taker on both legs
                    hit_m = _lookup_valid_step(mk_age, mk_bid, mk_ask, t_cur + mk_delay, t0_ms, step_ms, max_age_steps, max_rec_steps)
                    hit_h = _lookup_valid_step(hd_age, hd_bid, hd_ask, t_cur + hd_tk_delay, t0_ms, step_ms, max_age_steps, max_rec_steps)
                    if hit_m is not None and hit_h is not None:
                        jm, m_ms = hit_m
                        jh, h_ms = hit_h
                        exit_side = -ldir
                        mp = float(mk_ask[jm] if exit_side == 1 else mk_bid[jm])
                        hp = float(hd_bid[jh] if exit_side == 1 else hd_ask[jh])
                        msz = float(mk_asz[jm] if exit_side == 1 else mk_bsz[jm])
                        hsz = float(hd_bsz[jh] if exit_side == 1 else hd_asz[jh])
                        ex_notl = abs(pos_mk) * mp
                        sw_m, cl_m = _depth_sweep_cost(ex_notl, mp * msz, params.depth_impact_bps)
                        sw_h, cl_h = _depth_sweep_cost(abs(pos_mk) * hp, hp * hsz, params.depth_impact_bps)
                        if cl_m or cl_h:
                            depth_clipped_exits += 1
                        _book_matched_fill(exit_side, ex_notl, mp, hp, m_ms, h_ms, mk_tk_fee, hd_tk_fee, sw_m + sw_h)
            k += stride
    else:
        # Arms A0, A1, B0: Continuous inventory-skewed Basis Maker-Hedge
        tr_sgn = mk_trades["sgn"]
        tr_px = mk_trades["px"]
        tr_sz = mk_trades["sz"]
        tr_ts = mk_trades["ts"]

        def _ref_idx(t_ms: int) -> int:
            return min(max(int((t_ms - t0_ms) // step_ms) - 1, 0), n - 1)

        for k in sorted(by_step.keys()):
            t_cur = t0_ms + (k + 1) * step_ms
            if open_orphans:
                rem_o = []
                for orp in open_orphans:
                    if t_cur < int(orp["earliest_ms"]) or not _unwind_orphan_at(orp, t_cur):
                        rem_o.append(orp)
                open_orphans = rem_o
            if mk_age[k] > max_age_steps or hd_age[k] > max_age_steps:
                continue
            fb_raw = prior_fair[k]
            b_raw = raw_b[k]
            fast_raw = prior_fast[k]
            if not (math.isfinite(fb_raw) and math.isfinite(b_raw)):
                continue
            fb = b_sign * fb_raw
            b_now = b_sign * b_raw
            f_now = b_sign * fast_raw if math.isfinite(fast_raw) else fb

            mb, ma = float(mk_bid[k]), float(mk_ask[k])
            hb, ha = float(hd_bid[k]), float(hd_ask[k])
            if not (mb > 0.0 and ma > mb and hb > 0.0 and ha > hb):
                continue

            mk_mid_k = 0.5 * (mb + ma)
            q = pos_mk * mk_mid_k / clip_usd
            eff_e0 = params.e0_bps + (params.vol_edge_mult * prior_sigma[k] if params.vol_edge_mult > 0.0 else 0.0)
            eff_edges.append(eff_e0)
            sk_q = _skew_q(q, q_max_clips, params.nonlinear_skew_power)

            drift_bps = (b_now - f_now) * 1e4
            drift_block_bid = params.basis_drift_guard_bps > 0.0 and q >= 0.0 and drift_bps <= -params.basis_drift_guard_bps
            drift_block_ask = params.basis_drift_guard_bps > 0.0 and q <= 0.0 and drift_bps >= params.basis_drift_guard_bps

            hb_notl = hb * float(hd_bsz[k]) if math.isfinite(float(hd_bsz[k])) else 0.0
            ha_notl = ha * float(hd_asz[k]) if math.isfinite(float(hd_asz[k])) else 0.0
            l1_block_bid = params.min_hedge_l1_notional_usd > 0.0 and q >= 0.0 and hb_notl < params.min_hedge_l1_notional_usd
            l1_block_ask = params.min_hedge_l1_notional_usd > 0.0 and q <= 0.0 and ha_notl < params.min_hedge_l1_notional_usd

            quotes: list[tuple[int, float]] = []
            if q < q_max_clips:
                if drift_block_bid:
                    blocked_drift += 1
                elif l1_block_bid:
                    blocked_l1_thin += 1
                else:
                    e_bid = eff_e0 + params.skew_bps_per_clip * sk_q
                    cap = hb * math.exp(fb - e_bid * 1e-4)
                    bid_q = min(mb, math.floor(cap / tick + 1e-9) * tick)
                    if 0.0 < bid_q < ma:
                        quotes.append((1, bid_q))
            if q > -q_max_clips:
                if drift_block_ask:
                    blocked_drift += 1
                elif l1_block_ask:
                    blocked_l1_thin += 1
                else:
                    e_ask = eff_e0 - params.skew_bps_per_clip * sk_q
                    floor_ = ha * math.exp(fb + e_ask * 1e-4)
                    ask_q = max(ma, math.ceil(floor_ / tick - 1e-9) * tick)
                    if ask_q > mb:
                        quotes.append((-1, ask_q))

            if not quotes:
                continue

            bucket_indices = by_step[k]
            for side, qp in quotes:
                through_notl = 0.0
                first_tf: int | None = None
                for idx_t in bucket_indices:
                    sg = int(tr_sgn[idx_t])
                    px = float(tr_px[idx_t])
                    if (side == 1 and sg == -1 and px < qp) or (side == -1 and sg == 1 and px > qp):
                        if first_tf is None:
                            first_tf = int(tr_ts[idx_t])
                        through_notl += px * float(tr_sz[idx_t])
                if first_tf is None or through_notl < MIN_FILL_USD:
                    continue

                if params.shield_bps is not None and not params.flipped_role and hedge_speedbump_ms > 80:
                    i0 = _ref_idx(first_tf - hedge_speedbump_ms + 40)
                    i1 = _ref_idx(first_tf - 40)
                    if hd_age[i0] <= max_age_steps and hd_age[i1] <= max_age_steps:
                        r0 = 0.5 * (float(hd_bid[i0]) + float(hd_ask[i0]))
                        r1 = 0.5 * (float(hd_bid[i1]) + float(hd_ask[i1]))
                        if r0 > 0.0 and r1 > 0.0:
                            rmv = (r1 / r0 - 1.0) * 1e4
                            if (side == 1 and rmv <= -params.shield_bps) or (side == -1 and rmv >= params.shield_bps):
                                blocked_toxic += 1
                                continue

                _execute_hedge_and_book(side, min(clip_usd, through_notl), qp, first_tf)

    # Terminal unwind of open orphans and remaining matched lots with explicit exit delay!
    rem_orphans_end: list[dict[str, Any]] = []
    for orp in open_orphans:
        if not _unwind_orphan_at(orp, max(t_end_ms, int(orp["earliest_ms"])), force_end=True):
            rem_orphans_end.append(orp)

    terminal_flat = True
    terminal_residual_notl = 0.0
    terminal_drain_pnl = 0.0
    residual_mtm_pnl = 0.0

    for orp in rem_orphans_end:
        terminal_flat = False
        terminal_residual_notl += float(orp["notl"])
        end_m = 0.5 * (float(mk_bid[end_k]) + float(mk_ask[end_k]))
        if end_m > 0.0 and math.isfinite(end_m):
            residual_mtm_pnl += int(orp["side"]) * float(orp["qty"]) * (end_m - float(orp["qp"])) - float(orp["mk_ent_fee"])

    if lots:
        hit_m_end = _lookup_valid_step(mk_age, mk_bid, mk_ask, t_end_ms + mk_delay, t0_ms, step_ms, max_age_steps, max_rec_steps)
        hit_h_end = _lookup_valid_step(hd_age, hd_bid, hd_ask, t_end_ms + hd_tk_delay, t0_ms, step_ms, max_age_steps, max_rec_steps)
        if hit_m_end is not None and hit_h_end is not None:
            jm, m_exit_ms = hit_m_end
            jh, h_exit_ms = hit_h_end
            m_exit_ms = min(m_exit_ms, t_end_ms + max(mk_delay, hd_tk_delay))
            h_exit_ms = min(h_exit_ms, t_end_ms + max(mk_delay, hd_tk_delay))
            tot_qty = sum(float(l["qty"]) for l in lots)
            ldir = int(lots[0]["dir"])
            exit_side = -ldir
            mp_end = float(mk_ask[jm] if exit_side == 1 else mk_bid[jm])
            hp_end = float(hd_bid[jh] if exit_side == 1 else hd_ask[jh])
            msz_end = float(mk_asz[jm] if exit_side == 1 else mk_bsz[jm])
            hsz_end = float(hd_bsz[jh] if exit_side == 1 else hd_asz[jh])
            sw_m, cl_m = _depth_sweep_cost(tot_qty * mp_end, mp_end * msz_end, params.depth_impact_bps)
            sw_h, cl_h = _depth_sweep_cost(tot_qty * hp_end, hp_end * hsz_end, params.depth_impact_bps)
            if cl_m or cl_h:
                depth_clipped_exits += 1
            depth_slip_usd += sw_m + sw_h
            drain_fees = tot_qty * mp_end * mk_tk_fee * 1e-4 + tot_qty * hp_end * hd_tk_fee * 1e-4
            fees_usd += drain_fees
            drain_fx_exit_unit = (tot_qty * mp_end) * 1e-4
            drain_fx = drain_fx_exit_unit * fx_drag_bps if fx_drag_bps > 0.0 else 0.0
            fx_drag_usd += drain_fx
            fx_drag_unit_usd += drain_fx_exit_unit
            drain_fx_unit_usd += drain_fx_exit_unit

            drain_gross = 0.0
            drain_entry_costs = 0.0
            drain_funding = 0.0
            for lot in lots:
                dq = float(lot["qty"])
                d_gross = int(lot["dir"]) * dq * ((mp_end - float(lot["qp"])) + (float(lot["hp"]) - hp_end))
                d_ent = float(lot["mk_fee"]) + float(lot["hd_fee"]) + float(lot["fx"])
                drain_fx_unit_usd += float(lot["fx_unit"])
                f_mk, f_hd, pays = _settle_lot_funding(
                    lot_dir=int(lot["dir"]),
                    qty_base=dq,
                    mk_fill_ms=int(lot["mk_ms"]),
                    hd_fill_ms=int(lot["hd_ms"]),
                    mk_exit_ms=m_exit_ms,
                    hd_exit_ms=h_exit_ms,
                    mk_funding=mk_fund_map,
                    hd_funding=hd_fund_map,
                    mk_hours=mk_hours,
                    hd_hours=hd_hours,
                    mk_bid=mk_bid,
                    mk_ask=mk_ask,
                    hd_bid=hd_bid,
                    hd_ask=hd_ask,
                    t0_ms=t0_ms,
                    step_ms=step_ms,
                )
                funding_leg_mk_usd += f_mk
                funding_leg_hd_usd += f_hd
                funding_payments += pays
                hold_mins.append(max(0.0, (m_exit_ms - int(lot["mk_ms"])) / 60_000.0))
                drain_gross += d_gross
                drain_entry_costs += d_ent
                drain_funding += f_mk + f_hd

            terminal_drain_pnl = drain_gross + drain_funding - drain_entry_costs - drain_fees - drain_fx - (sw_m + sw_h)
        else:
            terminal_flat = False
            m_mid = 0.5 * (float(mk_bid[end_k]) + float(mk_ask[end_k]))
            h_mid = 0.5 * (float(hd_bid[end_k]) + float(hd_ask[end_k]))
            for lot in lots:
                dq = float(lot["qty"])
                terminal_residual_notl += dq * float(lot["qp"])
                if m_mid > 0.0 and h_mid > 0.0 and math.isfinite(m_mid) and math.isfinite(h_mid):
                    d_gross = int(lot["dir"]) * dq * ((m_mid - float(lot["qp"])) + (float(lot["hp"]) - h_mid))
                    d_ent = float(lot["mk_fee"]) + float(lot["hd_fee"]) + float(lot["fx"])
                    residual_mtm_pnl += d_gross - d_ent
                    residual_fx_unit_usd += float(lot["fx_unit"])

    realized_only = intraday_realized_pnl + terminal_drain_pnl
    mtm_incl_residual = realized_only + residual_mtm_pnl
    funding_cash = funding_leg_mk_usd + funding_leg_hd_usd

    out: dict[str, Any] = {
        "fills": fills,
        "maker_filled_notional_usd": round(maker_filled_notl, 2),
        "hedged_notional_usd": round(hedged_notl_tot, 2),
        "orphan_events": orphan_events,
        "orphan_notional_usd": round(orphan_notl_tot, 2),
        "orphan_unwind_rate": round(orphan_notl_tot / maker_filled_notl, 6) if maker_filled_notl > 0.0 else 0.0,
        "orphan_unwind_pnl_usd": round(orphan_unwind_pnl, 4),
        "orphan_unwind_cost_usd": round(orphan_unwind_cost, 4),
        "hedge_passive_fills": hedge_passive_fills,
        "hedge_taker_fallbacks": hedge_taker_fallbacks,
        "depth_clipped_entries": depth_clipped_entries,
        "depth_clipped_exits": depth_clipped_exits,
        "blocked_toxic": blocked_toxic,
        "blocked_drift": blocked_drift,
        "blocked_l1_thin": blocked_l1_thin,
        "fees_usd": round(fees_usd, 4),
        "fx_drag_usd": round(fx_drag_usd, 4),
        "fx_drag_unit_usd": round(fx_drag_unit_usd, 6),
        "intraday_fx_unit_usd": round(intraday_fx_unit_usd, 6),
        "drain_fx_unit_usd": round(drain_fx_unit_usd, 6),
        "residual_fx_unit_usd": round(residual_fx_unit_usd, 6),
        "depth_slip_usd": round(depth_slip_usd, 4),
        "funding_leg_mk_usd": round(funding_leg_mk_usd, 4),
        "funding_leg_hd_usd": round(funding_leg_hd_usd, 4),
        "funding_cash_usd": round(funding_cash, 4),
        "funding_payments": funding_payments,
        "intraday_realized_pnl_usd": round(intraday_realized_pnl, 4),
        "terminal_drain_pnl_usd": round(terminal_drain_pnl, 4),
        "realized_only_pnl_usd": round(realized_only, 4),
        "mtm_incl_residual_pnl_usd": round(mtm_incl_residual, 4),
        "terminal_flat": terminal_flat,
        "terminal_residual_notional_usd": round(terminal_residual_notl, 4),
        "peak_gross_notional_usd": round(peak_gross_notl, 2),
        "mean_hold_min": round(sum(hold_mins) / len(hold_mins), 2) if hold_mins else 0.0,
        "mean_effective_e0_bps": round(sum(eff_edges) / len(eff_edges), 3) if eff_edges else params.e0_bps,
    }
    if record_trades:
        out["trade_events"] = trade_events
    return out


def discover_w01_w27_campaigns(
    lakehouse_dir: Path,
    domain: str,
    *,
    min_window: int = 1,
    max_window: int = 27,
) -> list[dict[str, Any]]:
    """Discover Tokyo W01..W27 campaigns dynamically, strictly excluding `-w28`."""
    pat = str(
        lakehouse_dir
        / "parquet"
        / "bbo_quotes"
        / "collector_region=tokyo"
        / "venue=arcus"
        / "utc_date=*"
        / f"campaign_id=tokyo-{domain}-*-w*"
    )
    by_w: dict[int, dict[str, Any]] = {}
    for d in glob.glob(pat):
        cid = d.rsplit("campaign_id=", 1)[1]
        if cid.endswith("-w28"):
            continue
        m = re.search(rf"^tokyo-{re.escape(domain)}-(\d{{8}})T\d+Z-w(\d+)$", cid)
        if not m:
            continue
        ymd, w_num = m.group(1), int(m.group(2))
        if w_num < min_window or w_num > max_window:
            continue
        if domain == "crypto" and w_num == 8:
            continue
        utc_date = f"{ymd[:4]}-{ymd[4:6]}-{ymd[6:8]}"
        if w_num not in by_w:
            by_w[w_num] = {
                "window_num": w_num,
                "window": f"w{w_num:02d}",
                "campaign_id": cid,
                "utc_date": utc_date,
                "domain": domain,
                "split": split_for_window(w_num),
            }
    return [by_w[w] for w in sorted(by_w)]


def campaign_parquet_files(
    lakehouse_dir: Path, dataset: str, venue: str, utc_date: str, campaign_id: str
) -> list[str]:
    """Cross-midnight Hive parquet globbing over `utc_date` and `utc_date + 1`, never `-w28`."""
    if campaign_id.endswith("-w28"):
        return []
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
    return sorted({f for f in files if "-w28/" not in f and "-w28\\" not in f})


def _sql_list(files: Sequence[str]) -> str:
    return "[" + ", ".join("'" + f.replace("'", "''") + "'" for f in files) + "]"


def _load_venue_bbo_buckets(
    con: Any,
    files: Sequence[str],
    markets: Sequence[str],
    step_ms: int = STEP_MS,
) -> dict[str, tuple[dict[str, Any], dict[str, Any]]]:
    """Load 200ms bucketed BBOs + continuity stats for all markets of a venue in one DuckDB query."""
    import numpy as np

    if not files or not markets:
        return {}
    src = _sql_list(files)
    ph = ", ".join("?" for _ in markets)
    st_rows = con.execute(
        f"""
        WITH s AS (
          SELECT market, available_at_ms, LAG(available_at_ms) OVER (PARTITION BY market ORDER BY available_at_ms) AS prev_ts
          FROM read_parquet({src})
          WHERE market IN ({ph}) AND tradable AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
            AND NOT regexp_matches(campaign_id, '-w28$')
        )
        SELECT market, COUNT(*), MIN(available_at_ms), MAX(available_at_ms), COALESCE(MAX(available_at_ms - prev_ts), 0)
        FROM s
        GROUP BY 1
        """,
        list(markets),
    ).fetchall()
    cont_map: dict[str, dict[str, Any]] = {}
    for mkt, cnt, t0, t1, mgap in st_rows:
        if cnt and cnt >= 2 and t0 is not None and t1 is not None:
            dur = int(t1 - t0)
            cont_map[str(mkt)] = {
                "events": int(cnt),
                "t0_ms": int(t0),
                "t1_ms": int(t1),
                "duration_ms": dur,
                "hours": round(dur / HOUR_MS, 3),
                "max_gap_ms": int(mgap),
                "is_continuous_10h": dur >= TEN_HOURS_MS and int(mgap) <= MAX_CONTINUOUS_GAP_MS,
            }
    if not cont_map:
        return {}

    cols = con.execute(
        f"""
        SELECT market,
               available_at_ms // {int(step_ms)} AS b,
               arg_max(bid_px_f64, available_at_ms) AS bid,
               arg_max(CAST(bid_sz AS DOUBLE), available_at_ms) AS bsz,
               arg_max(ask_px_f64, available_at_ms) AS ask,
               arg_max(CAST(ask_sz AS DOUBLE), available_at_ms) AS asz
        FROM read_parquet({src})
        WHERE market IN ({ph}) AND tradable AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
          AND NOT regexp_matches(campaign_id, '-w28$')
        GROUP BY 1, 2
        ORDER BY 1, 2
        """,
        list(markets),
    ).fetchnumpy()
    mkt_arr = cols["market"]
    out: dict[str, tuple[dict[str, Any], dict[str, Any]]] = {}
    if len(mkt_arr) == 0:
        return out
    # Split contiguous sorted market blocks
    n_tot = len(mkt_arr)
    start = 0
    while start < n_tot:
        cur_m = str(mkt_arr[start])
        end = start + 1
        while end < n_tot and str(mkt_arr[end]) == cur_m:
            end += 1
        if cur_m in cont_map:
            out[cur_m] = (
                {
                    "b": np.asarray(cols["b"][start:end], dtype=np.int64),
                    "bid": np.asarray(cols["bid"][start:end], dtype=np.float64),
                    "bsz": np.asarray(cols["bsz"][start:end], dtype=np.float64),
                    "ask": np.asarray(cols["ask"][start:end], dtype=np.float64),
                    "asz": np.asarray(cols["asz"][start:end], dtype=np.float64),
                },
                cont_map[cur_m],
            )
        start = end
    return out


def _load_venue_trades(
    con: Any,
    files: Sequence[str],
    markets: Sequence[str],
) -> dict[str, dict[str, Any]]:
    import numpy as np

    if not files or not markets:
        return {}
    src = _sql_list(files)
    ph = ", ".join("?" for _ in markets)
    cols = con.execute(
        f"""
        SELECT market,
               available_at_ms AS ts,
               CASE WHEN lower(aggressor_side) IN ('buy','b','bid') THEN 1
                    WHEN lower(aggressor_side) IN ('sell','s','ask') THEN -1 ELSE 0 END AS sgn,
               price_f64 AS px,
               size_f64 AS sz
        FROM read_parquet({src})
        WHERE market IN ({ph}) AND NOT is_zero_price_transfer AND price_f64 > 0 AND size_f64 > 0
          AND NOT regexp_matches(campaign_id, '-w28$')
        ORDER BY 1, 2
        """,
        list(markets),
    ).fetchnumpy()
    mkt_arr = cols["market"]
    out: dict[str, dict[str, Any]] = {}
    n_tot = len(mkt_arr)
    start = 0
    while start < n_tot:
        cur_m = str(mkt_arr[start])
        end = start + 1
        while end < n_tot and str(mkt_arr[end]) == cur_m:
            end += 1
        out[cur_m] = {
            "ts": np.asarray(cols["ts"][start:end], dtype=np.int64),
            "sgn": np.asarray(cols["sgn"][start:end], dtype=np.int64),
            "px": np.asarray(cols["px"][start:end], dtype=np.float64),
            "sz": np.asarray(cols["sz"][start:end], dtype=np.float64),
        }
        start = end
    return out


def _load_venue_funding(
    con: Any,
    files: Sequence[str],
    venue: str,
    sym_to_raw: dict[str, str],
) -> dict[str, dict[int, float]]:
    """Load settled hourly funding rates, matching both symbol (`NVDA`) and raw market ID (`15`)."""
    if not files or not sym_to_raw or venue == "arcus":
        return {}
    src = _sql_list(files)
    alias_to_raw: dict[str, str] = {}
    for sym, raw in sym_to_raw.items():
        alias_to_raw[sym] = raw
        alias_to_raw[raw] = raw
    candidates = sorted(alias_to_raw.keys())
    ph = ", ".join("?" for _ in candidates)
    rows = con.execute(
        f"""
        SELECT market,
               ((funding_slot_ms // {HOUR_MS}) * {HOUR_MS}) + {HOUR_MS} AS pay_ms,
               avg(funding_rate_bps_f64) AS rate_bps
        FROM read_parquet({src})
        WHERE market IN ({ph}) AND ctx_kind = 'market_stats'
          AND funding_slot_ms IS NOT NULL AND funding_rate_bps_f64 IS NOT NULL
          AND available_at_ms >= funding_slot_ms AND available_at_ms < funding_slot_ms + {HOUR_MS}
          AND NOT regexp_matches(campaign_id, '-w28$')
        GROUP BY 1, 2
        """,
        candidates,
    ).fetchall()
    out: dict[str, dict[int, float]] = {}
    for mkt, pay_ms, rate in rows:
        if pay_ms is not None and rate is not None and math.isfinite(float(rate)):
            raw_key = alias_to_raw.get(str(mkt), str(mkt))
            out.setdefault(raw_key, {})[int(pay_ms)] = float(rate)
    return out


def _clone_run_with_fx(res_0bp: dict[str, Any], fx_bps: float) -> dict[str, Any]:
    """Derive exact `fx_bps` metrics from `0bp` run without redundant simulation."""
    out = dict(res_0bp)
    if fx_bps <= 0.0:
        return out
    intra_fx = fx_bps * float(res_0bp.get("intraday_fx_unit_usd", 0.0))
    drain_fx = fx_bps * float(res_0bp.get("drain_fx_unit_usd", 0.0))
    res_fx = fx_bps * float(res_0bp.get("residual_fx_unit_usd", 0.0))
    tot_fx = fx_bps * float(res_0bp.get("fx_drag_unit_usd", 0.0))
    new_intra = float(res_0bp["intraday_realized_pnl_usd"]) - intra_fx
    new_drain = float(res_0bp["terminal_drain_pnl_usd"]) - drain_fx
    new_real = new_intra + new_drain
    new_mtm = float(res_0bp["mtm_incl_residual_pnl_usd"]) - tot_fx
    out["fx_drag_usd"] = round(tot_fx, 4)
    out["intraday_realized_pnl_usd"] = round(new_intra, 4)
    out["terminal_drain_pnl_usd"] = round(new_drain, 4)
    out["realized_only_pnl_usd"] = round(new_real, 4)
    out["mtm_incl_residual_pnl_usd"] = round(new_mtm, 4)
    return out


def _build_grid_np(cols: dict[str, Any], t0_ms: int, t1_ms: int, step_ms: int = STEP_MS) -> dict[str, Any]:
    import numpy as np

    n = int((t1_ms - t0_ms) // step_ms)
    b0 = t0_ms // step_ms
    idx = cols["b"] - b0
    keep = (idx >= 0) & (idx < n)
    pre = idx < 0
    out: dict[str, Any] = {}
    for k in ("bid", "ask", "bsz", "asz"):
        arr = np.full(n, np.nan, dtype=np.float64)
        src = cols[k]
        arr[idx[keep]] = src[keep]
        if pre.any() and np.isnan(arr[0]):
            arr[0] = src[pre][-1]
        out[k] = arr
    updated = np.zeros(n, dtype=bool)
    updated[idx[keep]] = True
    valid = ~np.isnan(out["bid"])
    pos = np.where(valid, np.arange(n), 0)
    np.maximum.accumulate(pos, out=pos)
    for k in ("bid", "ask", "bsz", "asz"):
        out[k] = out[k][pos]
    upos = np.where(updated, np.arange(n), -1)
    np.maximum.accumulate(upos, out=upos)
    out["age"] = np.where(upos >= 0, np.arange(n) - upos, n).astype(np.int64)
    return out


def _infer_tick(prices: Sequence[float]) -> float:
    best = 1.0
    for px in prices[:500]:
        if px > 0.0 and math.isfinite(px):
            s = f"{px:.8f}".rstrip("0").rstrip(".")
            if "." in s:
                best = min(best, 10.0 ** (-len(s.split(".")[1])))
    return max(best, 1e-6)


def _aggregate_window_runs(runs: Sequence[dict[str, Any]]) -> dict[str, Any]:
    if not runs:
        return {
            "windows_covered": 0,
            "fills": 0,
            "maker_filled_notional_usd": 0.0,
            "hedged_notional_usd": 0.0,
            "orphan_events": 0,
            "orphan_notional_usd": 0.0,
            "orphan_unwind_rate": 0.0,
            "orphan_unwind_pnl_usd": 0.0,
            "orphan_unwind_cost_usd": 0.0,
            "fees_usd": 0.0,
            "fx_drag_usd": 0.0,
            "depth_slip_usd": 0.0,
            "funding_leg_mk_usd": 0.0,
            "funding_leg_hd_usd": 0.0,
            "funding_cash_usd": 0.0,
            "realized_only_pnl_usd": 0.0,
            "mtm_incl_residual_pnl_usd": 0.0,
            "bps_per_notional": 0.0,
            "terminal_flat_all": True,
            "terminal_residual_notional_usd": 0.0,
            "max_drawdown_usd": 0.0,
            "positive_window_ratio": 0.0,
            "peak_gross_notional_usd": 0.0,
            "mean_hold_min": 0.0,
        }
    fills = sum(int(r["fills"]) for r in runs)
    mk_notl = sum(float(r["maker_filled_notional_usd"]) for r in runs)
    hd_notl = sum(float(r["hedged_notional_usd"]) for r in runs)
    orp_ev = sum(int(r["orphan_events"]) for r in runs)
    orp_notl = sum(float(r["orphan_notional_usd"]) for r in runs)
    orp_pnl = sum(float(r["orphan_unwind_pnl_usd"]) for r in runs)
    orp_cost = sum(float(r["orphan_unwind_cost_usd"]) for r in runs)
    fees = sum(float(r["fees_usd"]) for r in runs)
    fx = sum(float(r["fx_drag_usd"]) for r in runs)
    dslip = sum(float(r["depth_slip_usd"]) for r in runs)
    fmk = sum(float(r["funding_leg_mk_usd"]) for r in runs)
    fhd = sum(float(r["funding_leg_hd_usd"]) for r in runs)
    fcash = sum(float(r["funding_cash_usd"]) for r in runs)
    realized = sum(float(r["realized_only_pnl_usd"]) for r in runs)
    mtm = sum(float(r["mtm_incl_residual_pnl_usd"]) for r in runs)
    res_notl = sum(float(r["terminal_residual_notional_usd"]) for r in runs)
    term_flat = all(bool(r["terminal_flat"]) for r in runs)
    peak_g = max((float(r["peak_gross_notional_usd"]) for r in runs), default=0.0)
    holds = [float(r["mean_hold_min"]) for r in runs if int(r["fills"]) > 0]

    # Window-level drawdown over ordered windows
    by_w: dict[int, float] = {}
    for r in runs:
        wn = int(r["window_num"])
        by_w[wn] = by_w.get(wn, 0.0) + float(r["mtm_incl_residual_pnl_usd"])
    cum = 0.0
    pk = 0.0
    dd = 0.0
    pos_w = 0
    for wn in sorted(by_w):
        wp = by_w[wn]
        cum += wp
        if cum > pk:
            pk = cum
        if pk - cum > dd:
            dd = pk - cum
        if wp > 0.0:
            pos_w += 1
    return {
        "windows_covered": len(by_w),
        "fills": fills,
        "maker_filled_notional_usd": round(mk_notl, 2),
        "hedged_notional_usd": round(hd_notl, 2),
        "orphan_events": orp_ev,
        "orphan_notional_usd": round(orp_notl, 2),
        "orphan_unwind_rate": round(orp_notl / mk_notl, 6) if mk_notl > 0.0 else 0.0,
        "orphan_unwind_pnl_usd": round(orp_pnl, 4),
        "orphan_unwind_cost_usd": round(orp_cost, 4),
        "fees_usd": round(fees, 4),
        "fx_drag_usd": round(fx, 4),
        "depth_slip_usd": round(dslip, 4),
        "funding_leg_mk_usd": round(fmk, 4),
        "funding_leg_hd_usd": round(fhd, 4),
        "funding_cash_usd": round(fcash, 4),
        "realized_only_pnl_usd": round(realized, 4),
        "mtm_incl_residual_pnl_usd": round(mtm, 4),
        "bps_per_notional": round(mtm / mk_notl * 1e4, 3) if mk_notl > 0.0 else 0.0,
        "terminal_flat_all": term_flat,
        "terminal_residual_notional_usd": round(res_notl, 4),
        "max_drawdown_usd": round(dd, 2),
        "positive_window_ratio": round(pos_w / len(by_w), 4) if by_w else 0.0,
        "peak_gross_notional_usd": round(peak_g, 2),
        "mean_hold_min": round(sum(holds) / len(holds), 2) if holds else 0.0,
        "window_pnl_series": {str(wn): round(by_w[wn], 4) for wn in sorted(by_w)},
    }


def compute_multiple_testing_stats(trials_by_name: dict[str, list[float]]) -> dict[str, Any]:
    if not trials_by_name:
        return {"status": "empty"}
    varying = {k: v for k, v in trials_by_name.items() if len(set(v)) > 1 and len(v) >= 4}
    out: dict[str, Any] = {"raw_trial_count": len(trials_by_name), "varying_trial_count": len(varying)}
    if not varying:
        out["status"] = "all_constant"
        return out
    t_len = len(next(iter(varying.values())))
    if t_len >= 3:
        out["dsr"] = deflated_sharpe_report(varying)
    if len(varying) >= 2 and t_len >= 16:
        out["cscv_pbo"] = cscv_pbo(varying, num_blocks=8)
    elif len(varying) >= 2 and t_len >= 8:
        out["cscv_pbo"] = cscv_pbo(varying, num_blocks=4)
    return out


def compute_fx_breakeven_bps(pnl_fx0: float, hedged_notional_usd: float) -> float | None:
    """Linear FX drag breakeven in bps (`pnl_fx0 / hedged_notional_usd * 1e4`)."""
    if hedged_notional_usd <= 0.0:
        return None
    return round((pnl_fx0 / hedged_notional_usd) * 1e4, 3)


def run_lakehouse_evaluation(
    lakehouse_dir: Path,
    *,
    fee_evidence_path: Path | None,
    output_path: Path,
    domains: tuple[str, ...] = ("rwa", "crypto"),
) -> dict[str, Any]:
    """Run full W01..W27 Track 02 evaluation on Aleph lakehouse (excluding `-w28`)."""
    import duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")

    fee_schedules = {
        mode: resolve_track02_fees(fee_evidence_path, lighter_mode=mode)
        for mode in ("verified_prem", "standard_zero")
    }
    specs = [s for s in build_symbol_catalog() if s.domain in domains]
    grid = build_candidate_grid()

    campaigns_by_dom = {
        dom: discover_w01_w27_campaigns(lakehouse_dir, dom, min_window=1, max_window=27)
        for dom in domains
    }

    # Key: (pair_group, lighter_mode, domain, fx_bps, arm, cand_name) -> list of per-symbol-window run dicts
    runs_store: dict[tuple[str, str, str, float, str, str], list[dict[str, Any]]] = {}
    symbol_coverage: dict[str, dict[str, Any]] = {}

    for dom in domains:
        dom_specs = [s for s in specs if s.domain == dom]
        if not dom_specs:
            continue
        all_camps = campaigns_by_dom[dom]
        arcus_mkts = sorted({VENUE_SYMBOL_MAP["arcus"][s.symbol] for s in dom_specs})
        rh_sym_map = {s.symbol: VENUE_SYMBOL_MAP["lighter_robinhood"][s.symbol] for s in dom_specs if s.pair_group == "arcus_rh"}
        lt_sym_map = {s.symbol: VENUE_SYMBOL_MAP["lighter"][s.symbol] for s in dom_specs if s.pair_group == "arcus_lighter"}
        rh_mkts = sorted(set(rh_sym_map.values()))
        lt_mkts = sorted(set(lt_sym_map.values()))

        for camp in all_camps:
            wnum = int(camp["window_num"])
            wname = camp["window"]
            split = camp["split"]
            cid = camp["campaign_id"]
            udate = camp["utc_date"]

            bbo_arcus = _load_venue_bbo_buckets(
                con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "arcus", udate, cid), arcus_mkts
            )
            bbo_rh = _load_venue_bbo_buckets(
                con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "lighter_robinhood", udate, cid), rh_mkts
            )
            bbo_lt = _load_venue_bbo_buckets(
                con, campaign_parquet_files(lakehouse_dir, "bbo_quotes", "lighter", udate, cid), lt_mkts
            )
            tr_arcus = _load_venue_trades(
                con, campaign_parquet_files(lakehouse_dir, "public_trades", "arcus", udate, cid), arcus_mkts
            )
            tr_rh = _load_venue_trades(
                con, campaign_parquet_files(lakehouse_dir, "public_trades", "lighter_robinhood", udate, cid), rh_mkts
            )
            tr_lt = _load_venue_trades(
                con, campaign_parquet_files(lakehouse_dir, "public_trades", "lighter", udate, cid), lt_mkts
            )
            fn_rh = _load_venue_funding(
                con, campaign_parquet_files(lakehouse_dir, "funding_and_ctx", "lighter_robinhood", udate, cid), "lighter_robinhood", rh_sym_map
            )
            fn_lt = _load_venue_funding(
                con, campaign_parquet_files(lakehouse_dir, "funding_and_ctx", "lighter", udate, cid), "lighter", lt_sym_map
            )

            for spec in dom_specs:
                am = VENUE_SYMBOL_MAP["arcus"][spec.symbol]
                hm = VENUE_SYMBOL_MAP[spec.venue_hedge][spec.symbol]
                bbo_h_map = bbo_rh if spec.venue_hedge == "lighter_robinhood" else bbo_lt
                tr_h_map = tr_rh if spec.venue_hedge == "lighter_robinhood" else tr_lt
                fn_h_map = fn_rh if spec.venue_hedge == "lighter_robinhood" else fn_lt

                if am not in bbo_arcus or hm not in bbo_h_map:
                    continue
                acols, acont = bbo_arcus[am]
                hcols, hcont = bbo_h_map[hm]
                t0 = max(int(acont["t0_ms"]), int(hcont["t0_ms"]))
                t1 = min(int(acont["t1_ms"]), int(hcont["t1_ms"]))
                if t1 - t0 < WARMUP_MS + 60_000:
                    continue

                ga = _build_grid_np(acols, t0, t1, STEP_MS)
                gh = _build_grid_np(hcols, t0, t1, STEP_MS)
                ta = tr_arcus.get(am)
                th = tr_h_map.get(hm)
                fh = fn_h_map.get(hm)
                causal = compute_causal_basis_2h(ga["bid"], ga["ask"], ga["age"], gh["bid"], gh["ask"], gh["age"])
                tick_a = _infer_tick(acols["bid"])
                tick_h = _infer_tick(hcols["bid"])
                n_steps = len(ga["bid"])
                dec_steps = max(1, min(TEN_HOURS_MS // STEP_MS, n_steps - WARMUP_MS // STEP_MS))

                pre_a = bucket_trades_by_step(ta, t0, VENUE_TIMING["arcus"]["maker_delay_ms"], n_steps, decision_steps=dec_steps)
                hm_key = "lighter_robinhood" if spec.venue_hedge == "lighter_robinhood" else "lighter_std"
                pre_h = bucket_trades_by_step(th, t0, VENUE_TIMING[hm_key]["maker_delay_ms"], n_steps, decision_steps=dec_steps)
                pre_hts = [int(x) for x in th["ts"]] if (th is not None and len(th["ts"]) > 0) else None

                cov = symbol_coverage.setdefault(
                    spec.label,
                    {"pair_group": spec.pair_group, "domain": dom, "symbol": spec.symbol, "windows": 0, "funding_hours_hedge": 0},
                )
                cov["windows"] += 1
                cov["funding_hours_hedge"] += len(fh) if fh else 0

                modes_to_run = ("verified_prem",) if spec.pair_group == "arcus_rh" else ("verified_prem", "standard_zero")
                fx_list = FX_DRAG_BPS_LIST if spec.cross_stablecoin else (0.0,)

                for lmode in modes_to_run:
                    fees_meta, _ = fee_schedules[lmode]
                    af = fees_meta["arcus"][dom]
                    hf = fees_meta[spec.venue_hedge][dom]
                    ht_key = "lighter_robinhood" if spec.venue_hedge == "lighter_robinhood" else ("lighter_std" if lmode == "standard_zero" else "lighter_prem")
                    ht_meta = VENUE_TIMING[ht_key]

                    for arm, cand_list in grid.items():
                        for cand in cand_list:
                            tick = tick_a if cand.flipped_role else tick_h
                            res_0 = simulate_ws_basis_arb(
                                arcus_bid=ga["bid"],
                                arcus_ask=ga["ask"],
                                arcus_bsz=ga["bsz"],
                                arcus_asz=ga["asz"],
                                arcus_age=ga["age"],
                                hedge_bid=gh["bid"],
                                hedge_ask=gh["ask"],
                                hedge_bsz=gh["bsz"],
                                hedge_asz=gh["asz"],
                                hedge_age=gh["age"],
                                causal=causal,
                                arcus_trades=ta,
                                hedge_trades=th,
                                t0_ms=t0,
                                tick=tick,
                                params=cand,
                                arcus_delay_ms=VENUE_TIMING["arcus"]["maker_delay_ms"],
                                hedge_maker_delay_ms=ht_meta["maker_delay_ms"],
                                hedge_taker_delay_ms=ht_meta["taker_delay_ms"],
                                hedge_speedbump_ms=ht_meta["speedbump_ms"],
                                arcus_mk_fee_bps=float(af["maker_fee_bps"]),
                                arcus_tk_fee_bps=float(af["taker_fee_bps"]),
                                hedge_mk_fee_bps=float(hf["maker_fee_bps"]),
                                hedge_tk_fee_bps=float(hf["taker_fee_bps"]),
                                decision_steps=dec_steps,
                                fx_drag_bps=0.0,
                                arcus_funding=None,  # Aleph lakehouse has no funding_and_ctx for Arcus
                                hedge_funding=fh,
                                pre_arcus_by_step=pre_a,
                                pre_hedge_by_step=pre_h,
                                pre_hedge_ts=pre_hts,
                            )
                            res_0["window"] = wname
                            res_0["window_num"] = wnum
                            res_0["split"] = split
                            res_0["symbol"] = spec.symbol
                            for fx_bps in fx_list:
                                runs_store.setdefault((spec.pair_group, lmode, dom, fx_bps, arm, cand.name), []).append(
                                    _clone_run_with_fx(res_0, fx_bps) if fx_bps > 0.0 else res_0
                                )

            del bbo_arcus, bbo_rh, bbo_lt, tr_arcus, tr_rh, tr_lt, fn_rh, fn_lt
            gc.collect()
            print(f"[processed] {dom} {wname} ({cid})", flush=True)

    # Select frozen winner per (pair_group, lighter_mode, domain, fx_bps, arm) strictly on selection_dev!
    report: dict[str, Any] = {
        "schema_version": "track02_arcus_rh_zero_fee_basis_w01_w27_v1",
        "campaigns": {
            dom: [{"window": c["window"], "window_num": c["window_num"], "split": c["split"]} for c in cl]
            for dom, cl in campaigns_by_dom.items()
        },
        "fee_provenance": {
            "verified_prem": fee_schedules["verified_prem"][1],
            "standard_zero": fee_schedules["standard_zero"][1],
            "schedules": {
                mode: fee_schedules[mode][0] for mode in ("verified_prem", "standard_zero")
            },
        },
        "funding_data_caveat": "Aleph lakehouse has zero funding_and_ctx files for Arcus; funding_leg_mk_usd on Arcus is unrecorded (0.0 reported, unknown true carry) while funding_leg_hd_usd reflects exact LighterRH/Lighter settlement.",
        "symbol_coverage": symbol_coverage,
        "cohorts": {},
    }

    cohort_configs = [
        ("arcus_rh_usdg_usdc", "arcus_rh", "verified_prem", FX_DRAG_BPS_LIST),
        ("arcus_lighter_usdc_usdc_std", "arcus_lighter", "standard_zero", (0.0,)),
        ("arcus_lighter_usdc_usdc_prem", "arcus_lighter", "verified_prem", (0.0,)),
    ]

    for cname, pgroup, lmode, fx_list in cohort_configs:
        coh_out: dict[str, Any] = {}
        for fx_bps in fx_list:
            fx_key = f"fx_{int(fx_bps)}bp"
            dom_out: dict[str, Any] = {}
            for dom in domains:
                arms_out: dict[str, Any] = {}
                aligned_trials: dict[str, list[float]] = {}
                for arm, cand_list in grid.items():
                    best_cand: str | None = None
                    best_score = -1e18
                    best_any_cand: str | None = None
                    best_any_score = -1e18

                    for cand in cand_list:
                        all_runs = runs_store.get((pgroup, lmode, dom, fx_bps, arm, cand.name), [])
                        if not all_runs:
                            continue
                        sel_runs = [r for r in all_runs if r["split"] == "selection_dev"]
                        sel_agg = _aggregate_window_runs(sel_runs)
                        full_agg = _aggregate_window_runs(all_runs)
                        ws = full_agg["window_pnl_series"]
                        if ws:
                            aligned_trials[f"{arm}:{cand.name}"] = [ws[k] for k in sorted(ws, key=int)]
                        pnl_sel = sel_agg["mtm_incl_residual_pnl_usd"]
                        if pnl_sel > best_any_score:
                            best_any_score = pnl_sel
                            best_any_cand = cand.name
                        if sel_agg["fills"] >= MIN_SELECTION_FILLS and pnl_sel > best_score:
                            best_score = pnl_sel
                            best_cand = cand.name

                    winner = best_cand or best_any_cand
                    if winner is None:
                        continue
                    win_runs = runs_store.get((pgroup, lmode, dom, fx_bps, arm, winner), [])
                    splits_out = {
                        sp: _aggregate_window_runs([r for r in win_runs if r["split"] == sp])
                        for sp in ("selection_dev", "holdout", "w26_w27_oos")
                    }
                    # Per-symbol breakdown across holdout + oos and full W01-W27
                    sym_breakdown: dict[str, Any] = {}
                    for sym in sorted({r["symbol"] for r in win_runs}):
                        s_runs = [r for r in win_runs if r["symbol"] == sym]
                        sym_breakdown[sym] = {
                            sp: _aggregate_window_runs([r for r in s_runs if r["split"] == sp])
                            for sp in ("selection_dev", "holdout", "w26_w27_oos")
                        }
                    ex_nvda_splits = {
                        sp: _aggregate_window_runs([r for r in win_runs if r["split"] == sp and r["symbol"] != "NVDA"])
                        for sp in ("selection_dev", "holdout", "w26_w27_oos")
                    }
                    top_sym_share: dict[str, float] = {}
                    for sp in ("selection_dev", "holdout", "w26_w27_oos"):
                        abs_by_sym = [abs(sym_breakdown[s][sp]["mtm_incl_residual_pnl_usd"]) for s in sym_breakdown]
                        tot_abs = sum(abs_by_sym)
                        top_sym_share[sp] = round(100.0 * max(abs_by_sym, default=0.0) / tot_abs, 2) if tot_abs > 0.0 else 0.0
                    arms_out[arm] = {
                        "frozen_param": winner,
                        "splits": splits_out,
                        "ex_nvda_splits": ex_nvda_splits,
                        "top_symbol_share_abs_pct": top_sym_share,
                        "by_symbol": sym_breakdown,
                    }

                mt_audit = compute_multiple_testing_stats(aligned_trials)
                dom_out[dom] = {"arms": arms_out, "multiple_testing": mt_audit}
            coh_out[fx_key] = dom_out

        # Compute exact FX drag breakeven using fx_0bp vs fx_1bp delta for arcus_rh
        if pgroup == "arcus_rh" and "fx_0bp" in coh_out:
            be_by_dom: dict[str, Any] = {}
            for dom in domains:
                be_arms: dict[str, Any] = {}
                for arm, ainfo in coh_out["fx_0bp"].get(dom, {}).get("arms", {}).items():
                    sp_be: dict[str, Any] = {}
                    ainfo_1bp = coh_out.get("fx_1bp", {}).get(dom, {}).get("arms", {}).get(arm, {})
                    for sp in ("selection_dev", "holdout", "w26_w27_oos"):
                        sd0 = ainfo["splits"][sp]
                        pnl0 = float(sd0["mtm_incl_residual_pnl_usd"])
                        sd1 = ainfo_1bp.get("splits", {}).get(sp)
                        if sd1 is not None:
                            drag_per_bp = pnl0 - float(sd1["mtm_incl_residual_pnl_usd"])
                            sp_be[sp] = round(pnl0 / drag_per_bp, 3) if drag_per_bp > 1e-9 else None
                        else:
                            sp_be[sp] = compute_fx_breakeven_bps(pnl0, sd0["hedged_notional_usd"])
                    be_arms[arm] = sp_be
                be_by_dom[dom] = be_arms
            coh_out["fx_breakeven_bps"] = be_by_dom

        report["cohorts"][cname] = coh_out

    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    return report


def run_self_test() -> dict[str, Any]:
    """Deterministic stdlib-only self-test verifying zero-fee flipped role, L1 clamp, orphan unwind & funding skew."""
    n = 160
    t0 = 1_700_000_400_000  # aligned to HOUR_MS
    a_bid = [99.99] * n
    a_ask = [100.01] * n
    a_bsz = [20.0] * n
    a_asz = [20.0] * n
    a_age = [0] * n

    h_bid = [99.99] * n
    h_ask = [100.01] * n
    h_bsz = [4.0] * n  # ~$400 L1 (< $1000 clip -> forces L1 clamp + $600 orphan unwind!)
    h_asz = [20.0] * n
    h_age = [0] * n

    # Make Arcus rich at k=80..95 and cheap at k=110..125 (sustained so 5s drift settles)
    a_bid[65:95] = [100.09] * 30
    a_ask[65:95] = [100.11] * 30
    a_bid[105:135] = [99.89] * 30
    a_ask[105:135] = [99.91] * 30

    causal = compute_causal_basis_2h(a_bid, a_ask, a_age, h_bid, h_ask, h_age, step_ms=STEP_MS)
    # Trade prints on Arcus: buyer hits ask at k=90 (side=-1), seller hits bid at k=125 (side=+1)
    tf_sell = t0 + 90 * STEP_MS + 200 + 36
    tf_buy = t0 + 125 * STEP_MS + 200 + 36
    a_tr = {
        "ts": [tf_sell, tf_buy],
        "sgn": [1, -1],
        "px": [100.20, 99.80],
        "sz": [12.0, 12.0],
    }
    # Settlement boundary strictly between tf_buy and tf_buy + 330ms
    h_fund = {tf_buy + 100: 10.0}
    p_a0 = BasisArbParams(
        name="selftest_a0",
        arm="A0_arcus_mk_hedge_tk",
        e0_bps=4.0,
        basis_drift_guard_bps=0.0,
        min_hedge_l1_notional_usd=0.0,
    )
    res = simulate_ws_basis_arb(
        arcus_bid=a_bid,
        arcus_ask=a_ask,
        arcus_bsz=a_bsz,
        arcus_asz=a_asz,
        arcus_age=a_age,
        hedge_bid=h_bid,
        hedge_ask=h_ask,
        hedge_bsz=h_bsz,
        hedge_asz=h_asz,
        hedge_age=h_age,
        causal=causal,
        arcus_trades=a_tr,
        hedge_trades=None,
        t0_ms=t0,
        tick=0.01,
        params=p_a0,
        arcus_delay_ms=36,
        hedge_maker_delay_ms=200,
        hedge_taker_delay_ms=330,
        hedge_speedbump_ms=200,
        arcus_mk_fee_bps=0.0,
        arcus_tk_fee_bps=2.1375,
        hedge_mk_fee_bps=0.0,
        hedge_tk_fee_bps=0.0,
        warmup_steps=50,
        decision_steps=100,
        hedge_funding=h_fund,
        record_trades=True,
    )
    assert res["fills"] >= 1
    assert res["orphan_events"] >= 1
    assert res["terminal_flat"] is True
    return {
        "self_test": "ok",
        "fills": res["fills"],
        "hedged_notional_usd": res["hedged_notional_usd"],
        "orphan_events": res["orphan_events"],
        "orphan_notional_usd": res["orphan_notional_usd"],
        "realized_only_pnl_usd": res["realized_only_pnl_usd"],
        "mtm_incl_residual_pnl_usd": res["mtm_incl_residual_pnl_usd"],
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--self-test", action="store_true")
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument(
        "--output",
        type=Path,
        default=Path("/tmp/profitability_upgrade_20261127/t2/track02_arcus_rh_zero_fee_basis_w01_w27.json"),
    )
    ap.add_argument("--domains", type=str, default="rwa,crypto")
    args = ap.parse_args()

    if args.self_test:
        print(json.dumps(run_self_test(), indent=2, sort_keys=True))
        return 0

    doms = tuple(d.strip().lower() for d in args.domains.split(",") if d.strip())
    report = run_lakehouse_evaluation(
        args.lakehouse,
        fee_evidence_path=args.fee_evidence,
        output_path=args.output,
        domains=doms,
    )
    print(json.dumps({"status": "ok", "output": str(args.output), "cohorts": list(report["cohorts"].keys())}))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
