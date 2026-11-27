#!/usr/bin/env python3
"""Track 02 (02_hedge_arbitrage): Stage A Attribution, Selective Z-Score & Passive-Then-IOC Arb Evaluator.

Implements and empirically evaluates the Track 02 strategy upgrades matching
`src/strategy/basis_maker_hedge.rs`:
1. Stage A Root-Cause Loss Attribution (`selection_dev` `w01..w20`) decomposing
   prior `upgraded_track2` losses into:
   - Maker post-fill adverse selection (`1s` and `5s` markouts)
   - Hedge latency drift (`hd_mid` move during hedge delay)
   - Hedge crossing half-spread (`hd_half_spread`) + L1 sweep slip (`depth_slip`)
   - Maker/hedge fee drag
   - Non-stationary basis drift & unfavorable-side bleeding
   - Maker/cancel latency sensitivity (`{0, 100, 200, 300} ms`)
2. Stage B Round-2 Iterative Optimization on `selection_dev` -> freeze -> ONE-SHOT
   validation on untouched `holdout (w21..w25)` and `w26_oos` across 4 arms:
   - `T2_taker_hedge` (prior `upgraded_track2` baseline)
   - `T2_zscore_selective` (Selective Basis Z-Score & Stationarity Gate + Microprice)
   - `T2_passive_then_ioc` (Passive-First-Then-IOC Hybrid Hedge FSM)
   - `T2_zscore_plus_passive_ioc` (Combined Z-Score + Stationarity + Microprice + Passive-IOC)

Strictly enforces:
- Lighter Standard `0/0 bps` scenario (`lighter_standard_zero_scenario`) plus `OPT-01`
  verified fees for `arcus` (`1.425/4.275` crypto, `0/2.1375` RWA), `extended`
  (`0/2.25` crypto, `0/0.9` RWA), and `lighter_robinhood` (`0/0` confirmed).
- Strict print-verification via `public_trades` for BOTH maker fills AND passive
  hedge fills (zero fabricated passive hedge fills).
- Strict separation of `USDC/USDC` WS distinct-domain, `USDG/USDC` WS distinct-domain
  (`0/1/2 bp` FX), and `same_risk_domain` (`lighter <-> lighter_robinhood`).
- 100% Python stdlib top-level imports (`duckdb`/`numpy` loaded lazily inside
  lakehouse loader functions only).
"""

from __future__ import annotations

import argparse
from bisect import bisect_left
import concurrent.futures
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

from track_fee_evidence import (  # noqa: E402
    VENUE_CAPABILITY_SNAPSHOT_VERSION,
    build_fee_provenance,
    load_track_fee_evidence,
    resolve_venue_fee,
)

TEN_HOURS_MS = 36_000_000
MAX_CONTINUOUS_GAP_MS = 60_000
STEP_MS = 100
WARMUP_MS = 15 * 60_000
MAX_BOOK_AGE_MS = 5_000
CLIP_USD = 1_000.0
Q_MAX_CLIPS = 4.0
DEFAULT_SKEW_BPS = 0.5
MIN_SELECTION_FILLS_CRYPTO = 15
MIN_SELECTION_FILLS_RWA = 5
LATENCIES_MS: tuple[int, ...] = (0, 100, 200, 300)

VENUE_SYMBOL_MAP: dict[str, dict[str, str]] = {
    "arcus": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
        "NVDA": "NVDA-USD",
        "SPY": "SPY-USD",
        "QQQ": "QQQ-USD",
        "AAPL": "AAPL-USD",
        "TSLA": "TSLA-USD",
    },
    "lighter": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "2",
        "NVDA": "110",
        "TSLA": "112",
        "AAPL": "113",
        "SPY": "128",
        "QQQ": "129",
    },
    "lighter_robinhood": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "3",
        "AAPL": "10",
        "NVDA": "15",
        "TSLA": "16",
        "QQQ": "25",
        "SPY": "26",
    },
    "extended": {
        "BTC": "BTC-USD",
        "ETH": "ETH-USD",
        "SOL": "SOL-USD",
    },
    "binance": {
        "BTC": "BTCUSDT",
        "ETH": "ETHUSDT",
        "SOL": "SOLUSDT",
    },
}

VENUE_META: dict[str, dict[str, Any]] = {
    "arcus": {
        "settlement": "USDC",
        "order_delay_ms": 40,
        "speedbump_ms": 0,
        "maker_fee_bps": {"crypto": 1.425, "rwa": 0.0},
        "taker_fee_bps": {"crypto": 4.275, "rwa": 2.1375},
        "fee_status": "verified_account_tier_fallback",
    },
    "lighter": {
        "settlement": "USDC",
        "order_delay_ms": 300,
        "speedbump_ms": 300,
        "maker_fee_bps": {"crypto": 0.0, "rwa": 0.0},
        "taker_fee_bps": {"crypto": 0.0, "rwa": 0.0},
        "fee_status": "lighter_standard_zero_scenario",
    },
    "lighter_robinhood": {
        "settlement": "USDG",
        "order_delay_ms": 200,
        "speedbump_ms": 200,
        "maker_fee_bps": {"crypto": 0.0, "rwa": 0.0},
        "taker_fee_bps": {"crypto": 0.0, "rwa": 0.0},
        "fee_status": "verified_account_tier_standard_zero",
    },
    "extended": {
        "settlement": "USDC",
        "order_delay_ms": 60,
        "speedbump_ms": 0,
        "maker_fee_bps": {"crypto": 0.0, "rwa": 0.0},
        "taker_fee_bps": {"crypto": 2.25, "rwa": 0.90},
        "fee_status": "verified_account_tier_fallback",
    },
}


@dataclass(frozen=True)
class ArbPairSpec:
    cohort: str
    maker_venue: str
    hedge_venue: str
    ref_venue: str
    symbol: str
    domain: str
    label: str

    @property
    def cross_stablecoin(self) -> bool:
        return VENUE_META[self.maker_venue]["settlement"] != VENUE_META[self.hedge_venue]["settlement"]

    @property
    def same_risk_domain(self) -> bool:
        return self.maker_venue.startswith("lighter") and self.hedge_venue.startswith("lighter")


def build_pair_catalog() -> list[ArbPairSpec]:
    """Build WS multi-venue arb pairs including zero-fee Lighter Standard hedge legs."""
    pairs: list[ArbPairSpec] = []
    for sym in ("BTC", "ETH", "SOL"):
        pairs.extend([
            ArbPairSpec("USDC/USDC", "lighter", "arcus", "binance", sym, "crypto", f"lighter->arcus {sym}"),
            ArbPairSpec("USDC/USDC", "arcus", "lighter", "binance", sym, "crypto", f"arcus->lighter {sym}"),
            ArbPairSpec("USDC/USDC", "lighter", "extended", "binance", sym, "crypto", f"lighter->extended {sym}"),
            ArbPairSpec("USDC/USDC", "extended", "lighter", "binance", sym, "crypto", f"extended->lighter {sym}"),
            ArbPairSpec("USDC/USDC", "arcus", "extended", "binance", sym, "crypto", f"arcus->extended {sym}"),
            ArbPairSpec("USDC/USDC", "extended", "arcus", "binance", sym, "crypto", f"extended->arcus {sym}"),
            ArbPairSpec("USDG/USDC", "lighter_robinhood", "arcus", "binance", sym, "crypto", f"rh->arcus {sym}"),
            ArbPairSpec("USDG/USDC", "arcus", "lighter_robinhood", "binance", sym, "crypto", f"arcus->rh {sym}"),
            ArbPairSpec("USDG/USDC", "lighter_robinhood", "lighter", "binance", sym, "crypto", f"rh->lighter {sym}"),
            ArbPairSpec("USDG/USDC", "lighter", "lighter_robinhood", "binance", sym, "crypto", f"lighter->rh {sym}"),
        ])
    for sym in ("NVDA", "QQQ", "SPY", "AAPL", "TSLA"):
        pairs.extend([
            ArbPairSpec("USDC/USDC", "lighter", "arcus", "lighter_robinhood", sym, "rwa", f"lighter->arcus {sym}"),
            ArbPairSpec("USDC/USDC", "arcus", "lighter", "lighter_robinhood", sym, "rwa", f"arcus->lighter {sym}"),
            ArbPairSpec("USDG/USDC", "lighter_robinhood", "arcus", "lighter", sym, "rwa", f"rh->arcus {sym}"),
            ArbPairSpec("USDG/USDC", "arcus", "lighter_robinhood", "lighter", sym, "rwa", f"arcus->rh {sym}"),
            ArbPairSpec("USDG/USDC", "lighter_robinhood", "lighter", "arcus", sym, "rwa", f"rh->lighter {sym}"),
            ArbPairSpec("USDG/USDC", "lighter", "lighter_robinhood", "arcus", sym, "rwa", f"lighter->rh {sym}"),
        ])
    return pairs


def resolve_venue_meta(
    fee_evidence_path: Path | None = None,
    *,
    lighter_fee_mode: str = "standard_zero",
) -> tuple[dict[str, dict[str, Any]], dict[str, Any], dict[str, Any]]:
    """Resolve venue fees under Lighter Standard 0/0 (default) or verified Premium 0.4/2.8."""
    ev = load_track_fee_evidence(fee_evidence_path)
    resolved: dict[str, dict[str, Any]] = {}
    for venue, base_meta in VENUE_META.items():
        vm = {
            **base_meta,
            "maker_fee_bps": dict(base_meta["maker_fee_bps"]),
            "taker_fee_bps": dict(base_meta["taker_fee_bps"]),
        }
        if venue == "lighter" and lighter_fee_mode == "standard_zero":
            vm["maker_fee_bps"] = {"crypto": 0.0, "rwa": 0.0}
            vm["taker_fee_bps"] = {"crypto": 0.0, "rwa": 0.0}
            vm["fee_status"] = "lighter_standard_zero_scenario"
            vm["fee_source"] = {"crypto": "lighter_standard_zero_scenario", "rwa": "lighter_standard_zero_scenario"}
            resolved[venue] = vm
            continue

        c_mk, c_tk, c_st, c_src = resolve_venue_fee(
            venue,
            "crypto",
            default_maker_bps=float(vm["maker_fee_bps"]["crypto"]),
            default_taker_bps=float(vm["taker_fee_bps"]["crypto"]),
            default_status=str(vm["fee_status"]),
            evidence=ev,
        )
        r_mk, r_tk, r_st, r_src = resolve_venue_fee(
            venue,
            "rwa",
            default_maker_bps=float(vm["maker_fee_bps"]["rwa"]),
            default_taker_bps=float(vm["taker_fee_bps"]["rwa"]),
            default_status=str(vm["fee_status"]),
            evidence=ev,
        )
        vm["maker_fee_bps"] = {"crypto": c_mk, "rwa": r_mk}
        vm["taker_fee_bps"] = {"crypto": c_tk, "rwa": r_tk}
        vm["fee_status"] = c_st
        vm["fee_source"] = {"crypto": c_src, "rwa": r_src}
        resolved[venue] = vm
    prov = build_fee_provenance(ev, venues_in_scope=tuple(VENUE_META.keys()))
    prov["lighter_fee_mode"] = lighter_fee_mode
    return resolved, ev, prov


def resolve_pair_fees(
    pair: ArbPairSpec,
    venue_meta: dict[str, dict[str, Any]],
    evidence: dict[str, Any] | None = None,
    *,
    lighter_fee_mode: str = "standard_zero",
) -> dict[str, Any]:
    """Resolve maker/taker/hedge maker/hedge taker fees for a pair."""
    mm = venue_meta[pair.maker_venue]
    hm = venue_meta[pair.hedge_venue]

    def _get(v: str, meta: dict[str, Any]) -> tuple[float, float, str, str]:
        if v == "lighter" and lighter_fee_mode == "standard_zero":
            return 0.0, 0.0, "lighter_standard_zero_scenario", "lighter_standard_zero_scenario"
        raw_mkt = VENUE_SYMBOL_MAP.get(v, {}).get(pair.symbol)
        return resolve_venue_fee(
            v,
            pair.domain,
            default_maker_bps=float(meta["maker_fee_bps"][pair.domain]),
            default_taker_bps=float(meta["taker_fee_bps"][pair.domain]),
            default_status=str(meta["fee_status"]),
            evidence=evidence,
            raw_market=raw_mkt,
        )

    mk_mk, mk_tk, mk_st, mk_src = _get(pair.maker_venue, mm)
    hd_mk, hd_tk, hd_st, hd_src = _get(pair.hedge_venue, hm)
    return {
        "maker_fee_bps": mk_mk,
        "maker_taker_fee_bps": mk_tk,
        "hedge_maker_fee_bps": hd_mk,
        "hedge_taker_fee_bps": hd_tk,
        "maker_fee_status": mk_st,
        "hedge_fee_status": hd_st,
        "maker_fee_source": mk_src,
        "hedge_fee_source": hd_src,
    }


@dataclass(frozen=True)
class PassiveHedgeParams:
    enabled: bool = False
    max_passive_hedge_ms: int = 1_000
    hazard_ioc_adverse_bps: float = 2.0
    max_hedge_book_age_ms: int = MAX_BOOK_AGE_MS
    improve_one_tick: bool = False
    reduce_only: bool = True


@dataclass(frozen=True)
class StrategyParams:
    name: str
    arm: str
    e0_bps: float = 4.0
    tau_s: float = 1800.0
    skew_bps_per_clip: float = DEFAULT_SKEW_BPS
    nonlinear_skew_power: float = 2.0
    vol_edge_mult: float = 0.5
    basis_vol_tau_s: float = 300.0
    basis_drift_guard_bps: float = 3.0
    basis_drift_tau_s: float = 5.0
    min_hedge_l1_notional_usd: float = CLIP_USD
    lead_beta: float = 0.0
    improve_one_tick: bool = False
    shield_bps: float | None = None
    depth_impact_bps: float = 2.0
    min_basis_zscore: float = 0.0
    basis_vol_floor_bps: float = 0.0
    max_drift_to_vol_ratio: float = 0.0
    quote_favorable_side_only: bool = False
    use_microprice_anchor: bool = False
    passive_hedge: PassiveHedgeParams = PassiveHedgeParams()


def build_round2_arms(has_speedbump: bool) -> dict[str, list[StrategyParams]]:
    """Build apples-to-apples Round-2 candidate sets for the 4 Track 02 arms on `selection_dev`."""
    sh = 2.5 if has_speedbump else None
    t2_taker = [
        StrategyParams(
            name=f"taker_e{int(e0)}_v{vm}",
            arm="T2_taker_hedge",
            e0_bps=e0,
            vol_edge_mult=vm,
            shield_bps=sh,
        )
        for e0 in (2.0, 4.0, 6.0)
        for vm in (0.5, 1.0)
    ]

    t2_zscore: list[StrategyParams] = []
    for e0 in (4.0, 6.0):
        for z in (1.5, 2.5):
            for l1_min in (1_000.0, 2_500.0):
                for micro in (False, True):
                    t2_zscore.append(
                        StrategyParams(
                            name=f"z{z}_e{int(e0)}_l1{int(l1_min)}_m{int(micro)}",
                            arm="T2_zscore_selective",
                            e0_bps=e0,
                            vol_edge_mult=1.0,
                            basis_drift_guard_bps=0.0,
                            min_hedge_l1_notional_usd=l1_min,
                            lead_beta=0.0,
                            shield_bps=sh,
                            min_basis_zscore=z,
                            basis_vol_floor_bps=2.0,
                            max_drift_to_vol_ratio=1.5,
                            quote_favorable_side_only=True,
                            use_microprice_anchor=micro,
                        )
                    )

    t2_passive: list[StrategyParams] = []
    for e0 in (4.0, 6.0):
        for max_ms in (200, 500, 1_000):
            for haz in (0.75, 1.5):
                for imp in (False, True):
                    t2_passive.append(
                        StrategyParams(
                            name=f"pas_e{int(e0)}_t{max_ms}_h{haz}_i{int(imp)}",
                            arm="T2_passive_then_ioc",
                            e0_bps=e0,
                            vol_edge_mult=1.0,
                            shield_bps=sh,
                            passive_hedge=PassiveHedgeParams(
                                enabled=True,
                                max_passive_hedge_ms=max_ms,
                                hazard_ioc_adverse_bps=haz,
                                improve_one_tick=imp,
                                reduce_only=True,
                            ),
                        )
                    )

    t2_combo: list[StrategyParams] = []
    for e0 in (4.0, 6.0):
        for z in (1.5, 2.5):
            for l1_min in (1_000.0, 2_500.0):
                for max_ms in (200, 500):
                    for haz in (0.75, 1.5):
                        t2_combo.append(
                            StrategyParams(
                                name=f"comb_e{int(e0)}_z{z}_l1{int(l1_min)}_t{max_ms}_h{haz}",
                                arm="T2_zscore_plus_passive_ioc",
                                e0_bps=e0,
                                vol_edge_mult=1.0,
                                basis_drift_guard_bps=0.0,
                                min_hedge_l1_notional_usd=l1_min,
                                lead_beta=0.0,
                                shield_bps=sh,
                                min_basis_zscore=z,
                                basis_vol_floor_bps=2.0,
                                max_drift_to_vol_ratio=1.5,
                                quote_favorable_side_only=True,
                                use_microprice_anchor=True,
                                passive_hedge=PassiveHedgeParams(
                                    enabled=True,
                                    max_passive_hedge_ms=max_ms,
                                    hazard_ioc_adverse_bps=haz,
                                    improve_one_tick=True,
                                    reduce_only=True,
                                ),
                            )
                        )

    return {
        "T2_taker_hedge": t2_taker,
        "T2_zscore_selective": t2_zscore,
        "T2_passive_then_ioc": t2_passive,
        "T2_zscore_plus_passive_ioc": t2_combo,
    }


def split_for_window(w_num: int) -> str:
    """Assign w01..w20 -> selection_dev, w21..w25 -> holdout, w26 -> w26_oos."""
    if w_num <= 20:
        return "selection_dev"
    if w_num <= 25:
        return "holdout"
    return "w26_oos"


def discover_campaigns(
    lakehouse_dir: Path, domain: str, *, min_window: int = 1, max_window: int = 26
) -> list[dict[str, Any]]:
    """Discover all campaign directories across W01..W26."""
    pat = str(
        lakehouse_dir
        / "parquet"
        / "bbo_quotes"
        / "collector_region=tokyo"
        / "venue=*"
        / "utc_date=*"
        / f"campaign_id=tokyo-{domain}-*-w*"
    )
    by_w: dict[int, dict[str, Any]] = {}
    for d in glob.glob(pat):
        cid = d.rsplit("campaign_id=", 1)[1]
        m = re.search(rf"^tokyo-{re.escape(domain)}-(\d{{8}})T\d+Z-w(\d+)$", cid)
        if not m:
            continue
        ymd, w_str = m.group(1), m.group(2)
        w_num = int(w_str)
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
    return [by_w[w] for w in sorted(by_w.keys())]


def campaign_parquet_files(
    lakehouse_dir: Path, dataset: str, venue: str, utc_date: str, campaign_id: str
) -> list[str]:
    """Cross-midnight Hive parquet globbing over `utc_date` and `utc_date + 1`."""
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


def _infer_tick(prices: Sequence[float]) -> float:
    best = 1.0
    for px in prices[:1000]:
        f = float(px)
        if f > 0.0 and math.isfinite(f):
            s = f"{f:.8f}".rstrip("0").rstrip(".")
            if "." in s:
                best = min(best, 10.0 ** (-len(s.split(".")[1])))
    return max(best, 1e-6)


def l1_microprice(bid: float, ask: float, bid_sz: float, ask_sz: float) -> float:
    """Compute Stoikov L1 microprice with safe fallback to mid."""
    if (
        bid > 0.0
        and ask > bid
        and math.isfinite(bid)
        and math.isfinite(ask)
        and math.isfinite(bid_sz)
        and math.isfinite(ask_sz)
        and bid_sz > 0.0
        and ask_sz > 0.0
    ):
        tot = bid_sz + ask_sz
        if tot > 0.0 and math.isfinite(tot):
            return (bid * ask_sz + ask * bid_sz) / tot
    return 0.5 * (bid + ask)


def compute_causal_basis_series(
    mk_bid: Sequence[float],
    mk_ask: Sequence[float],
    mk_bid_sz: Sequence[float],
    mk_ask_sz: Sequence[float],
    mk_age: Sequence[int],
    hd_bid: Sequence[float],
    hd_ask: Sequence[float],
    hd_bid_sz: Sequence[float],
    hd_ask_sz: Sequence[float],
    hd_age: Sequence[int],
    *,
    use_microprice_anchor: bool = False,
    step_ms: int = STEP_MS,
    tau_ms: int = 1_800_000,
    vol_tau_ms: int = 300_000,
    drift_tau_ms: int = 5_000,
    max_age_steps: int = MAX_BOOK_AGE_MS // STEP_MS,
    max_gap_steps: int = MAX_CONTINUOUS_GAP_MS // STEP_MS,
) -> dict[str, list[float]]:
    """Causal basis, fair EMA, sigma (bps), and fast EMA matching Rust `MakerHedgeQuoter`."""
    n = len(mk_bid)
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
        if mk_age[k] > max_age_steps or hd_age[k] > max_age_steps:
            continue
        mb, ma = float(mk_bid[k]), float(mk_ask[k])
        hb, ha = float(hd_bid[k]), float(hd_ask[k])
        if not (mb > 0.0 and ma > mb and hb > 0.0 and ha > hb):
            continue
        if use_microprice_anchor:
            mk_a = l1_microprice(mb, ma, float(mk_bid_sz[k]), float(mk_ask_sz[k]))
            hd_a = l1_microprice(hb, ha, float(hd_bid_sz[k]), float(hd_ask_sz[k]))
        else:
            mk_a = 0.5 * (mb + ma)
            hd_a = 0.5 * (hb + ha)
        b = log(mk_a) - log(hd_a)
        raw_b[k] = b
        if fair is not None and last_fresh_k is not None and (k - last_fresh_k) <= max_gap_steps:
            dt_ms = (k - last_fresh_k) * step_ms
            prior_fair[k] = fair
            prior_sigma_bps[k] = sqrt(var_bps2) if (var_bps2 is not None and var_bps2 > 0.0) else 0.0
            prior_fast[k] = fast if fast is not None else fair
            alpha = 1.0 - exp(-dt_ms / max(tau_ms, 1))
            alpha_v = 1.0 - exp(-dt_ms / max(vol_tau_ms, 1))
            alpha_d = 1.0 - exp(-dt_ms / max(drift_tau_ms, 1))
            dev_bps = (b - fair) * 1e4
            var_bps2 = dev_bps * dev_bps if var_bps2 is None else (1.0 - alpha_v) * var_bps2 + alpha_v * dev_bps * dev_bps
            fast = b if fast is None else fast + alpha_d * (b - fast)
            fair = fair + alpha * (b - fair)
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
    delay_ms: int,
    n_steps: int,
    *,
    step_ms: int = STEP_MS,
    start_step: int = 0,
    end_step: int | None = None,
) -> dict[int, list[int]]:
    """Bucket trade indices by causal step `k`."""
    if trades is None or len(trades["ts"]) == 0:
        return {}
    tr_ts = trades["ts"]
    lim = n_steps if end_step is None else min(end_step, n_steps)
    by_step: dict[int, list[int]] = {}
    for idx_t in range(len(tr_ts)):
        k = int((int(tr_ts[idx_t]) - delay_ms - t0_ms) // step_ms) - 1
        if start_step <= k < lim:
            by_step.setdefault(k, []).append(idx_t)
    return by_step


def simulate_pair_arb(
    *,
    mk_bid: Sequence[float],
    mk_ask: Sequence[float],
    mk_age: Sequence[int],
    hd_bid: Sequence[float],
    hd_ask: Sequence[float],
    hd_bid_sz: Sequence[float],
    hd_ask_sz: Sequence[float],
    hd_age: Sequence[int],
    ref_mid: Sequence[float],
    ref_age: Sequence[int],
    causal: dict[str, list[float]],
    mk_trades: dict[str, Sequence[Any]] | None,
    hd_trades: dict[str, Sequence[Any]] | None,
    t0_ms: int,
    tick: float,
    hd_tick: float,
    params: StrategyParams,
    maker_delay_ms: int,
    cancel_latency_ms: int,
    hedge_delay_ms: int,
    speedbump_ms: int,
    rtt_ms: int,
    maker_fee_bps: float,
    maker_taker_fee_bps: float,
    hedge_maker_fee_bps: float,
    hedge_taker_fee_bps: float,
    step_ms: int = STEP_MS,
    warmup_steps: int = WARMUP_MS // STEP_MS,
    decision_steps: int = TEN_HOURS_MS // STEP_MS,
    max_age_steps: int = MAX_BOOK_AGE_MS // STEP_MS,
    clip_usd: float = CLIP_USD,
    q_max_clips: float = Q_MAX_CLIPS,
    cross_stablecoin_bps: float = 0.0,
    maker_fr_bps_hr: float | None = None,
    hedge_fr_bps_hr: float | None = None,
    pre_mk_by_step: dict[int, list[int]] | None = None,
    pre_hd_by_step: dict[int, list[int]] | None = None,
    pre_hp_ts: list[int] | None = None,
    collect_attribution: bool = False,
) -> dict[str, Any]:
    """Simulate causal Track 02 arb with Z-score/Stationarity Gate & Passive-First-Then-IOC Hedge."""
    n = len(mk_bid)
    empty: dict[str, Any] = {
        "fills": 0,
        "fills_bid": 0,
        "fills_ask": 0,
        "maker_notional_usd": 0.0,
        "quote_hedged_pnl_usd": 0.0,
        "print_verified_pnl_usd": None,
        "hedge_print_checked": 0,
        "hedge_print_coverage_pct": 0.0,
        "fees_usd": 0.0,
        "fx_drag_usd": 0.0,
        "depth_slip_usd": 0.0,
        "passive_hedge_qty": 0.0,
        "ioc_hedge_qty": 0.0,
        "passive_hedge_fill_pct": 0.0,
        "passive_escalate_hazard": 0,
        "passive_escalate_timeout": 0,
        "passive_fail_closed_halts": 0,
        "blocked_toxic": 0,
        "blocked_drift": 0,
        "blocked_l1_thin": 0,
        "blocked_zscore": 0,
        "blocked_nonstationary": 0,
        "unhedged_stale_halts": 0,
        "terminal_drain_usd": 0.0,
        "funding_carry_usd": 0.0,
        "pnl_plus_funding_usd": 0.0,
        "funding_available_legs": int(maker_fr_bps_hr is not None) + int(hedge_fr_bps_hr is not None),
    }
    if n < 2 or mk_trades is None or len(mk_trades["ts"]) == 0:
        return empty

    raw_b = causal["raw_b"]
    prior_fair = causal["prior_fair"]
    prior_sigma = causal["prior_sigma_bps"]
    prior_fast = causal["prior_fast"]

    tr_ts = mk_trades["ts"]
    tr_sgn = mk_trades["sgn"]
    tr_px = mk_trades["px"]
    tr_sz = mk_trades["sz"]

    last_step_lim = min(warmup_steps + decision_steps, n)
    mk_by_step = (
        pre_mk_by_step
        if pre_mk_by_step is not None
        else bucket_trades_by_step(
            mk_trades,
            t0_ms,
            maker_delay_ms,
            n,
            step_ms=step_ms,
            start_step=warmup_steps,
            end_step=last_step_lim,
        )
    )

    if hd_trades is not None and len(hd_trades["ts"]) > 0:
        hp_ts = pre_hp_ts if pre_hp_ts is not None else [int(x) for x in hd_trades["ts"]]
        hp_sgn = hd_trades["sgn"]
        hp_px = hd_trades["px"]
        hp_sz = hd_trades["sz"]
        hd_by_step = (
            pre_hd_by_step
            if pre_hd_by_step is not None
            else bucket_trades_by_step(
                hd_trades,
                t0_ms,
                0,
                n,
                step_ms=step_ms,
                start_step=warmup_steps,
                end_step=n,
            )
        )
    else:
        hp_ts = None
        hp_sgn = None
        hp_px = None
        hp_sz = None
        hd_by_step = {}

    pos_mk = 0.0
    pos_hd = 0.0
    cash = 0.0
    fees_usd = 0.0
    mk_fees_usd = 0.0
    hd_fees_usd = 0.0
    fx_drag_usd = 0.0
    depth_slip_usd = 0.0
    mk_notional = 0.0
    fills_bid = 0
    fills_ask = 0
    blocked_toxic = 0
    blocked_drift = 0
    blocked_l1_thin = 0
    blocked_zscore = 0
    blocked_nonstationary = 0
    unhedged_stale_halts = 0
    passive_hedge_qty = 0.0
    ioc_hedge_qty = 0.0
    passive_esc_hazard = 0
    passive_esc_timeout = 0
    passive_fail_closed = 0
    print_delta = 0.0
    print_checked = 0

    # Stage A attribution accumulators
    attr_gross_edge_usd = 0.0
    attr_hd_lat_drift_usd = 0.0
    attr_hd_half_spr_usd = 0.0
    attr_mk_1s_usd = 0.0
    attr_mk_5s_usd = 0.0
    attr_mk_1s_cnt = 0
    attr_mk_5s_cnt = 0
    attr_fav_hz_pnl = 0.0
    attr_fav_hz_n = 0
    attr_fav_lz_pnl = 0.0
    attr_fav_lz_n = 0
    attr_unfav_pnl = 0.0
    attr_unfav_n = 0
    attr_stat_pnl = 0.0
    attr_stat_n = 0
    attr_nonstat_pnl = 0.0
    attr_nonstat_n = 0

    last_carry_k = warmup_steps
    mk_usd_hours = 0.0
    hd_usd_hours = 0.0

    def _accumulate_carry(curr_k: int) -> None:
        nonlocal last_carry_k, mk_usd_hours, hd_usd_hours
        if curr_k <= last_carry_k:
            return
        dt_h = (curr_k - last_carry_k) * step_ms / 3_600_000.0
        if pos_mk != 0.0:
            mk_m = 0.5 * (float(mk_bid[curr_k]) + float(mk_ask[curr_k]))
            if mk_m > 0.0 and math.isfinite(mk_m):
                mk_usd_hours += pos_mk * mk_m * dt_h
        if pos_hd != 0.0:
            hd_m = 0.5 * (float(hd_bid[curr_k]) + float(hd_ask[curr_k]))
            if hd_m > 0.0 and math.isfinite(hd_m):
                hd_usd_hours += pos_hd * hd_m * dt_h
        last_carry_k = curr_k

    def _ref_idx(t_ms: int) -> int:
        return min(max(int((t_ms - t0_ms) // step_ms) - 1, 0), n - 1)

    end_k = min(warmup_steps + decision_steps - 1, n - 1)

    for k in sorted(mk_by_step.keys()):
        if mk_age[k] > max_age_steps or hd_age[k] > max_age_steps:
            continue
        fb = prior_fair[k]
        b_now = raw_b[k]
        if not (math.isfinite(fb) and math.isfinite(b_now)):
            continue
        mb, ma = float(mk_bid[k]), float(mk_ask[k])
        hb, ha = float(hd_bid[k]), float(hd_ask[k])
        if not (mb > 0.0 and ma > mb and hb > 0.0 and ha > hb):
            continue
        _accumulate_carry(k)

        mk_mid_k = 0.5 * (mb + ma)
        hd_mid_k = 0.5 * (hb + ha)
        q = pos_mk * mk_mid_k / clip_usd

        sigma_k = prior_sigma[k]
        eff_e0 = params.e0_bps + (params.vol_edge_mult * sigma_k if params.vol_edge_mult > 0.0 else 0.0)
        sk_q = _skew_q(q, q_max_clips, params.nonlinear_skew_power)

        drift_bps = (b_now - prior_fast[k]) * 1e4 if math.isfinite(prior_fast[k]) else 0.0
        drift_block_bid = False
        drift_block_ask = False
        if params.basis_drift_guard_bps > 0.0 and math.isfinite(prior_fast[k]):
            drift_block_bid = q >= 0.0 and drift_bps <= -params.basis_drift_guard_bps
            drift_block_ask = q <= 0.0 and drift_bps >= params.basis_drift_guard_bps

        l1_block_bid = False
        l1_block_ask = False
        if params.min_hedge_l1_notional_usd > 0.0:
            hb_sz = float(hd_bid_sz[k])
            ha_sz = float(hd_ask_sz[k])
            hb_notl = hb * hb_sz if math.isfinite(hb_sz) else 0.0
            ha_notl = ha * ha_sz if math.isfinite(ha_sz) else 0.0
            l1_block_bid = q >= 0.0 and hb_notl < params.min_hedge_l1_notional_usd
            l1_block_ask = q <= 0.0 and ha_notl < params.min_hedge_l1_notional_usd

        # Selective Basis Z-Score & Stationarity Gate (opening sides only)
        disloc_bps = (b_now - fb) * 1e4
        denom_bps = max(sigma_k, params.basis_vol_floor_bps)
        z_ready = math.isfinite(denom_bps) and denom_bps > 1e-9

        nonstationary = False
        if params.max_drift_to_vol_ratio > 0.0:
            if math.isfinite(prior_fast[k]) and z_ready:
                trend_bps = abs(prior_fast[k] - fb) * 1e4
                nonstationary = (trend_bps / denom_bps) > params.max_drift_to_vol_ratio
            else:
                nonstationary = True

        z_block_bid = False
        z_block_ask = False
        if params.min_basis_zscore > 0.0:
            if not z_ready:
                z_block_bid = q >= 0.0
                z_block_ask = q <= 0.0
            else:
                z_val = disloc_bps / denom_bps
                z_block_bid = q >= 0.0 and z_val > -params.min_basis_zscore
                z_block_ask = q <= 0.0 and z_val < params.min_basis_zscore
        elif params.quote_favorable_side_only:
            z_block_bid = q >= 0.0 and disloc_bps >= 0.0
            z_block_ask = q <= 0.0 and disloc_bps <= 0.0

        stat_block_bid = nonstationary and q >= 0.0
        stat_block_ask = nonstationary and q <= 0.0

        shift = math.exp(params.lead_beta * (b_now - fb)) if params.lead_beta else 1.0
        hb_adj, ha_adj = hb * shift, ha * shift
        can_imp = params.improve_one_tick and (ma - mb) > 1.5 * tick

        quotes: list[tuple[int, float]] = []
        if q < q_max_clips:
            if drift_block_bid:
                blocked_drift += 1
            elif l1_block_bid:
                blocked_l1_thin += 1
            elif stat_block_bid:
                blocked_nonstationary += 1
            elif z_block_bid:
                blocked_zscore += 1
            else:
                e_bid = eff_e0 + params.skew_bps_per_clip * sk_q
                cap = hb_adj * math.exp(fb - e_bid * 1e-4)
                join = mb + tick if can_imp else mb
                bid_q = min(join, math.floor(cap / tick + 1e-9) * tick)
                if 0.0 < bid_q < ma:
                    quotes.append((1, bid_q))

        if q > -q_max_clips:
            if drift_block_ask:
                blocked_drift += 1
            elif l1_block_ask:
                blocked_l1_thin += 1
            elif stat_block_ask:
                blocked_nonstationary += 1
            elif z_block_ask:
                blocked_zscore += 1
            else:
                e_ask = eff_e0 - params.skew_bps_per_clip * sk_q
                floor_ = ha_adj * math.exp(fb + e_ask * 1e-4)
                join = ma - tick if can_imp else ma
                ask_q = max(join, math.ceil(floor_ / tick - 1e-9) * tick)
                if ask_q > mb:
                    quotes.append((-1, ask_q))

        if not quotes:
            continue

        bucket_indices = mk_by_step[k]
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
            if first_tf is None or through_notl <= 0.0:
                continue

            # Cancel shield accounting for cancel_latency_ms
            eff_rtt = max(rtt_ms, cancel_latency_ms)
            if params.shield_bps is not None and speedbump_ms > 2 * eff_rtt and len(ref_mid) == n:
                i0 = _ref_idx(first_tf - speedbump_ms + eff_rtt)
                i1 = _ref_idx(first_tf - eff_rtt)
                r0, r1 = float(ref_mid[i0]), float(ref_mid[i1])
                if ref_age[i0] <= max_age_steps and ref_age[i1] <= max_age_steps and r0 > 0.0 and r1 > 0.0:
                    r_bps = (r1 / r0 - 1.0) * 1e4
                    if (side == 1 and r_bps <= -params.shield_bps) or (side == -1 and r_bps >= params.shield_bps):
                        blocked_toxic += 1
                        continue

            fill_notl = min(clip_usd, through_notl)
            qty = fill_notl / qp
            j0 = min(max(int((first_tf + hedge_delay_ms - t0_ms) // step_ms) - 1, 0), n - 1)
            if hd_age[j0] > max_age_steps:
                jf = j0
                while jf < n - 1 and hd_age[jf] > max_age_steps and (jf - j0) < 600:
                    jf += 1
                j0 = jf
            if hd_age[j0] > max_age_steps:
                unhedged_stale_halts += 1
                continue

            hb0, ha0 = float(hd_bid[j0]), float(hd_ask[j0])
            if not (hb0 > 0.0 and ha0 > hb0 and math.isfinite(hb0) and math.isfinite(ha0)):
                unhedged_stale_halts += 1
                continue

            # Book maker fill
            cash -= side * qty * qp
            pos_mk += side * qty
            mk_f = fill_notl * maker_fee_bps * 1e-4
            fx_c = fill_notl * cross_stablecoin_bps * 1e-4 if cross_stablecoin_bps > 0.0 else 0.0
            fees_usd += mk_f
            mk_fees_usd += mk_f
            fx_drag_usd += fx_c
            mk_notional += fill_notl
            if side == 1:
                fills_bid += 1
            else:
                fills_ask += 1

            rem_qty = qty
            total_hd_fee_this_fill = 0.0
            total_d_slip_this_fill = 0.0
            weighted_h_px_sum = 0.0
            j_ioc = j0

            if params.passive_hedge.enabled:
                anchor_mid = 0.5 * (hb0 + ha0)
                max_steps_p = max(1, int(params.passive_hedge.max_passive_hedge_ms // step_ms))
                j_lim = min(j0 + max_steps_p, end_k)
                escalated = False
                for jp in range(j0, j_lim + 1):
                    if hd_age[jp] > params.passive_hedge.max_hedge_book_age_ms // step_ms:
                        passive_fail_closed += 1
                        # Fail-closed recovery: find next valid hedge touch to unwind safely
                        jf = jp
                        while jf < n - 1 and hd_age[jf] > max_age_steps and (jf - jp) < 600:
                            jf += 1
                        j_ioc = jf
                        escalated = True
                        break
                    hbp, hap = float(hd_bid[jp]), float(hd_ask[jp])
                    if not (hbp > 0.0 and hap > hbp and math.isfinite(hbp) and math.isfinite(hap)):
                        passive_fail_closed += 1
                        j_ioc = jp
                        escalated = True
                        break
                    mid_p = 0.5 * (hbp + hap)
                    adv_bps = (1.0 - mid_p / anchor_mid) * 1e4 if side == 1 else (mid_p / anchor_mid - 1.0) * 1e4
                    if adv_bps >= params.passive_hedge.hazard_ioc_adverse_bps:
                        passive_esc_hazard += 1
                        j_ioc = jp
                        escalated = True
                        break

                    # Passive resting price (strictly non-crossing)
                    can_imp_h = params.passive_hedge.improve_one_tick and (hap - hbp) > 1.5 * hd_tick
                    if side == 1:
                        p_rest = (math.floor((hap - hd_tick) / hd_tick + 1e-9) * hd_tick) if can_imp_h else hap
                        if p_rest <= hbp:
                            p_rest = hap
                    else:
                        p_rest = (math.ceil((hbp + hd_tick) / hd_tick - 1e-9) * hd_tick) if can_imp_h else hbp
                        if p_rest >= hap:
                            p_rest = hbp

                    # Strictly print-verified passive fill via hedge public_trades!
                    # NEVER fabricate passive fills without real strict trade-through prints.
                    if jp in hd_by_step and hp_px is not None and hp_sgn is not None and hp_sz is not None:
                        p_through_qty = 0.0
                        for idx_h in hd_by_step[jp]:
                            hsg = int(hp_sgn[idx_h])
                            hpx = float(hp_px[idx_h])
                            hsz = float(hp_sz[idx_h])
                            if (side == 1 and hsg == 1 and hpx > p_rest) or (side == -1 and hsg == -1 and hpx < p_rest):
                                p_through_qty += hsz
                        if p_through_qty > 0.0:
                            fill_p = min(rem_qty, p_through_qty)
                            notl_p = fill_p * p_rest
                            fee_p = notl_p * hedge_maker_fee_bps * 1e-4
                            cash += side * notl_p
                            pos_hd -= side * fill_p
                            fees_usd += fee_p
                            hd_fees_usd += fee_p
                            total_hd_fee_this_fill += fee_p
                            passive_hedge_qty += fill_p
                            weighted_h_px_sum += fill_p * p_rest
                            rem_qty -= fill_p
                            if rem_qty <= 1e-12:
                                rem_qty = 0.0
                                break
                    j_ioc = jp
                if rem_qty > 0.0 and not escalated:
                    passive_esc_timeout += 1
                    j_ioc = min(j_lim, end_k)

            # Execute remaining qty via CrossIoc at j_ioc
            if rem_qty > 0.0:
                if hd_age[j_ioc] > max_age_steps:
                    jf = j_ioc
                    while jf < n - 1 and hd_age[jf] > max_age_steps and (jf - j_ioc) < 600:
                        jf += 1
                    j_ioc = jf
                if hd_age[j_ioc] <= max_age_steps:
                    h_px = float(hd_bid[j_ioc] if side == 1 else hd_ask[j_ioc])
                    h_sz = float(hd_bid_sz[j_ioc] if side == 1 else hd_ask_sz[j_ioc])
                    if h_px > 0.0 and math.isfinite(h_px):
                        hd_notl = rem_qty * h_px
                        l1_h = h_px * h_sz if (math.isfinite(h_sz) and h_sz > 0.0) else 0.0
                        if l1_h <= 0.0:
                            d_slip_bps = 10.0
                        elif hd_notl > l1_h:
                            d_slip_bps = min(params.depth_impact_bps * (hd_notl / l1_h - 1.0), 25.0)
                        else:
                            d_slip_bps = 0.0
                        d_slip_u = hd_notl * d_slip_bps * 1e-4
                        hd_f = hd_notl * hedge_taker_fee_bps * 1e-4
                        cash += side * hd_notl
                        pos_hd -= side * rem_qty
                        fees_usd += hd_f
                        hd_fees_usd += hd_f
                        depth_slip_usd += d_slip_u
                        total_hd_fee_this_fill += hd_f
                        total_d_slip_this_fill += d_slip_u
                        ioc_hedge_qty += rem_qty
                        weighted_h_px_sum += rem_qty * h_px

                        if hp_ts is not None and hp_sgn is not None and hp_px is not None:
                            t_h = t0_ms + (j_ioc + 1) * step_ms
                            lo = bisect_left(hp_ts, t_h)
                            hi = bisect_left(hp_ts, t_h + 1_000)
                            if hi > lo:
                                vals = [float(hp_px[m]) for m in range(lo, hi) if int(hp_sgn[m]) == -side]
                                if vals:
                                    p_print = sum(vals) / len(vals)
                                    print_delta += side * rem_qty * (p_print - h_px)
                                    print_checked += 1
                    else:
                        unhedged_stale_halts += 1
                else:
                    unhedged_stale_halts += 1
            elif hp_ts is not None:
                # 100% filled via real public_trades on hedge venue -> verified by definition!
                print_checked += 1

            if collect_attribution and weighted_h_px_sum > 0.0:
                avg_h_px = weighted_h_px_sum / qty
                hd_mid_exec = 0.5 * (float(hd_bid[j_ioc]) + float(hd_ask[j_ioc]))
                raw_edge_u = side * (hd_mid_k - qp) * qty
                hd_lat_u = side * (hd_mid_exec - hd_mid_k) * qty
                hd_hspr_u = side * (hd_mid_exec - avg_h_px) * qty
                attr_gross_edge_usd += raw_edge_u
                attr_hd_lat_drift_usd += hd_lat_u
                attr_hd_half_spr_usd += hd_hspr_u
                pair_net = raw_edge_u + hd_lat_u - hd_hspr_u - total_d_slip_this_fill - mk_f - total_hd_fee_this_fill - fx_c

                k_f = min(max(int((first_tf - t0_ms) // step_ms) - 1, 0), n - 1)
                if k_f + 10 < n and mk_age[k_f + 10] <= max_age_steps:
                    m1 = 0.5 * (float(mk_bid[k_f + 10]) + float(mk_ask[k_f + 10]))
                    attr_mk_1s_usd += side * (m1 - qp) * qty
                    attr_mk_1s_cnt += 1
                if k_f + 50 < n and mk_age[k_f + 50] <= max_age_steps:
                    m5 = 0.5 * (float(mk_bid[k_f + 50]) + float(mk_ask[k_f + 50]))
                    attr_mk_5s_usd += side * (m5 - qp) * qty
                    attr_mk_5s_cnt += 1

                z_a = disloc_bps / max(sigma_k, 0.5)
                if side * z_a <= -1.5:
                    attr_fav_hz_pnl += pair_net
                    attr_fav_hz_n += 1
                elif side * z_a <= 0.0:
                    attr_fav_lz_pnl += pair_net
                    attr_fav_lz_n += 1
                else:
                    attr_unfav_pnl += pair_net
                    attr_unfav_n += 1

                dv = abs(drift_bps) / max(sigma_k, 0.5)
                if dv <= 1.0:
                    attr_stat_pnl += pair_net
                    attr_stat_n += 1
                else:
                    attr_nonstat_pnl += pair_net
                    attr_nonstat_n += 1

    _accumulate_carry(end_k)

    drain_cash = 0.0
    drain_fees = 0.0
    end_mb, end_ma = float(mk_bid[end_k]), float(mk_ask[end_k])
    end_hb, end_ha = float(hd_bid[end_k]), float(hd_ask[end_k])
    if pos_mk != 0.0 and end_mb > 0.0 and end_ma > 0.0:
        p_mk = end_mb if pos_mk > 0.0 else end_ma
        drain_cash += pos_mk * p_mk
        drain_fees += abs(pos_mk) * p_mk * maker_taker_fee_bps * 1e-4
    if pos_hd != 0.0 and end_hb > 0.0 and end_ha > 0.0:
        p_hd = end_hb if pos_hd > 0.0 else end_ha
        drain_cash += pos_hd * p_hd
        drain_fees += abs(pos_hd) * p_hd * hedge_taker_fee_bps * 1e-4

    mtm_pre = cash - fees_usd - fx_drag_usd - depth_slip_usd
    if pos_mk != 0.0 and end_mb > 0.0 and end_ma > 0.0:
        mtm_pre += pos_mk * 0.5 * (end_mb + end_ma)
    if pos_hd != 0.0 and end_hb > 0.0 and end_ha > 0.0:
        mtm_pre += pos_hd * 0.5 * (end_hb + end_ha)

    pnl = cash - fees_usd - fx_drag_usd - depth_slip_usd + drain_cash - drain_fees
    fills = fills_bid + fills_ask

    carry_usd = 0.0
    if maker_fr_bps_hr is not None:
        carry_usd += mk_usd_hours * (-maker_fr_bps_hr) * 1e-4
    if hedge_fr_bps_hr is not None:
        carry_usd += hd_usd_hours * (-hedge_fr_bps_hr) * 1e-4

    tot_h_qty = passive_hedge_qty + ioc_hedge_qty
    out: dict[str, Any] = {
        "fills": fills,
        "fills_bid": fills_bid,
        "fills_ask": fills_ask,
        "maker_notional_usd": round(mk_notional, 2),
        "quote_hedged_pnl_usd": round(pnl, 4),
        "print_verified_pnl_usd": round(pnl + print_delta, 4) if hp_ts is not None else None,
        "hedge_print_checked": print_checked,
        "hedge_print_coverage_pct": round(100.0 * print_checked / fills, 2) if fills else 0.0,
        "fees_usd": round(fees_usd + drain_fees, 4),
        "maker_fees_usd": round(mk_fees_usd, 4),
        "hedge_fees_usd": round(hd_fees_usd, 4),
        "fx_drag_usd": round(fx_drag_usd, 4),
        "depth_slip_usd": round(depth_slip_usd, 4),
        "passive_hedge_qty": round(passive_hedge_qty, 6),
        "ioc_hedge_qty": round(ioc_hedge_qty, 6),
        "passive_hedge_fill_pct": round(100.0 * passive_hedge_qty / tot_h_qty, 2) if tot_h_qty > 0.0 else 0.0,
        "passive_escalate_hazard": passive_esc_hazard,
        "passive_escalate_timeout": passive_esc_timeout,
        "passive_fail_closed_halts": passive_fail_closed,
        "blocked_toxic": blocked_toxic,
        "blocked_drift": blocked_drift,
        "blocked_l1_thin": blocked_l1_thin,
        "blocked_zscore": blocked_zscore,
        "blocked_nonstationary": blocked_nonstationary,
        "unhedged_stale_halts": unhedged_stale_halts,
        "terminal_drain_usd": round(mtm_pre - pnl, 4),
        "funding_carry_usd": round(carry_usd, 4),
        "pnl_plus_funding_usd": round(pnl + carry_usd, 4),
        "funding_available_legs": int(maker_fr_bps_hr is not None) + int(hedge_fr_bps_hr is not None),
    }
    if collect_attribution:
        out["attribution"] = {
            "gross_edge_usd": round(attr_gross_edge_usd, 4),
            "hedge_lat_drift_usd": round(attr_hd_lat_drift_usd, 4),
            "hedge_half_spread_usd": round(attr_hd_half_spr_usd, 4),
            "depth_slip_usd": round(depth_slip_usd, 4),
            "maker_fees_usd": round(mk_fees_usd, 4),
            "hedge_fees_usd": round(hd_fees_usd, 4),
            "drain_net_usd": round(drain_cash - drain_fees, 4),
            "maker_1s_markout_usd": round(attr_mk_1s_usd, 4),
            "maker_1s_markout_cnt": attr_mk_1s_cnt,
            "maker_5s_markout_usd": round(attr_mk_5s_usd, 4),
            "maker_5s_markout_cnt": attr_mk_5s_cnt,
            "fav_high_z_pnl_usd": round(attr_fav_hz_pnl, 4),
            "fav_high_z_fills": attr_fav_hz_n,
            "fav_low_z_pnl_usd": round(attr_fav_lz_pnl, 4),
            "fav_low_z_fills": attr_fav_lz_n,
            "unfav_side_pnl_usd": round(attr_unfav_pnl, 4),
            "unfav_side_fills": attr_unfav_n,
            "stationary_pnl_usd": round(attr_stat_pnl, 4),
            "stationary_fills": attr_stat_n,
            "nonstationary_pnl_usd": round(attr_nonstat_pnl, 4),
            "nonstationary_fills": attr_nonstat_n,
        }
    return out


def _sql_list(files: Sequence[str]) -> str:
    return "[" + ", ".join("'" + f.replace("'", "''") + "'" for f in files) + "]"


def _build_grid_fast(
    cols: dict[str, Any] | None, t0_ms: int, t1_ms: int, step_ms: int = STEP_MS
) -> dict[str, Any]:
    import numpy as np

    n = int((t1_ms - t0_ms) // step_ms)
    if cols is None or len(cols["b"]) == 0:
        nan_arr = np.full(n, np.nan)
        return {"bid": nan_arr, "ask": nan_arr, "bsz": nan_arr, "asz": nan_arr, "age": np.full(n, n, dtype=np.int64)}

    b0 = t0_ms // step_ms
    idx = np.asarray(cols["b"], dtype=np.int64) - b0
    keep = (idx >= 0) & (idx < n)
    pre = idx < 0
    out: dict[str, Any] = {}
    for k in ("bid", "ask", "bsz", "asz"):
        arr = np.full(n, np.nan, dtype=np.float64)
        src = np.asarray(cols[k], dtype=np.float64)
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


def _load_bbo_buckets_np(con: Any, files: Sequence[str], market: str, step_ms: int = STEP_MS) -> tuple[dict[str, Any] | None, int, int]:
    if not files:
        return None, 0, 0
    src = _sql_list(files)
    st = con.execute(
        f"""
        SELECT COUNT(*), MIN(available_at_ms), MAX(available_at_ms)
        FROM read_parquet({src})
        WHERE market = ? AND tradable AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
        """,
        [market],
    ).fetchone()
    if not st or not st[0] or int(st[0]) < 2 or st[1] is None or st[2] is None:
        return None, 0, 0
    cols = con.execute(
        f"""
        SELECT available_at_ms // {int(step_ms)} AS b,
               arg_max(bid_px_f64, available_at_ms) AS bid,
               arg_max(CAST(bid_sz AS DOUBLE), available_at_ms) AS bsz,
               arg_max(ask_px_f64, available_at_ms) AS ask,
               arg_max(CAST(ask_sz AS DOUBLE), available_at_ms) AS asz
        FROM read_parquet({src})
        WHERE market = ? AND tradable AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
        GROUP BY 1 ORDER BY 1
        """,
        [market],
    ).fetchnumpy()
    return cols, int(st[1]), int(st[2])


def _load_trades_np(con: Any, files: Sequence[str], market: str, t0_ms: int, t1_ms: int) -> dict[str, Any] | None:
    import numpy as np

    if not files:
        return None
    src = _sql_list(files)
    cols = con.execute(
        f"""
        SELECT available_at_ms AS ts,
               CASE WHEN lower(aggressor_side) IN ('buy','b','bid') THEN 1
                    WHEN lower(aggressor_side) IN ('sell','s','ask') THEN -1 ELSE 0 END AS sgn,
               price_f64 AS px, size_f64 AS sz
        FROM read_parquet({src})
        WHERE market = ? AND NOT is_zero_price_transfer AND price_f64 > 0 AND size_f64 > 0
          AND available_at_ms >= ? AND available_at_ms < ?
        ORDER BY available_at_ms ASC
        """,
        [market, int(t0_ms), int(t1_ms)],
    ).fetchnumpy()
    return {k: np.asarray(v) for k, v in cols.items()}


def _load_mean_funding_bps_hr(
    con: Any, lakehouse_dir: Path, venue: str, symbol: str, raw_market: str, utc_date: str, campaign_id: str
) -> float | None:
    files = campaign_parquet_files(lakehouse_dir, "funding_and_ctx", venue, utc_date, campaign_id)
    if not files:
        return None
    candidates = list(dict.fromkeys([symbol, raw_market]))
    src = _sql_list(files)
    placeholders = ", ".join("?" for _ in candidates)
    row = con.execute(
        f"""
        SELECT AVG(funding_rate_bps_f64), COUNT(*)
        FROM read_parquet({src})
        WHERE market IN ({placeholders}) AND funding_rate_bps_f64 IS NOT NULL
        """,
        candidates,
    ).fetchone()
    if row is None or row[0] is None or int(row[1] or 0) < 5:
        return None
    return float(row[0])


def _process_campaign_worker(task: dict[str, Any]) -> dict[str, Any]:
    """Process one campaign across either selection sweep or frozen evaluation."""
    import duckdb

    lakehouse_dir = Path(task["lakehouse_dir"])
    camp = task["camp"]
    dom = camp["domain"]
    w_name = camp["window"]
    w_num = camp["window_num"]
    cid = camp["campaign_id"]
    utc_date = camp["utc_date"]
    split = camp["split"]
    resolved_meta = task["resolved_meta"]
    pair_fees = task["pair_fees"]
    mode = task["worker_mode"]  # "sweep_selection_dev" or "eval_frozen"
    frozen_by_pair_arm = task.get("frozen_by_pair_arm") or {}
    latencies: tuple[int, ...] = tuple(task.get("latencies_ms", LATENCIES_MS))

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=1; SET memory_limit='1GB';")

    bbo_cache: dict[tuple[str, str], tuple[dict[str, Any] | None, int, int]] = {}
    tr_cache: dict[tuple[str, str], dict[str, Any] | None] = {}
    fund_cache: dict[tuple[str, str], float | None] = {}

    def get_bbo(venue: str, sym: str) -> tuple[dict[str, Any] | None, int, int]:
        k = (venue, sym)
        if k not in bbo_cache:
            mkt = VENUE_SYMBOL_MAP.get(venue, {}).get(sym)
            if not mkt:
                bbo_cache[k] = (None, 0, 0)
            else:
                fs = campaign_parquet_files(lakehouse_dir, "bbo_quotes", venue, utc_date, cid)
                bbo_cache[k] = _load_bbo_buckets_np(con, fs, mkt, STEP_MS)
        return bbo_cache[k]

    def get_tr(venue: str, sym: str, t0: int, t1: int) -> dict[str, Any] | None:
        k = (venue, sym)
        if k not in tr_cache:
            mkt = VENUE_SYMBOL_MAP.get(venue, {}).get(sym)
            if not mkt:
                tr_cache[k] = None
            else:
                fs = campaign_parquet_files(lakehouse_dir, "public_trades", venue, utc_date, cid)
                tr_cache[k] = _load_trades_np(con, fs, mkt, t0, t1)
        return tr_cache[k]

    def get_fund(venue: str, sym: str) -> float | None:
        k = (venue, sym)
        if k not in fund_cache:
            mkt = VENUE_SYMBOL_MAP.get(venue, {}).get(sym)
            if not mkt:
                fund_cache[k] = None
            else:
                fund_cache[k] = _load_mean_funding_bps_hr(con, lakehouse_dir, venue, sym, mkt, utc_date, cid)
        return fund_cache[k]

    dom_pairs = [p for p in build_pair_catalog() if p.domain == dom]
    out_rows: list[dict[str, Any]] = []
    stage_a_rows: list[dict[str, Any]] = []

    for pair in dom_pairs:
        mk_cols, mk_t0, mk_t1 = get_bbo(pair.maker_venue, pair.symbol)
        hd_cols, hd_t0, hd_t1 = get_bbo(pair.hedge_venue, pair.symbol)
        if mk_cols is None or hd_cols is None:
            continue
        t0 = max(mk_t0, hd_t0)
        t1 = min(mk_t1, hd_t1)
        rf_cols, rf_t0, rf_t1 = get_bbo(pair.ref_venue, pair.symbol)
        if rf_cols is not None:
            t0 = max(t0, rf_t0)
            t1 = min(t1, rf_t1)
        if t1 - t0 < WARMUP_MS + 60_000:
            continue

        mk_g = _build_grid_fast(mk_cols, t0, t1, STEP_MS)
        hd_g = _build_grid_fast(hd_cols, t0, t1, STEP_MS)
        rf_g = _build_grid_fast(rf_cols, t0, t1, STEP_MS) if rf_cols is not None else None
        rf_mid = 0.5 * (rf_g["bid"] + rf_g["ask"]) if rf_g is not None else []
        rf_age = rf_g["age"] if rf_g is not None else []

        causal_mid = compute_causal_basis_series(
            mk_g["bid"], mk_g["ask"], mk_g["bsz"], mk_g["asz"], mk_g["age"],
            hd_g["bid"], hd_g["ask"], hd_g["bsz"], hd_g["asz"], hd_g["age"],
            use_microprice_anchor=False,
        )
        causal_micro = compute_causal_basis_series(
            mk_g["bid"], mk_g["ask"], mk_g["bsz"], mk_g["asz"], mk_g["age"],
            hd_g["bid"], hd_g["ask"], hd_g["bsz"], hd_g["asz"], hd_g["age"],
            use_microprice_anchor=True,
        )
        mk_tr = get_tr(pair.maker_venue, pair.symbol, t0, t1)
        hd_tr = get_tr(pair.hedge_venue, pair.symbol, t0, t1)
        mk_fr = get_fund(pair.maker_venue, pair.symbol)
        hd_fr = get_fund(pair.hedge_venue, pair.symbol)
        if mk_tr is None or len(mk_tr["ts"]) == 0:
            continue

        n = len(mk_g["bid"])
        tick = _infer_tick(mk_cols["bid"])
        hd_tick = _infer_tick(hd_cols["bid"])
        pf = pair_fees[pair.label]
        mm = resolved_meta[pair.maker_venue]
        hm = resolved_meta[pair.hedge_venue]
        has_sb = mm["speedbump_ms"] > 0
        pre_hp_ts = [int(x) for x in hd_tr["ts"]] if (hd_tr is not None and len(hd_tr["ts"]) > 0) else None
        pre_hd_steps = bucket_trades_by_step(hd_tr, t0, 0, n, start_step=WARMUP_MS // STEP_MS, end_step=n) if hd_tr is not None else {}

        if mode == "sweep_selection_dev":
            # Stage A attribution across latencies on baseline T2_taker_hedge (e0=4, vol=0.5)
            base_param = StrategyParams(
                name="stage_a_upgraded_track2",
                arm="StageA",
                e0_bps=4.0,
                vol_edge_mult=0.5,
                shield_bps=2.5 if has_sb else None,
            )
            for lat_ms in latencies:
                pre_mk_steps = bucket_trades_by_step(
                    mk_tr, t0, lat_ms, n,
                    start_step=WARMUP_MS // STEP_MS,
                    end_step=min(WARMUP_MS // STEP_MS + TEN_HOURS_MS // STEP_MS, n),
                )
                res_a = simulate_pair_arb(
                    mk_bid=mk_g["bid"], mk_ask=mk_g["ask"], mk_age=mk_g["age"],
                    hd_bid=hd_g["bid"], hd_ask=hd_g["ask"], hd_bid_sz=hd_g["bsz"], hd_ask_sz=hd_g["asz"], hd_age=hd_g["age"],
                    ref_mid=rf_mid, ref_age=rf_age, causal=causal_mid,
                    mk_trades=mk_tr, hd_trades=hd_tr, t0_ms=t0, tick=tick, hd_tick=hd_tick,
                    params=base_param, maker_delay_ms=lat_ms, cancel_latency_ms=lat_ms,
                    hedge_delay_ms=hm["order_delay_ms"], speedbump_ms=mm["speedbump_ms"], rtt_ms=40,
                    maker_fee_bps=pf["maker_fee_bps"], maker_taker_fee_bps=pf["maker_taker_fee_bps"],
                    hedge_maker_fee_bps=pf["hedge_maker_fee_bps"], hedge_taker_fee_bps=pf["hedge_taker_fee_bps"],
                    maker_fr_bps_hr=mk_fr, hedge_fr_bps_hr=hd_fr,
                    pre_mk_by_step=pre_mk_steps, pre_hd_by_step=pre_hd_steps, pre_hp_ts=pre_hp_ts,
                    collect_attribution=True,
                )
                stage_a_rows.append({
                    "pair": pair.label, "cohort": pair.cohort, "domain": dom,
                    "maker_venue": pair.maker_venue, "hedge_venue": pair.hedge_venue,
                    "same_risk_domain": pair.same_risk_domain, "window": w_name,
                    "lat_ms": lat_ms, **res_a,
                })

            # Round-2 selection grid at anchor latency 100ms
            lat_sel = 100
            pre_mk_100 = bucket_trades_by_step(
                mk_tr, t0, lat_sel, n,
                start_step=WARMUP_MS // STEP_MS,
                end_step=min(WARMUP_MS // STEP_MS + TEN_HOURS_MS // STEP_MS, n),
            )
            arms = build_round2_arms(has_sb)
            for arm_name, cands in arms.items():
                for cand in cands:
                    c_ser = causal_micro if cand.use_microprice_anchor else causal_mid
                    res = simulate_pair_arb(
                        mk_bid=mk_g["bid"], mk_ask=mk_g["ask"], mk_age=mk_g["age"],
                        hd_bid=hd_g["bid"], hd_ask=hd_g["ask"], hd_bid_sz=hd_g["bsz"], hd_ask_sz=hd_g["asz"], hd_age=hd_g["age"],
                        ref_mid=rf_mid, ref_age=rf_age, causal=c_ser,
                        mk_trades=mk_tr, hd_trades=hd_tr, t0_ms=t0, tick=tick, hd_tick=hd_tick,
                        params=cand, maker_delay_ms=lat_sel, cancel_latency_ms=lat_sel,
                        hedge_delay_ms=hm["order_delay_ms"], speedbump_ms=mm["speedbump_ms"], rtt_ms=40,
                        maker_fee_bps=pf["maker_fee_bps"], maker_taker_fee_bps=pf["maker_taker_fee_bps"],
                        hedge_maker_fee_bps=pf["hedge_maker_fee_bps"], hedge_taker_fee_bps=pf["hedge_taker_fee_bps"],
                        maker_fr_bps_hr=mk_fr, hedge_fr_bps_hr=hd_fr,
                        pre_mk_by_step=pre_mk_100, pre_hd_by_step=pre_hd_steps, pre_hp_ts=pre_hp_ts,
                    )
                    out_rows.append({
                        "pair": pair.label, "arm": arm_name, "cand": cand.name,
                        "window": w_name, "fills": res["fills"], "pnl": res["quote_hedged_pnl_usd"],
                        "print_pnl": res["print_verified_pnl_usd"],
                    })
        else:
            # Frozen validation across latencies {0, 100, 200, 300} ms and FX {0, 1, 2} bp
            for lat_ms in latencies:
                pre_mk_steps = bucket_trades_by_step(
                    mk_tr, t0, lat_ms, n,
                    start_step=WARMUP_MS // STEP_MS,
                    end_step=min(WARMUP_MS // STEP_MS + TEN_HOURS_MS // STEP_MS, n),
                )
                for arm_name, cand_dict in frozen_by_pair_arm.get(pair.label, {}).items():
                    ph_d = cand_dict.get("passive_hedge") or {}
                    cand = StrategyParams(
                        **{k: v for k, v in cand_dict.items() if k != "passive_hedge"},
                        passive_hedge=PassiveHedgeParams(**ph_d),
                    )
                    c_ser = causal_micro if cand.use_microprice_anchor else causal_mid
                    fx_list = (0.0, 1.0, 2.0) if pair.cross_stablecoin else (0.0,)
                    fx_pnls: dict[str, Any] = {}
                    base_res: dict[str, Any] | None = None
                    for fx in fx_list:
                        res = simulate_pair_arb(
                            mk_bid=mk_g["bid"], mk_ask=mk_g["ask"], mk_age=mk_g["age"],
                            hd_bid=hd_g["bid"], hd_ask=hd_g["ask"], hd_bid_sz=hd_g["bsz"], hd_ask_sz=hd_g["asz"], hd_age=hd_g["age"],
                            ref_mid=rf_mid, ref_age=rf_age, causal=c_ser,
                            mk_trades=mk_tr, hd_trades=hd_tr, t0_ms=t0, tick=tick, hd_tick=hd_tick,
                            params=cand, maker_delay_ms=lat_ms, cancel_latency_ms=lat_ms,
                            hedge_delay_ms=hm["order_delay_ms"], speedbump_ms=mm["speedbump_ms"], rtt_ms=40,
                            maker_fee_bps=pf["maker_fee_bps"], maker_taker_fee_bps=pf["maker_taker_fee_bps"],
                            hedge_maker_fee_bps=pf["hedge_maker_fee_bps"], hedge_taker_fee_bps=pf["hedge_taker_fee_bps"],
                            cross_stablecoin_bps=fx, maker_fr_bps_hr=mk_fr, hedge_fr_bps_hr=hd_fr,
                            pre_mk_by_step=pre_mk_steps, pre_hd_by_step=pre_hd_steps, pre_hp_ts=pre_hp_ts,
                        )
                        if fx == 0.0:
                            base_res = res
                        fx_pnls[f"{int(fx)}bp"] = {
                            "pnl_usd": res["quote_hedged_pnl_usd"],
                            "print_pnl_usd": res["print_verified_pnl_usd"],
                        }
                    assert base_res is not None
                    out_rows.append({
                        "pair": pair.label, "cohort": pair.cohort, "domain": dom,
                        "maker_venue": pair.maker_venue, "hedge_venue": pair.hedge_venue,
                        "same_risk_domain": pair.same_risk_domain, "cross_stablecoin": pair.cross_stablecoin,
                        "arm": arm_name, "window": w_name, "window_num": w_num, "split": split,
                        "lat_ms": lat_ms, "fx_pnls": fx_pnls if pair.cross_stablecoin else None,
                        **base_res,
                    })

    con.close()
    bbo_cache.clear()
    tr_cache.clear()
    fund_cache.clear()
    gc.collect()
    return {"campaign_id": cid, "domain": dom, "split": split, "rows": out_rows, "stage_a": stage_a_rows}


def summarize_stage_a(stage_a_rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Aggregate Stage A loss attribution by latency, domain, and hedge leg."""
    by_latency: dict[str, Any] = {}
    for lat in LATENCIES_MS:
        rows = [r for r in stage_a_rows if r["lat_ms"] == lat]
        fills = sum(r["fills"] for r in rows)
        notl = sum(r["maker_notional_usd"] for r in rows)
        pnl = sum(r["quote_hedged_pnl_usd"] for r in rows)
        pr_vals = [r["print_verified_pnl_usd"] for r in rows if r["print_verified_pnl_usd"] is not None]
        ge = sum(r["attribution"]["gross_edge_usd"] for r in rows)
        hl = sum(r["attribution"]["hedge_lat_drift_usd"] for r in rows)
        hs = sum(r["attribution"]["hedge_half_spread_usd"] for r in rows)
        ds = sum(r["attribution"]["depth_slip_usd"] for r in rows)
        mf = sum(r["attribution"]["maker_fees_usd"] for r in rows)
        hf = sum(r["attribution"]["hedge_fees_usd"] for r in rows)
        dn = sum(r["attribution"]["drain_net_usd"] for r in rows)
        m1 = sum(r["attribution"]["maker_1s_markout_usd"] for r in rows)
        m5 = sum(r["attribution"]["maker_5s_markout_usd"] for r in rows)
        bps_scale = 1e4 / notl if notl > 0 else 0.0
        by_latency[f"{lat}ms"] = {
            "fills": fills,
            "maker_notional_usd": round(notl, 2),
            "quote_hedged_pnl_usd": round(pnl, 2),
            "quote_hedged_pnl_bps": round(pnl * bps_scale, 3),
            "print_verified_pnl_usd": round(sum(pr_vals), 2) if pr_vals else None,
            "gross_edge_usd": round(ge, 2),
            "gross_edge_bps": round(ge * bps_scale, 3),
            "hedge_lat_drift_usd": round(hl, 2),
            "hedge_lat_drift_bps": round(hl * bps_scale, 3),
            "hedge_half_spread_usd": round(hs, 2),
            "hedge_half_spread_bps": round(hs * bps_scale, 3),
            "depth_slip_usd": round(ds, 2),
            "depth_slip_bps": round(ds * bps_scale, 3),
            "maker_fees_usd": round(mf, 2),
            "hedge_fees_usd": round(hf, 2),
            "hedge_fees_bps": round(hf * bps_scale, 3),
            "drain_net_usd": round(dn, 2),
            "maker_1s_markout_usd": round(m1, 2),
            "maker_1s_markout_bps": round(m1 * bps_scale, 3),
            "maker_5s_markout_usd": round(m5, 2),
            "maker_5s_markout_bps": round(m5 * bps_scale, 3),
        }

    r100 = [r for r in stage_a_rows if r["lat_ms"] == 100]
    z_buckets = {
        "fav_high_z": {
            "fills": sum(r["attribution"]["fav_high_z_fills"] for r in r100),
            "pnl_usd": round(sum(r["attribution"]["fav_high_z_pnl_usd"] for r in r100), 2),
        },
        "fav_low_z": {
            "fills": sum(r["attribution"]["fav_low_z_fills"] for r in r100),
            "pnl_usd": round(sum(r["attribution"]["fav_low_z_pnl_usd"] for r in r100), 2),
        },
        "unfav_side": {
            "fills": sum(r["attribution"]["unfav_side_fills"] for r in r100),
            "pnl_usd": round(sum(r["attribution"]["unfav_side_pnl_usd"] for r in r100), 2),
        },
    }
    stationarity_buckets = {
        "stationary": {
            "fills": sum(r["attribution"]["stationary_fills"] for r in r100),
            "pnl_usd": round(sum(r["attribution"]["stationary_pnl_usd"] for r in r100), 2),
        },
        "nonstationary": {
            "fills": sum(r["attribution"]["nonstationary_fills"] for r in r100),
            "pnl_usd": round(sum(r["attribution"]["nonstationary_pnl_usd"] for r in r100), 2),
        },
    }
    return {
        "by_latency": by_latency,
        "z_buckets_at_100ms": z_buckets,
        "stationarity_buckets_at_100ms": stationarity_buckets,
    }


def _agg_eval_rows(rows: Sequence[dict[str, Any]]) -> dict[str, Any]:
    if not rows:
        return {"pair_windows": 0, "fills": 0, "quote_hedged_pnl_usd": 0.0}
    fills = sum(r["fills"] for r in rows)
    notl = sum(r["maker_notional_usd"] for r in rows)
    pnl = sum(r["quote_hedged_pnl_usd"] for r in rows)
    pr_vals = [r["print_verified_pnl_usd"] for r in rows if r["print_verified_pnl_usd"] is not None]
    pas_q = sum(r["passive_hedge_qty"] for r in rows)
    ioc_q = sum(r["ioc_hedge_qty"] for r in rows)
    tot_hq = pas_q + ioc_q
    covs = [r["hedge_print_coverage_pct"] for r in rows if r["fills"] > 0]
    return {
        "pair_windows": len(rows),
        "fills": fills,
        "maker_notional_usd": round(notl, 2),
        "quote_hedged_pnl_usd": round(pnl, 2),
        "quote_hedged_pnl_bps": round(pnl * 1e4 / notl, 3) if notl > 0 else 0.0,
        "print_verified_pnl_usd": round(sum(pr_vals), 2) if pr_vals else None,
        "hedge_print_coverage_pct": round(sum(covs) / len(covs), 2) if covs else 0.0,
        "fees_usd": round(sum(r["fees_usd"] for r in rows), 2),
        "depth_slip_usd": round(sum(r["depth_slip_usd"] for r in rows), 2),
        "funding_carry_usd": round(sum(r["funding_carry_usd"] for r in rows), 2),
        "pnl_plus_funding_usd": round(sum(r["pnl_plus_funding_usd"] for r in rows), 2),
        "passive_hedge_fill_pct": round(100.0 * pas_q / tot_hq, 2) if tot_hq > 0 else 0.0,
        "passive_escalate_hazard": sum(r["passive_escalate_hazard"] for r in rows),
        "passive_escalate_timeout": sum(r["passive_escalate_timeout"] for r in rows),
        "passive_fail_closed_halts": sum(r["passive_fail_closed_halts"] for r in rows),
        "blocked_zscore": sum(r["blocked_zscore"] for r in rows),
        "blocked_nonstationary": sum(r["blocked_nonstationary"] for r in rows),
        "blocked_l1_thin": sum(r["blocked_l1_thin"] for r in rows),
        "unhedged_stale_halts": sum(r["unhedged_stale_halts"] for r in rows),
    }


def evaluate_lakehouse(
    lakehouse_dir: Path,
    *,
    fee_evidence_path: Path | None = None,
    lighter_fee_mode: str = "standard_zero",
    max_workers: int = 10,
) -> dict[str, Any]:
    """Run Stage A + Round-2 selection on `selection_dev`, freeze, and validate on `holdout` & `w26_oos`."""
    resolved_meta, fee_ev, fee_prov = resolve_venue_meta(fee_evidence_path, lighter_fee_mode=lighter_fee_mode)
    all_pairs = build_pair_catalog()
    pair_fees = {p.label: resolve_pair_fees(p, resolved_meta, fee_ev, lighter_fee_mode=lighter_fee_mode) for p in all_pairs}

    all_camps = (
        discover_campaigns(lakehouse_dir, "crypto", min_window=1, max_window=26)
        + discover_campaigns(lakehouse_dir, "rwa", min_window=1, max_window=26)
    )
    dev_camps = [c for c in all_camps if c["split"] == "selection_dev"]

    # Step 1: Stage A + Round-2 Sweep on selection_dev
    dev_tasks = [
        {
            "lakehouse_dir": str(lakehouse_dir),
            "camp": c,
            "resolved_meta": resolved_meta,
            "pair_fees": pair_fees,
            "worker_mode": "sweep_selection_dev",
            "latencies_ms": LATENCIES_MS,
        }
        for c in dev_camps
    ]
    sweep_rows: list[dict[str, Any]] = []
    stage_a_rows: list[dict[str, Any]] = []
    with concurrent.futures.ProcessPoolExecutor(max_workers=max_workers) as ex:
        for res in ex.map(_process_campaign_worker, dev_tasks):
            sweep_rows.extend(res["rows"])
            stage_a_rows.extend(res["stage_a"])
            print(json.dumps({"stage": "dev_sweep", "campaign_id": res["campaign_id"]}), flush=True)

    stage_a_summary = summarize_stage_a(stage_a_rows)

    # Step 2: Freeze best config per (pair, arm) on selection_dev (with domain-appropriate min fills)
    cand_scores: dict[tuple[str, str, str], tuple[int, float]] = {}
    for r in sweep_rows:
        k = (r["pair"], r["arm"], r["cand"])
        prev_f, prev_p = cand_scores.get(k, (0, 0.0))
        cand_scores[k] = (prev_f + int(r["fills"]), prev_p + float(r["pnl"]))

    frozen_by_pair_arm: dict[str, dict[str, dict[str, Any]]] = {}
    for pair in all_pairs:
        has_sb = resolved_meta[pair.maker_venue]["speedbump_ms"] > 0
        min_fills = MIN_SELECTION_FILLS_CRYPTO if pair.domain == "crypto" else MIN_SELECTION_FILLS_RWA
        arms = build_round2_arms(has_sb)
        frozen_by_pair_arm[pair.label] = {}
        for arm_name, cands in arms.items():
            best_c: StrategyParams | None = None
            best_score = -1e18
            for c in cands:
                f_cnt, pnl_sum = cand_scores.get((pair.label, arm_name, c.name), (0, 0.0))
                score = pnl_sum if f_cnt >= min_fills else (-1e12 + pnl_sum)
                if score > best_score or best_c is None:
                    best_score = score
                    best_c = c
            assert best_c is not None
            frozen_by_pair_arm[pair.label][arm_name] = asdict(best_c)

    # Step 3: Frozen ONE-SHOT Evaluation across selection_dev, holdout, w26_oos & latencies {0,100,200,300} ms
    eval_tasks = [
        {
            "lakehouse_dir": str(lakehouse_dir),
            "camp": c,
            "resolved_meta": resolved_meta,
            "pair_fees": pair_fees,
            "worker_mode": "eval_frozen",
            "frozen_by_pair_arm": frozen_by_pair_arm,
            "latencies_ms": LATENCIES_MS,
        }
        for c in all_camps
    ]
    eval_rows: list[dict[str, Any]] = []
    with concurrent.futures.ProcessPoolExecutor(max_workers=max_workers) as ex:
        for res in ex.map(_process_campaign_worker, eval_tasks):
            eval_rows.extend(res["rows"])
            print(json.dumps({"stage": "frozen_eval", "campaign_id": res["campaign_id"], "split": res["split"]}), flush=True)

    # Aggregate cohort summaries by arm x split x latency
    arm_names = ("T2_taker_hedge", "T2_zscore_selective", "T2_passive_then_ioc", "T2_zscore_plus_passive_ioc")
    splits = ("selection_dev", "holdout", "w26_oos")
    cohorts = {
        "USDC_USDC_ws_distinct_domain": lambda r: r["cohort"] == "USDC/USDC" and not r["same_risk_domain"],
        "USDG_USDC_ws_distinct_domain": lambda r: r["cohort"] == "USDG/USDC" and not r["same_risk_domain"],
        "USDG_USDC_same_risk_domain_rh_lighter": lambda r: r["cohort"] == "USDG/USDC" and r["same_risk_domain"],
        "zero_fee_hedge_to_lighter_std": lambda r: r["hedge_venue"] in ("lighter", "lighter_robinhood") and not r["same_risk_domain"],
    }

    cohort_summaries: dict[str, Any] = {}
    for c_name, pred in cohorts.items():
        c_rows = [r for r in eval_rows if pred(r)]
        c_out: dict[str, Any] = {}
        for arm in arm_names:
            a_rows = [r for r in c_rows if r["arm"] == arm]
            a_out: dict[str, Any] = {}
            for sp in splits:
                sp_rows = [r for r in a_rows if r["split"] == sp]
                sp_out: dict[str, Any] = {}
                for lat in LATENCIES_MS:
                    l_rows = [r for r in sp_rows if r["lat_ms"] == lat]
                    agg = _agg_eval_rows(l_rows)
                    if c_name.startswith("USDG_USDC"):
                        fx_s: dict[str, Any] = {}
                        for fx_k in ("0bp", "1bp", "2bp"):
                            fx_s[fx_k] = round(
                                sum(r["fx_pnls"][fx_k]["pnl_usd"] for r in l_rows if r.get("fx_pnls")), 2
                            )
                        agg["fx_sensitivity_usd"] = fx_s
                    sp_out[f"{lat}ms"] = agg
                a_out[sp] = sp_out
            c_out[arm] = a_out
        cohort_summaries[c_name] = c_out

    # Per-pair summary at 100ms across splits for top/bottom comparison
    pair_summaries: list[dict[str, Any]] = []
    for pair in all_pairs:
        p_rows = [r for r in eval_rows if r["pair"] == pair.label and r["lat_ms"] == 100]
        if not p_rows:
            continue
        arms_p: dict[str, Any] = {}
        for arm in arm_names:
            arms_p[arm] = {
                "frozen_params": frozen_by_pair_arm[pair.label][arm],
                **{sp: _agg_eval_rows([r for r in p_rows if r["arm"] == arm and r["split"] == sp]) for sp in splits},
            }
        pair_summaries.append({
            "pair": pair.label,
            "cohort": pair.cohort,
            "domain": pair.domain,
            "same_risk_domain": pair.same_risk_domain,
            "cross_stablecoin": pair.cross_stablecoin,
            "fees": pair_fees[pair.label],
            "arms_at_100ms": arms_p,
        })

    return {
        "schema": "track02_passive_hedge_and_zscore_arb_v1",
        "venue_capability_snapshot_version": VENUE_CAPABILITY_SNAPSHOT_VERSION,
        "fee_provenance": fee_prov,
        "venue_meta": resolved_meta,
        "splits": {"selection_dev": "w01..w20 (ex w08 crypto)", "holdout": "w21..w25", "w26_oos": "w26"},
        "latencies_ms": list(LATENCIES_MS),
        "stage_a_attribution": stage_a_summary,
        "cohort_summaries": cohort_summaries,
        "pairs_at_100ms": pair_summaries,
    }


def run_self_test() -> None:
    """Pure-stdlib verification of Z-score, Stationarity, Microprice, and Print-Verified Passive Hedge FSM."""
    n = 160
    mk_bid = [99.99] * n
    mk_ask = [100.01] * n
    mk_bsz = [10.0] * n
    mk_asz = [10.0] * n
    mk_age = [0] * n
    hd_bid = [99.99] * n
    hd_ask = [100.01] * n
    hd_bsz = [20.0] * n
    hd_asz = [20.0] * n
    hd_age = [0] * n
    ref_mid = [100.0] * n
    ref_age = [0] * n

    # Inject controlled basis vol in warmup
    for k in range(10, 40):
        if k % 2 == 0:
            mk_bid[k], mk_ask[k] = 100.04, 100.06
        else:
            mk_bid[k], mk_ask[k] = 99.94, 99.96

    # At k=70: maker cheap (-10bp -> z << -2.0). At k=90: maker neutral (z ~ 0 -> blocked by z-score!)
    mk_bid[70], mk_ask[70] = 99.89, 99.91
    # Maker sell print at k=70 fills maker bid (MakerBought -> sell hedge passively at 100.01)
    # Maker sell print at k=90 would fill neutral bid if not blocked by z-score gate
    mk_trades = {
        "ts": [70 * STEP_MS + 100, 90 * STEP_MS + 100],
        "sgn": [-1, -1],
        "px": [99.85, 99.95],
        "sz": [15.0, 15.0],
    }
    # Real hedge aggressor BUY trade-through print (> 100.01) at step k=72 fills passive sell hedge!
    hd_trades = {
        "ts": [72 * STEP_MS + 120],
        "sgn": [1],
        "px": [100.02],
        "sz": [15.0],
    }

    causal = compute_causal_basis_series(
        mk_bid, mk_ask, mk_bsz, mk_asz, mk_age,
        hd_bid, hd_ask, hd_bsz, hd_asz, hd_age,
        vol_tau_ms=2_000,
    )
    p_combo = StrategyParams(
        name="test_combo",
        arm="T2_zscore_plus_passive_ioc",
        e0_bps=1.0,
        vol_edge_mult=0.0,
        basis_drift_guard_bps=0.0,
        min_basis_zscore=2.0,
        basis_vol_floor_bps=2.0,
        max_drift_to_vol_ratio=2.0,
        quote_favorable_side_only=True,
        passive_hedge=PassiveHedgeParams(
            enabled=True,
            max_passive_hedge_ms=500,
            hazard_ioc_adverse_bps=2.0,
        ),
    )
    res_p = simulate_pair_arb(
        mk_bid=mk_bid, mk_ask=mk_ask, mk_age=mk_age,
        hd_bid=hd_bid, hd_ask=hd_ask, hd_bid_sz=hd_bsz, hd_ask_sz=hd_asz, hd_age=hd_age,
        ref_mid=ref_mid, ref_age=ref_age, causal=causal, mk_trades=mk_trades, hd_trades=hd_trades,
        t0_ms=0, tick=0.01, hd_tick=0.01, params=p_combo,
        maker_delay_ms=0, cancel_latency_ms=0, hedge_delay_ms=100, speedbump_ms=0, rtt_ms=40,
        maker_fee_bps=0.0, maker_taker_fee_bps=0.0, hedge_maker_fee_bps=0.0, hedge_taker_fee_bps=2.25,
        warmup_steps=50, decision_steps=80, collect_attribution=True,
    )
    assert res_p["fills"] == 1, f"expected 1 z-filtered fill, got {res_p}"
    assert res_p["blocked_zscore"] >= 1, f"expected z-score block at k=90, got {res_p}"
    assert res_p["passive_hedge_fill_pct"] == 100.0, f"expected 100% print-verified passive hedge fill: {res_p}"
    assert res_p["hedge_fees_usd"] == 0.0, f"expected 0 hedge maker fee: {res_p}"

    # Without hedge prints, passive hedge MUST NOT fabricate fills: must escalate to CrossIoc!
    res_no_hp = simulate_pair_arb(
        mk_bid=mk_bid, mk_ask=mk_ask, mk_age=mk_age,
        hd_bid=hd_bid, hd_ask=hd_ask, hd_bid_sz=hd_bsz, hd_ask_sz=hd_asz, hd_age=hd_age,
        ref_mid=ref_mid, ref_age=ref_age, causal=causal, mk_trades=mk_trades, hd_trades=None,
        t0_ms=0, tick=0.01, hd_tick=0.01, params=p_combo,
        maker_delay_ms=0, cancel_latency_ms=0, hedge_delay_ms=100, speedbump_ms=0, rtt_ms=40,
        maker_fee_bps=0.0, maker_taker_fee_bps=0.0, hedge_maker_fee_bps=0.0, hedge_taker_fee_bps=2.25,
        warmup_steps=50, decision_steps=80,
    )
    assert res_no_hp["passive_hedge_fill_pct"] == 0.0
    assert res_no_hp["passive_escalate_timeout"] == 1
    assert res_no_hp["hedge_fees_usd"] > 0.0

    # Microprice tilts causal series when L1 sizes are asymmetric
    assert abs(l1_microprice(99.90, 100.10, 90.0, 10.0) - 100.08) < 1e-9
    print(json.dumps({"self_test": "ok", "passive_fill_pct": res_p["passive_hedge_fill_pct"], "blocked_z": res_p["blocked_zscore"]}))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument("--output", type=Path)
    ap.add_argument("--fee-evidence", type=Path, default=None)
    ap.add_argument("--lighter-fee-mode", choices=("standard_zero", "verified_account"), default="standard_zero")
    ap.add_argument("--max-workers", type=int, default=10)
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        run_self_test()
        return 0

    out = evaluate_lakehouse(
        args.lakehouse,
        fee_evidence_path=args.fee_evidence,
        lighter_fee_mode=args.lighter_fee_mode,
        max_workers=args.max_workers,
    )
    rendered = json.dumps(out, indent=2, allow_nan=False) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(rendered, encoding="utf-8")
        sha = hashlib.sha256(rendered.encode("utf-8")).hexdigest()
        print(json.dumps({"output": str(args.output), "sha256": sha}), flush=True)
    else:
        print(rendered, end="")
    return 0


if __name__ == "__main__":
    sys.exit(main())
