#!/usr/bin/env python3
"""Track 02 (02_hedge_arbitrage) Multi-Venue 10h Arb + Funding Carry Evaluator.

Evaluates upgraded MakerHedgeQuoter mechanics (causal basis-vol edge scaling,
nonlinear inventory skew near cap, adverse basis-drift guard, opposite-BBO L1
notional liquidity guard, and external cancel shield) across the full Aleph
Hive parquet history (`w01..w25`: `bbo_quotes`, `public_trades`, `funding_and_ctx`).

Strictly separates:
- Same-settlement `USDC/USDC` cohort (`lighter<->arcus`, `lighter<->extended`,
  `arcus<->extended`, `lighter<->trade_xyz`, `arcus<->trade_xyz`)
- Cross-stablecoin `USDG/USDC` cohort (`lighter_robinhood<->arcus`,
  `lighter_robinhood<->lighter`, `lighter_robinhood<->trade_xyz`) with explicit
  0 / 1 / 2 bp FX conversion sensitivity (`cross_stablecoin_bps`).

Protocol:
- Dynamic campaign discovery (`tokyo-{crypto,rwa}-*-w*` + cross-midnight
  `utc_date` and `utc_date + 1 day` globbing).
- `full_dev` = `w01..w20` (`ex w08` for crypto, available RWA windows `w01..w20`)
  selects frozen parameters per pair/mode.
- `holdout` = `w21..w25` (`w21..w25` crypto, `w21, w23..w25` RWA) is evaluated
  ONCE with frozen selection parameters.
- Stratifies 10h windows into `calm` / `normal` / `extreme_crash` (highlighting
  `w03`) and reports `worst_10h_window`, `max_drawdown_usd`,
  `positive_window_ratio`, and cumulative multi-window equity curves.
- Module top-level imports are 100% Python stdlib (`duckdb`/`numpy` are
  imported lazily inside lakehouse loader functions only).
"""

from __future__ import annotations

import argparse
from bisect import bisect_left
from dataclasses import dataclass
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

TEN_HOURS_MS = 36_000_000
MAX_CONTINUOUS_GAP_MS = 60_000
STEP_MS = 100
WARMUP_MS = 15 * 60_000
MAX_BOOK_AGE_MS = 5_000
CLIP_USD = 1_000.0
Q_MAX_CLIPS = 4.0
DEFAULT_SKEW_BPS = 0.5
MIN_SELECTION_FILLS = 40

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
        "AMZN": "AMZN-USD",
        "SNDK": "SNDK-USD",
    },
    "lighter": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "2",
        "NVDA": "110",
        "TSLA": "112",
        "AAPL": "113",
        "AMZN": "114",
        "SPY": "128",
        "QQQ": "129",
    },
    "lighter_robinhood": {
        "ETH": "0",
        "BTC": "1",
        "SOL": "3",
        "AAPL": "10",
        "AMZN": "11",
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
    "trade_xyz": {
        "NVDA": "xyz:NVDA",
        "AAPL": "xyz:AAPL",
        "TSLA": "xyz:TSLA",
        "AMZN": "xyz:AMZN",
        "SNDK": "xyz:SNDK",
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
        "maker_fee_bps": {"crypto": 0.0, "rwa": 0.0},
        "taker_fee_bps": {"crypto": 2.30, "rwa": 1.15},
        "bbo_cadence": "ws_event",
        "fee_status": "audited",
    },
    "lighter": {
        "settlement": "USDC",
        "order_delay_ms": 330,
        "speedbump_ms": 300,
        "maker_fee_bps": {"crypto": 0.0, "rwa": 0.0},
        "taker_fee_bps": {"crypto": 0.0, "rwa": 0.0},
        "bbo_cadence": "ws_event",
        "fee_status": "public_tier0_unverified",
    },
    "lighter_robinhood": {
        "settlement": "USDG",
        "order_delay_ms": 230,
        "speedbump_ms": 200,
        "maker_fee_bps": {"crypto": 0.0, "rwa": 0.0},
        "taker_fee_bps": {"crypto": 0.0, "rwa": 0.0},
        "bbo_cadence": "ws_event",
        "fee_status": "public_tier0_unverified",
    },
    "extended": {
        "settlement": "USDC",
        "order_delay_ms": 60,
        "speedbump_ms": 0,
        "maker_fee_bps": {"crypto": 0.0, "rwa": 0.0},
        "taker_fee_bps": {"crypto": 2.50, "rwa": 2.50},
        "bbo_cadence": "ws_event",
        "fee_status": "account_evidence_required",
    },
    "trade_xyz": {
        "settlement": "USDC",
        "order_delay_ms": 250,
        "speedbump_ms": 0,
        "maker_fee_bps": {"crypto": 0.30, "rwa": 0.30},
        "taker_fee_bps": {"crypto": 0.90, "rwa": 0.90},
        "bbo_cadence": "polled_15s_diagnostic",
        "fee_status": "account_evidence_required",
    },
}


@dataclass(frozen=True)
class ArbPairSpec:
    cohort: str  # "USDC/USDC" or "USDG/USDC"
    maker_venue: str
    hedge_venue: str
    ref_venue: str
    symbol: str
    domain: str  # "crypto" or "rwa"
    label: str

    @property
    def maker_market(self) -> str:
        return VENUE_SYMBOL_MAP[self.maker_venue][self.symbol]

    @property
    def hedge_market(self) -> str:
        return VENUE_SYMBOL_MAP[self.hedge_venue][self.symbol]

    @property
    def ref_market(self) -> str:
        return VENUE_SYMBOL_MAP[self.ref_venue][self.symbol]

    @property
    def cross_stablecoin(self) -> bool:
        return VENUE_META[self.maker_venue]["settlement"] != VENUE_META[self.hedge_venue]["settlement"]

    @property
    def same_risk_domain(self) -> bool:
        return self.maker_venue.startswith("lighter") and self.hedge_venue.startswith("lighter")

    @property
    def diagnostic_polled_venue(self) -> bool:
        return (
            VENUE_META[self.maker_venue]["bbo_cadence"] != "ws_event"
            or VENUE_META[self.hedge_venue]["bbo_cadence"] != "ws_event"
        )


def build_pair_catalog() -> list[ArbPairSpec]:
    """Build strictly separated USDC/USDC and USDG/USDC multi-venue pair cohorts."""
    pairs: list[ArbPairSpec] = []

    # Cohort 1: Same-settlement USDC/USDC (Crypto + RWA)
    for sym in ("BTC", "ETH", "SOL"):
        pairs.extend([
            ArbPairSpec("USDC/USDC", "lighter", "arcus", "binance", sym, "crypto", f"lighter->arcus {sym}"),
            ArbPairSpec("USDC/USDC", "arcus", "lighter", "binance", sym, "crypto", f"arcus->lighter {sym}"),
            ArbPairSpec("USDC/USDC", "lighter", "extended", "binance", sym, "crypto", f"lighter->extended {sym}"),
            ArbPairSpec("USDC/USDC", "arcus", "extended", "binance", sym, "crypto", f"arcus->extended {sym}"),
            ArbPairSpec("USDC/USDC", "extended", "arcus", "binance", sym, "crypto", f"extended->arcus {sym}"),
        ])
    for sym in ("NVDA", "QQQ", "SPY", "AAPL", "TSLA"):
        pairs.extend([
            ArbPairSpec("USDC/USDC", "lighter", "arcus", "lighter_robinhood", sym, "rwa", f"lighter->arcus {sym}"),
            ArbPairSpec("USDC/USDC", "arcus", "lighter", "lighter_robinhood", sym, "rwa", f"arcus->lighter {sym}"),
        ])
    for sym in ("NVDA", "AAPL", "TSLA"):
        pairs.extend([
            ArbPairSpec("USDC/USDC", "lighter", "trade_xyz", "arcus", sym, "rwa", f"lighter->trade_xyz {sym}"),
            ArbPairSpec("USDC/USDC", "arcus", "trade_xyz", "lighter", sym, "rwa", f"arcus->trade_xyz {sym}"),
        ])

    # Cohort 2: Cross-stablecoin USDG/USDC (Crypto + RWA)
    for sym in ("BTC", "ETH", "SOL"):
        pairs.extend([
            ArbPairSpec("USDG/USDC", "lighter_robinhood", "arcus", "binance", sym, "crypto", f"rh->arcus {sym}"),
            ArbPairSpec("USDG/USDC", "arcus", "lighter_robinhood", "binance", sym, "crypto", f"arcus->rh {sym}"),
            ArbPairSpec("USDG/USDC", "lighter_robinhood", "lighter", "binance", sym, "crypto", f"rh->lighter {sym}"),
        ])
    for sym in ("NVDA", "QQQ", "SPY", "AAPL", "TSLA"):
        pairs.extend([
            ArbPairSpec("USDG/USDC", "lighter_robinhood", "arcus", "lighter", sym, "rwa", f"rh->arcus {sym}"),
            ArbPairSpec("USDG/USDC", "arcus", "lighter_robinhood", "lighter", sym, "rwa", f"arcus->rh {sym}"),
            ArbPairSpec("USDG/USDC", "lighter_robinhood", "lighter", "arcus", sym, "rwa", f"rh->lighter {sym}"),
        ])
    for sym in ("NVDA", "AAPL", "TSLA"):
        pairs.append(
            ArbPairSpec("USDG/USDC", "lighter_robinhood", "trade_xyz", "arcus", sym, "rwa", f"rh->trade_xyz {sym}")
        )
    return pairs


@dataclass(frozen=True)
class StrategyParams:
    name: str
    e0_bps: float = 4.0
    tau_s: float = 1800.0
    skew_bps_per_clip: float = DEFAULT_SKEW_BPS
    nonlinear_skew_power: float = 1.0
    vol_edge_mult: float = 0.0
    basis_vol_tau_s: float = 300.0
    basis_drift_guard_bps: float = 0.0
    basis_drift_tau_s: float = 5.0
    min_hedge_l1_notional_usd: float = 0.0
    lead_beta: float = 0.0
    improve_one_tick: bool = False
    shield_bps: float | None = None
    depth_impact_bps: float = 2.0


def selection_grid(has_speedbump: bool) -> dict[str, list[StrategyParams]]:
    """Build parameter candidates for baseline_unshielded, baseline_shielded, upgraded_track2."""
    unshielded = [
        StrategyParams(name=f"base_unsh_e{int(e0)}", e0_bps=e0, tau_s=1800.0, shield_bps=None)
        for e0 in (2.0, 4.0, 6.0)
    ]
    if has_speedbump:
        shielded = [
            StrategyParams(name=f"base_sh_e{int(e0)}", e0_bps=e0, tau_s=1800.0, shield_bps=2.5)
            for e0 in (2.0, 4.0, 6.0)
        ]
    else:
        shielded = list(unshielded)

    upgraded: list[StrategyParams] = []
    sh = 2.5 if has_speedbump else None
    for e0 in (2.0, 4.0, 6.0):
        for vol_mult in (0.5, 1.0):
            upgraded.append(
                StrategyParams(
                    name=f"upg_e{int(e0)}_v{vol_mult}",
                    e0_bps=e0,
                    tau_s=1800.0,
                    skew_bps_per_clip=DEFAULT_SKEW_BPS,
                    nonlinear_skew_power=2.0,
                    vol_edge_mult=vol_mult,
                    basis_vol_tau_s=300.0,
                    basis_drift_guard_bps=3.0,
                    basis_drift_tau_s=5.0,
                    min_hedge_l1_notional_usd=CLIP_USD,
                    lead_beta=0.0,
                    improve_one_tick=False,
                    shield_bps=sh,
                )
            )
    return {
        "baseline_unshielded": unshielded,
        "baseline_shielded": shielded,
        "upgraded_track2": upgraded,
    }


def split_for_window(w_num: int) -> str:
    """Assign w01..w20 -> full_dev, w21..w25 -> holdout."""
    return "full_dev" if w_num <= 20 else "holdout"


def discover_campaigns(
    lakehouse_dir: Path, domain: str, *, min_window: int = 1, max_window: int = 25
) -> list[dict[str, Any]]:
    """Dynamically discover all campaign directories for a domain across W01..W25."""
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
            # Canonical crypto dataset excludes w08.
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


def compute_continuity_stats(timestamps_ms: Sequence[int]) -> dict[str, Any]:
    """Check whether a chronological stream satisfies continuous 10h requirements."""
    n = len(timestamps_ms)
    if n < 2:
        return {
            "events": n,
            "duration_ms": 0,
            "hours": 0.0,
            "max_gap_ms": 0,
            "is_continuous_10h": False,
        }
    dur = int(timestamps_ms[-1] - timestamps_ms[0])
    max_gap = max(int(timestamps_ms[i] - timestamps_ms[i - 1]) for i in range(1, n))
    return {
        "events": n,
        "duration_ms": dur,
        "hours": round(dur / 3_600_000.0, 3),
        "max_gap_ms": max_gap,
        "is_continuous_10h": dur >= TEN_HOURS_MS and max_gap <= MAX_CONTINUOUS_GAP_MS,
    }


def _infer_tick(prices: Sequence[float]) -> float:
    best = 1.0
    for px in prices[:1000]:
        if px > 0 and math.isfinite(px):
            s = f"{px:.8f}".rstrip("0").rstrip(".")
            if "." in s:
                best = min(best, 10.0 ** (-len(s.split(".")[1])))
    return max(best, 1e-6)


def compute_realized_1s_vol_bps(
    bid: Sequence[float], ask: Sequence[float], age: Sequence[int], *, step_ms: int = STEP_MS, max_age_steps: int = MAX_BOOK_AGE_MS // STEP_MS
) -> float:
    """Compute causal 1s-sampled log-return volatility in bps for regime stratification."""
    stride = max(1_000 // step_ms, 1)
    rets: list[float] = []
    prev_mid: float | None = None
    for k in range(0, len(bid), stride):
        if age[k] > max_age_steps:
            prev_mid = None
            continue
        b, a = float(bid[k]), float(ask[k])
        if not (b > 0.0 and a > b):
            prev_mid = None
            continue
        mid = 0.5 * (b + a)
        if prev_mid is not None and prev_mid > 0.0:
            r = math.log(mid / prev_mid) * 1e4
            if math.isfinite(r):
                rets.append(r)
        prev_mid = mid
    if len(rets) < 10:
        return 0.0
    mean_r = sum(rets) / len(rets)
    var = sum((x - mean_r) ** 2 for x in rets) / (len(rets) - 1)
    return round(math.sqrt(max(var, 0.0)), 4)


def assign_window_regimes(vol_by_window: dict[str, float]) -> dict[str, str]:
    """Stratify windows into `calm`, `normal`, and `extreme_crash` by realized 1s vol.

    Explicitly tags `w03` (canonical crash window) and extreme volatility spikes
    (`vol >= max(2.0 * median_vol, p85_vol)`) as `extreme_crash`, bottom third as
    `calm`, and remaining windows as `normal`.
    """
    if not vol_by_window:
        return {}
    vals = sorted(vol_by_window.values())
    n = len(vals)
    med = vals[n // 2]
    p33 = vals[max(0, int(0.33 * (n - 1)))]
    p85 = vals[max(0, int(0.85 * (n - 1)))]
    crash_thresh = max(2.0 * med, p85) if med > 0.0 else p85

    regimes: dict[str, str] = {}
    for w, v in vol_by_window.items():
        if w == "w03" or (crash_thresh > 0.0 and v >= crash_thresh and n >= 4):
            regimes[w] = "extreme_crash"
        elif v <= p33 and n >= 3:
            regimes[w] = "calm"
        else:
            regimes[w] = "normal"
    return regimes


def compute_regime_and_tail_metrics(window_runs: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Compute regime stratification, worst 10h window, max drawdown, positive ratio & curve."""
    if not window_runs:
        return {
            "windows_covered": 0,
            "hours_covered": 0.0,
            "positive_windows": 0,
            "positive_window_ratio": 0.0,
            "worst_10h_window": None,
            "max_drawdown_usd": 0.0,
            "by_regime": {},
            "w03_crash_window": None,
            "cumulative_curve": [],
        }
    ordered = sorted(window_runs, key=lambda r: r["window_num"])
    cum = 0.0
    peak = 0.0
    max_dd = 0.0
    curve: list[dict[str, Any]] = []
    worst: dict[str, Any] | None = None
    pos_cnt = 0
    by_reg: dict[str, list[dict[str, Any]]] = {}
    w03_rec: dict[str, Any] | None = None

    for r in ordered:
        pnl = float(r["quote_hedged_pnl_usd"])
        cum += pnl
        if cum > peak:
            peak = cum
        dd = peak - cum
        if dd > max_dd:
            max_dd = dd
        if pnl > 0.0:
            pos_cnt += 1
        if worst is None or pnl < worst["pnl_usd"]:
            worst = {
                "window": r["window"],
                "regime": r.get("regime", "normal"),
                "pnl_usd": round(pnl, 2),
                "fills": int(r["fills"]),
            }
        reg = r.get("regime", "normal")
        by_reg.setdefault(reg, []).append(r)
        pt = {
            "window": r["window"],
            "regime": reg,
            "vol_1s_bps": r.get("vol_1s_bps", 0.0),
            "fills": int(r["fills"]),
            "pnl_usd": round(pnl, 2),
            "cum_pnl_usd": round(cum, 2),
        }
        curve.append(pt)
        if r["window"] == "w03":
            w03_rec = {
                "window": "w03",
                "regime": reg,
                "vol_1s_bps": r.get("vol_1s_bps", 0.0),
                "fills": int(r["fills"]),
                "quote_hedged_pnl_usd": round(pnl, 2),
                "print_verified_pnl_usd": r.get("print_verified_pnl_usd"),
                "blocked_toxic": int(r.get("blocked_toxic", 0)),
                "blocked_drift": int(r.get("blocked_drift", 0)),
                "blocked_l1_thin": int(r.get("blocked_l1_thin", 0)),
            }

    reg_summary: dict[str, Any] = {}
    for reg_name in ("calm", "normal", "extreme_crash"):
        items = by_reg.get(reg_name, [])
        if not items:
            continue
        pr_vals = [x["print_verified_pnl_usd"] for x in items if x.get("print_verified_pnl_usd") is not None]
        reg_summary[reg_name] = {
            "windows": len(items),
            "fills": sum(int(x["fills"]) for x in items),
            "maker_notional_usd": round(sum(float(x["maker_notional_usd"]) for x in items), 2),
            "quote_hedged_pnl_usd": round(sum(float(x["quote_hedged_pnl_usd"]) for x in items), 2),
            "print_verified_pnl_usd": round(sum(pr_vals), 2) if pr_vals else None,
            "funding_carry_usd": round(sum(float(x["funding_carry_usd"]) for x in items), 2),
        }

    return {
        "windows_covered": len(ordered),
        "hours_covered": round(sum(float(r.get("span_hours", 0.0)) for r in ordered), 2),
        "positive_windows": pos_cnt,
        "positive_window_ratio": round(pos_cnt / len(ordered), 4),
        "worst_10h_window": worst,
        "max_drawdown_usd": round(max_dd, 2),
        "by_regime": reg_summary,
        "w03_crash_window": w03_rec,
        "cumulative_curve": curve,
    }


def compute_causal_basis_series(
    mk_bid: Sequence[float],
    mk_ask: Sequence[float],
    mk_age: Sequence[int],
    hd_bid: Sequence[float],
    hd_ask: Sequence[float],
    hd_age: Sequence[int],
    *,
    step_ms: int = STEP_MS,
    tau_ms: int = 1_800_000,
    vol_tau_ms: int = 300_000,
    drift_tau_ms: int = 5_000,
    max_age_steps: int = MAX_BOOK_AGE_MS // STEP_MS,
    max_gap_steps: int = MAX_CONTINUOUS_GAP_MS // STEP_MS,
) -> dict[str, list[float]]:
    """Compute strictly causal basis, fair EMA, basis sigma (bps), and fast-EMA drift baseline.

    Matches `MakerHedgeQuoter` semantics: stale maker/hedge frames never pollute
    fair EMA, basis variance, or fast drift state, and gaps > max_gap_steps re-seed cleanly.
    """
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
        b = log(0.5 * (mb + ma)) - log(0.5 * (hb + ha))
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
    mk_trades: dict[str, Sequence[Any]] | None,
    t0_ms: int,
    maker_delay_ms: int,
    n_steps: int,
    *,
    step_ms: int = STEP_MS,
    warmup_steps: int = WARMUP_MS // STEP_MS,
    decision_steps: int = TEN_HOURS_MS // STEP_MS,
) -> dict[int, list[int]]:
    """Bucket maker print indices once by causal step `k`."""
    if mk_trades is None or len(mk_trades["ts"]) == 0:
        return {}
    tr_ts = mk_trades["ts"]
    last_k = min(warmup_steps + decision_steps, n_steps)
    by_step: dict[int, list[int]] = {}
    for idx_t in range(len(tr_ts)):
        k = int((int(tr_ts[idx_t]) - maker_delay_ms - t0_ms) // step_ms) - 1
        if warmup_steps <= k < last_k:
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
    params: StrategyParams,
    maker_delay_ms: int,
    hedge_delay_ms: int,
    speedbump_ms: int,
    rtt_ms: int,
    maker_fee_bps: float,
    maker_taker_fee_bps: float,
    hedge_fee_bps: float,
    step_ms: int = STEP_MS,
    warmup_steps: int = WARMUP_MS // STEP_MS,
    decision_steps: int = TEN_HOURS_MS // STEP_MS,
    max_age_steps: int = MAX_BOOK_AGE_MS // STEP_MS,
    clip_usd: float = CLIP_USD,
    q_max_clips: float = Q_MAX_CLIPS,
    cross_stablecoin_bps: float = 0.0,
    maker_fr_bps_hr: float | None = None,
    hedge_fr_bps_hr: float | None = None,
    pre_by_step: dict[int, list[int]] | None = None,
    pre_hp_ts: list[int] | None = None,
) -> dict[str, Any]:
    """Causal maker-first basis arb replay with L1 slippage, shield, print check & funding carry."""
    n = len(mk_bid)
    empty = {
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
        "mean_quote_hedge_slip_bps": 0.0,
        "mean_expected_hedge_slip_bps": 0.0,
        "mean_effective_min_edge_bps": params.e0_bps,
        "blocked_toxic": 0,
        "blocked_drift": 0,
        "blocked_l1_thin": 0,
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
    by_step = (
        pre_by_step
        if pre_by_step is not None
        else bucket_trades_by_step(
            mk_trades,
            t0_ms,
            maker_delay_ms,
            n,
            step_ms=step_ms,
            warmup_steps=warmup_steps,
            decision_steps=decision_steps,
        )
    )

    if hd_trades is not None and len(hd_trades["ts"]) > 0:
        hp_ts = pre_hp_ts if pre_hp_ts is not None else [int(x) for x in hd_trades["ts"]]
        hp_sgn = hd_trades["sgn"]
        hp_px = hd_trades["px"]
    else:
        hp_ts = None
        hp_sgn = None
        hp_px = None

    pos_mk = 0.0
    pos_hd = 0.0
    cash = 0.0
    fees_usd = 0.0
    fx_drag_usd = 0.0
    depth_slip_usd = 0.0
    mk_notional = 0.0
    fills_bid = 0
    fills_ask = 0
    blocked_toxic = 0
    blocked_drift = 0
    blocked_l1_thin = 0
    unhedged_stale_halts = 0
    print_delta = 0.0
    print_checked = 0

    quote_slips: list[float] = []
    expected_slips: list[float] = []
    eff_edges: list[float] = []

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

    for k in sorted(by_step.keys()):
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
        q = pos_mk * mk_mid_k / clip_usd

        sigma_k = prior_sigma[k]
        eff_e0 = params.e0_bps + (params.vol_edge_mult * sigma_k if params.vol_edge_mult > 0.0 else 0.0)
        eff_edges.append(eff_e0)
        sk_q = _skew_q(q, q_max_clips, params.nonlinear_skew_power)

        drift_block_bid = False
        drift_block_ask = False
        if params.basis_drift_guard_bps > 0.0 and math.isfinite(prior_fast[k]):
            drift_bps = (b_now - prior_fast[k]) * 1e4
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

        shift = math.exp(params.lead_beta * (b_now - fb)) if params.lead_beta else 1.0
        hb_adj, ha_adj = hb * shift, ha * shift
        can_imp = params.improve_one_tick and (ma - mb) > 1.5 * tick

        quotes: list[tuple[int, float]] = []
        if q < q_max_clips:
            if drift_block_bid:
                blocked_drift += 1
            elif l1_block_bid:
                blocked_l1_thin += 1
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
            else:
                e_ask = eff_e0 - params.skew_bps_per_clip * sk_q
                floor_ = ha_adj * math.exp(fb + e_ask * 1e-4)
                join = ma - tick if can_imp else ma
                ask_q = max(join, math.ceil(floor_ / tick - 1e-9) * tick)
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
            if first_tf is None or through_notl <= 0.0:
                continue

            if params.shield_bps is not None and speedbump_ms > 2 * rtt_ms and len(ref_mid) == n:
                i0 = _ref_idx(first_tf - speedbump_ms + rtt_ms)
                i1 = _ref_idx(first_tf - rtt_ms)
                r0, r1 = float(ref_mid[i0]), float(ref_mid[i1])
                if ref_age[i0] <= max_age_steps and ref_age[i1] <= max_age_steps and r0 > 0.0 and r1 > 0.0:
                    r_bps = (r1 / r0 - 1.0) * 1e4
                    if (side == 1 and r_bps <= -params.shield_bps) or (side == -1 and r_bps >= params.shield_bps):
                        blocked_toxic += 1
                        continue

            fill_notl = min(clip_usd, through_notl)
            qty = fill_notl / qp

            j = min(max(int((first_tf + hedge_delay_ms - t0_ms) // step_ms) - 1, 0), n - 1)
            if hd_age[j] > max_age_steps:
                j_f = j
                while j_f < n - 1 and hd_age[j_f] > max_age_steps and (j_f - j) < 600:
                    j_f += 1
                j = j_f
            if hd_age[j] > max_age_steps:
                unhedged_stale_halts += 1
                continue

            h_px = float(hd_bid[j] if side == 1 else hd_ask[j])
            h_sz = float(hd_bid_sz[j] if side == 1 else hd_ask_sz[j])
            h_ref = float(hb if side == 1 else ha)
            if not (h_px > 0.0 and math.isfinite(h_px)):
                unhedged_stale_halts += 1
                continue

            l1_hedge_usd = h_px * h_sz if (math.isfinite(h_sz) and h_sz > 0.0) else 0.0
            if l1_hedge_usd <= 0.0:
                depth_slip_bps = 10.0
            elif fill_notl > l1_hedge_usd:
                depth_slip_bps = min(params.depth_impact_bps * (fill_notl / l1_hedge_usd - 1.0), 25.0)
            else:
                depth_slip_bps = 0.0

            q_slip_bps = side * (h_px / h_ref - 1.0) * 1e4
            quote_slips.append(q_slip_bps)
            expected_slips.append(depth_slip_bps + max(0.0, -q_slip_bps))

            cash -= side * qty * qp
            pos_mk += side * qty
            cash += side * qty * h_px
            pos_hd -= side * qty

            mk_fee = fill_notl * maker_fee_bps * 1e-4
            hd_notl = qty * h_px
            hd_fee = hd_notl * hedge_fee_bps * 1e-4
            d_slip_cost = hd_notl * depth_slip_bps * 1e-4
            fx_cost = fill_notl * cross_stablecoin_bps * 1e-4 if cross_stablecoin_bps > 0.0 else 0.0

            fees_usd += mk_fee + hd_fee
            depth_slip_usd += d_slip_cost
            fx_drag_usd += fx_cost
            mk_notional += fill_notl

            if hp_ts is not None and hp_sgn is not None and hp_px is not None:
                t_h = first_tf + hedge_delay_ms
                lo = bisect_left(hp_ts, t_h)
                hi = bisect_left(hp_ts, t_h + 1_000)
                if hi > lo:
                    vals = [float(hp_px[m]) for m in range(lo, hi) if int(hp_sgn[m]) == -side]
                    if vals:
                        p_print = sum(vals) / len(vals)
                        print_delta += side * qty * (p_print - h_px)
                        print_checked += 1

            if side == 1:
                fills_bid += 1
            else:
                fills_ask += 1

    end_k = min(warmup_steps + decision_steps - 1, n - 1)
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
        drain_fees += abs(pos_hd) * p_hd * hedge_fee_bps * 1e-4

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

    return {
        "fills": fills,
        "fills_bid": fills_bid,
        "fills_ask": fills_ask,
        "maker_notional_usd": round(mk_notional, 2),
        "quote_hedged_pnl_usd": round(pnl, 4),
        "print_verified_pnl_usd": round(pnl + print_delta, 4) if hp_ts is not None else None,
        "hedge_print_checked": print_checked,
        "hedge_print_coverage_pct": round(100.0 * print_checked / fills, 2) if fills else 0.0,
        "fees_usd": round(fees_usd + drain_fees, 4),
        "fx_drag_usd": round(fx_drag_usd, 4),
        "depth_slip_usd": round(depth_slip_usd, 4),
        "mean_quote_hedge_slip_bps": round(sum(quote_slips) / len(quote_slips), 3) if quote_slips else 0.0,
        "mean_expected_hedge_slip_bps": round(sum(expected_slips) / len(expected_slips), 3) if expected_slips else 0.0,
        "mean_effective_min_edge_bps": round(sum(eff_edges) / len(eff_edges), 3) if eff_edges else params.e0_bps,
        "blocked_toxic": blocked_toxic,
        "blocked_drift": blocked_drift,
        "blocked_l1_thin": blocked_l1_thin,
        "unhedged_stale_halts": unhedged_stale_halts,
        "terminal_drain_usd": round(mtm_pre - pnl, 4),
        "funding_carry_usd": round(carry_usd, 4),
        "pnl_plus_funding_usd": round(pnl + carry_usd, 4),
        "funding_available_legs": int(maker_fr_bps_hr is not None) + int(hedge_fr_bps_hr is not None),
    }


def campaign_parquet_files(
    lakehouse_dir: Path, dataset: str, venue: str, utc_date: str, campaign_id: str
) -> list[str]:
    """Cross-midnight Hive parquet globbing over `utc_date` and `utc_date + 1`."""
    date_dirs = [utc_date]
    try:
        next_day = (date.fromisoformat(utc_date) + timedelta(days=1)).isoformat()
        date_dirs.append(next_day)
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


def _sql_list(files: Sequence[str]) -> str:
    return "[" + ", ".join("'" + f.replace("'", "''") + "'" for f in files) + "]"


def connect_duckdb():
    """Open read-only in-memory DuckDB with strict 2-thread / 2GB memory cap."""
    import duckdb

    con = duckdb.connect(":memory:")
    con.execute("SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';")
    return con


def _build_grid_fast(
    cols: dict[str, Any] | None, t0_ms: int, t1_ms: int, step_ms: int = STEP_MS
) -> dict[str, Any]:
    """Lazy-numpy fast grid builder for lakehouse execution."""
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


def _load_bbo_buckets_np(
    con, files: Sequence[str], market: str, step_ms: int = STEP_MS
) -> tuple[dict[str, Any] | None, dict[str, Any]]:
    if not files:
        return None, {"events": 0, "duration_ms": 0, "hours": 0.0, "max_gap_ms": 0, "is_continuous_10h": False}
    src = _sql_list(files)
    st = con.execute(
        f"""
        WITH s AS (
          SELECT available_at_ms, LAG(available_at_ms) OVER (ORDER BY available_at_ms) AS prev_ts
          FROM read_parquet({src})
          WHERE market = ? AND tradable AND bid_px_f64 > 0 AND ask_px_f64 > bid_px_f64
        )
        SELECT COUNT(*), MIN(available_at_ms), MAX(available_at_ms), COALESCE(MAX(available_at_ms - prev_ts), 0)
        FROM s
        """,
        [market],
    ).fetchone()
    cnt, t0, t1, mgap = st
    if not cnt or cnt < 2 or t0 is None or t1 is None:
        return None, {"events": int(cnt or 0), "duration_ms": 0, "hours": 0.0, "max_gap_ms": 0, "is_continuous_10h": False}
    dur = int(t1 - t0)
    cont = {
        "events": int(cnt),
        "t0_ms": int(t0),
        "t1_ms": int(t1),
        "duration_ms": dur,
        "hours": round(dur / 3_600_000.0, 3),
        "max_gap_ms": int(mgap),
        "is_continuous_10h": dur >= TEN_HOURS_MS and int(mgap) <= MAX_CONTINUOUS_GAP_MS,
    }
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
    return cols, cont


def _load_trades_np(
    con, files: Sequence[str], market: str, t0_ms: int, t1_ms: int
) -> dict[str, Any] | None:
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
    con, lakehouse_dir: Path, venue: str, symbol: str, raw_market: str, utc_date: str, campaign_id: str
) -> float | None:
    """Load mean hourly funding rate (bps/hr) from `funding_and_ctx` when available."""
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


def evaluate_lakehouse(lakehouse_dir: Path, max_window: int = 25) -> dict[str, Any]:
    """Execute read-only W01..W25 multi-venue Track 02 evaluation window-by-window."""
    con = connect_duckdb()
    all_pairs = build_pair_catalog()
    campaigns_by_dom = {
        "crypto": discover_campaigns(lakehouse_dir, "crypto", min_window=1, max_window=max_window),
        "rwa": discover_campaigns(lakehouse_dir, "rwa", min_window=1, max_window=max_window),
    }

    # Per-(pair.label, mode_name, cand_name, window) compact results stored cleanly.
    cand_runs: dict[tuple[str, str, str], list[dict[str, Any]]] = {}
    cand_lookup: dict[tuple[str, str, str], StrategyParams] = {}
    pair_meta: dict[str, dict[str, Any]] = {}

    for dom in ("crypto", "rwa"):
        dom_pairs = [p for p in all_pairs if p.domain == dom]
        for p in dom_pairs:
            pair_meta[p.label] = {
                "label": p.label,
                "cohort": p.cohort,
                "domain": p.domain,
                "maker_venue": p.maker_venue,
                "hedge_venue": p.hedge_venue,
                "ref_venue": p.ref_venue,
                "cross_stablecoin": p.cross_stablecoin,
                "same_risk_domain": p.same_risk_domain,
                "diagnostic_polled_venue": p.diagnostic_polled_venue,
                "maker_fee_status": VENUE_META[p.maker_venue]["fee_status"],
                "hedge_fee_status": VENUE_META[p.hedge_venue]["fee_status"],
            }

        for camp in campaigns_by_dom[dom]:
            w_name = camp["window"]
            w_num = camp["window_num"]
            cid = camp["campaign_id"]
            utc_date = camp["utc_date"]
            split = camp["split"]

            raw_bbo_cache: dict[tuple[str, str], tuple[dict[str, Any] | None, dict[str, Any]]] = {}
            raw_tr_cache: dict[tuple[str, str], dict[str, Any] | None] = {}
            fund_cache: dict[tuple[str, str], float | None] = {}

            def get_raw_bbo(venue: str, sym: str):
                k = (venue, sym)
                if k not in raw_bbo_cache:
                    fs = campaign_parquet_files(lakehouse_dir, "bbo_quotes", venue, utc_date, cid)
                    raw_bbo_cache[k] = _load_bbo_buckets_np(con, fs, VENUE_SYMBOL_MAP[venue][sym], STEP_MS)
                return raw_bbo_cache[k]

            def get_raw_tr(venue: str, sym: str, t0_ms: int, t1_ms: int):
                k = (venue, sym)
                if k not in raw_tr_cache:
                    fs = campaign_parquet_files(lakehouse_dir, "public_trades", venue, utc_date, cid)
                    raw_tr_cache[k] = _load_trades_np(con, fs, VENUE_SYMBOL_MAP[venue][sym], t0_ms, t1_ms)
                return raw_tr_cache[k]

            def get_fund(venue: str, sym: str) -> float | None:
                k = (venue, sym)
                if k not in fund_cache:
                    fund_cache[k] = _load_mean_funding_bps_hr(
                        con, lakehouse_dir, venue, sym, VENUE_SYMBOL_MAP[venue][sym], utc_date, cid
                    )
                return fund_cache[k]

            for pair in dom_pairs:
                mk_cols, mk_cont = get_raw_bbo(pair.maker_venue, pair.symbol)
                hd_cols, hd_cont = get_raw_bbo(pair.hedge_venue, pair.symbol)
                rf_cols, rf_cont = get_raw_bbo(pair.ref_venue, pair.symbol)
                if not mk_cont.get("t0_ms") or not hd_cont.get("t0_ms"):
                    continue
                t0 = max(int(mk_cont["t0_ms"]), int(hd_cont["t0_ms"]))
                t1 = min(int(mk_cont["t1_ms"]), int(hd_cont["t1_ms"]))
                if rf_cont.get("t0_ms"):
                    t0 = max(t0, int(rf_cont["t0_ms"]))
                    t1 = min(t1, int(rf_cont["t1_ms"]))
                if t1 - t0 < WARMUP_MS + 60_000:
                    continue

                mk_g = _build_grid_fast(mk_cols, t0, t1, STEP_MS)
                hd_g = _build_grid_fast(hd_cols, t0, t1, STEP_MS)
                rf_g = _build_grid_fast(rf_cols, t0, t1, STEP_MS) if rf_cols is not None else None
                if rf_g is not None:
                    rf_mid = 0.5 * (rf_g["bid"] + rf_g["ask"])
                    rf_age = rf_g["age"]
                else:
                    rf_mid = []
                    rf_age = []

                causal = compute_causal_basis_series(
                    mk_g["bid"], mk_g["ask"], mk_g["age"], hd_g["bid"], hd_g["ask"], hd_g["age"]
                )
                vol_1s = compute_realized_1s_vol_bps(hd_g["bid"], hd_g["ask"], hd_g["age"])
                mk_tr = get_raw_tr(pair.maker_venue, pair.symbol, t0, t1)
                hd_tr = get_raw_tr(pair.hedge_venue, pair.symbol, t0, t1)
                mk_fr = get_fund(pair.maker_venue, pair.symbol)
                hd_fr = get_fund(pair.hedge_venue, pair.symbol)

                span_ms = t1 - t0
                is_cont = (
                    span_ms >= WARMUP_MS + TEN_HOURS_MS
                    and mk_cont["max_gap_ms"] <= MAX_CONTINUOUS_GAP_MS
                    and hd_cont["max_gap_ms"] <= MAX_CONTINUOUS_GAP_MS
                )
                inc_reasons: list[str] = []
                if span_ms < WARMUP_MS + TEN_HOURS_MS:
                    inc_reasons.append(f"short_span_{round(span_ms / 3_600_000.0, 2)}h")
                if mk_cont["max_gap_ms"] > MAX_CONTINUOUS_GAP_MS:
                    inc_reasons.append(f"maker_gap_{mk_cont['max_gap_ms']}ms")
                if hd_cont["max_gap_ms"] > MAX_CONTINUOUS_GAP_MS:
                    inc_reasons.append(f"hedge_gap_{hd_cont['max_gap_ms']}ms")

                mv, hv = pair.maker_venue, pair.hedge_venue
                mm, hm = VENUE_META[mv], VENUE_META[hv]
                tick = _infer_tick(mk_cols["bid"]) if mk_cols is not None else 0.01
                n_steps = len(mk_g["bid"])
                pre_by_step = bucket_trades_by_step(mk_tr, t0, mm["order_delay_ms"], n_steps)
                pre_hp_ts = [int(x) for x in hd_tr["ts"]] if (hd_tr is not None and len(hd_tr["ts"]) > 0) else None

                has_sb = mm["speedbump_ms"] > 0
                cands = selection_grid(has_sb)
                for mode_name, cand_list in cands.items():
                    for cand in cand_list:
                        key = (pair.label, mode_name, cand.name)
                        cand_lookup[key] = cand
                        fx_list = (0.0, 1.0, 2.0) if pair.cross_stablecoin else (0.0,)
                        fx_pnls: dict[str, Any] = {}
                        base_res: dict[str, Any] | None = None
                        for fx in fx_list:
                            res = simulate_pair_arb(
                                mk_bid=mk_g["bid"],
                                mk_ask=mk_g["ask"],
                                mk_age=mk_g["age"],
                                hd_bid=hd_g["bid"],
                                hd_ask=hd_g["ask"],
                                hd_bid_sz=hd_g["bsz"],
                                hd_ask_sz=hd_g["asz"],
                                hd_age=hd_g["age"],
                                ref_mid=rf_mid,
                                ref_age=rf_age,
                                causal=causal,
                                mk_trades=mk_tr,
                                hd_trades=hd_tr,
                                t0_ms=t0,
                                tick=tick,
                                params=cand,
                                maker_delay_ms=mm["order_delay_ms"],
                                hedge_delay_ms=hm["order_delay_ms"],
                                speedbump_ms=mm["speedbump_ms"],
                                rtt_ms=40,
                                maker_fee_bps=mm["maker_fee_bps"][dom],
                                maker_taker_fee_bps=mm["taker_fee_bps"][dom],
                                hedge_fee_bps=hm["taker_fee_bps"][dom],
                                cross_stablecoin_bps=fx,
                                maker_fr_bps_hr=mk_fr,
                                hedge_fr_bps_hr=hd_fr,
                                pre_by_step=pre_by_step,
                                pre_hp_ts=pre_hp_ts,
                            )
                            if fx == 0.0:
                                base_res = res
                            fx_pnls[f"{int(fx)}bp"] = {
                                "pnl_usd": res["quote_hedged_pnl_usd"],
                                "print_pnl_usd": res["print_verified_pnl_usd"],
                            }
                        assert base_res is not None
                        cand_runs.setdefault(key, []).append({
                            **base_res,
                            "window": w_name,
                            "window_num": w_num,
                            "split": split,
                            "span_hours": round(span_ms / 3_600_000.0, 3),
                            "is_continuous_10h": is_cont,
                            "incomplete_reason": ",".join(inc_reasons) if inc_reasons else None,
                            "vol_1s_bps": vol_1s,
                            "fx_pnls": fx_pnls if pair.cross_stablecoin else None,
                        })

            raw_bbo_cache.clear()
            raw_tr_cache.clear()
            fund_cache.clear()
            gc.collect()
            print(json.dumps({"completed_campaign": cid, "domain": dom, "split": split}), flush=True)

    con.close()

    def _agg_split(runs: list[dict[str, Any]]) -> dict[str, Any]:
        if not runs:
            return {"windows": 0, "windows_covered": 0, "hours_covered": 0.0}
        pr_vals = [r["print_verified_pnl_usd"] for r in runs if r["print_verified_pnl_usd"] is not None]
        slips = [r["mean_expected_hedge_slip_bps"] for r in runs if r["fills"] > 0]
        eff_edges = [r["mean_effective_min_edge_bps"] for r in runs if r["fills"] > 0]
        covs = [r["hedge_print_coverage_pct"] for r in runs if r["fills"] > 0]
        tail = compute_regime_and_tail_metrics(runs)
        return {
            "windows": len(runs),
            "continuous_10h_windows": sum(1 for r in runs if r["is_continuous_10h"]),
            "incomplete_windows": [
                {"window": r["window"], "reason": r["incomplete_reason"]}
                for r in runs
                if not r["is_continuous_10h"]
            ],
            "fills": sum(r["fills"] for r in runs),
            "maker_notional_usd": round(sum(r["maker_notional_usd"] for r in runs), 2),
            "quote_hedged_pnl_usd": round(sum(r["quote_hedged_pnl_usd"] for r in runs), 2),
            "print_verified_pnl_usd": round(sum(pr_vals), 2) if pr_vals else None,
            "hedge_print_coverage_pct": round(sum(covs) / len(covs), 2) if covs else 0.0,
            "funding_carry_usd": round(sum(r["funding_carry_usd"] for r in runs), 2),
            "pnl_plus_funding_usd": round(sum(r["pnl_plus_funding_usd"] for r in runs), 2),
            "depth_slip_usd": round(sum(r["depth_slip_usd"] for r in runs), 2),
            "mean_expected_hedge_slip_bps": round(sum(slips) / len(slips), 3) if slips else 0.0,
            "mean_effective_min_edge_bps": round(sum(eff_edges) / len(eff_edges), 3) if eff_edges else 0.0,
            "blocked_toxic": sum(r["blocked_toxic"] for r in runs),
            "blocked_drift": sum(r["blocked_drift"] for r in runs),
            "blocked_l1_thin": sum(r["blocked_l1_thin"] for r in runs),
            "unhedged_stale_halts": sum(r["unhedged_stale_halts"] for r in runs),
            **tail,
        }

    pair_records: list[dict[str, Any]] = []
    for pair in all_pairs:
        meta = pair_meta[pair.label]
        has_sb = VENUE_META[pair.maker_venue]["speedbump_ms"] > 0
        cands = selection_grid(has_sb)
        mode_results: dict[str, Any] = {}
        any_dev = False

        for mode_name, cand_list in cands.items():
            best_cand: StrategyParams | None = None
            best_score = -1e18
            best_dev_runs: list[dict[str, Any]] = []
            best_hold_runs: list[dict[str, Any]] = []

            for cand in cand_list:
                runs = cand_runs.get((pair.label, mode_name, cand.name), [])
                dev_runs = [r for r in runs if r["split"] == "full_dev"]
                hold_runs = [r for r in runs if r["split"] == "holdout"]
                if not dev_runs:
                    continue
                any_dev = True
                dev_fills = sum(r["fills"] for r in dev_runs)
                dev_pnl = sum(r["quote_hedged_pnl_usd"] for r in dev_runs)
                score = dev_pnl if dev_fills >= MIN_SELECTION_FILLS else (-1e12 + dev_pnl)
                if score > best_score or best_cand is None:
                    best_score = score
                    best_cand = cand
                    best_dev_runs = dev_runs
                    best_hold_runs = hold_runs

            if best_cand is None:
                continue

            # Assign regimes across all covered windows for this pair so quantiles are well-calibrated.
            vol_map = {r["window"]: float(r["vol_1s_bps"]) for r in (best_dev_runs + best_hold_runs)}
            reg_map = assign_window_regimes(vol_map)
            for r in best_dev_runs:
                r["regime"] = reg_map.get(r["window"], "normal")
            for r in best_hold_runs:
                r["regime"] = reg_map.get(r["window"], "normal")

            fx_sens: dict[str, Any] = {}
            if pair.cross_stablecoin:
                for fx_k in ("0bp", "1bp", "2bp"):
                    fx_sens[fx_k] = {
                        "full_dev_pnl_usd": round(sum(r["fx_pnls"][fx_k]["pnl_usd"] for r in best_dev_runs), 2),
                        "holdout_pnl_usd": round(sum(r["fx_pnls"][fx_k]["pnl_usd"] for r in best_hold_runs), 2),
                    }

            mode_results[mode_name] = {
                "frozen_params": {
                    "name": best_cand.name,
                    "e0_bps": best_cand.e0_bps,
                    "vol_edge_mult": best_cand.vol_edge_mult,
                    "nonlinear_skew_power": best_cand.nonlinear_skew_power,
                    "basis_drift_guard_bps": best_cand.basis_drift_guard_bps,
                    "min_hedge_l1_notional_usd": best_cand.min_hedge_l1_notional_usd,
                    "shield_bps": best_cand.shield_bps,
                },
                "full_dev": _agg_split(best_dev_runs),
                "holdout": _agg_split(best_hold_runs),
                "cross_stablecoin_fx_sensitivity": fx_sens if pair.cross_stablecoin else None,
            }

        if not any_dev:
            pair_records.append({**meta, "status": "missing_full_dev_data"})
            continue

        pair_records.append({**meta, "status": "evaluated", "modes": mode_results})

    def _cohort_summary(cohort_name: str, exclude_same_domain: bool, exclude_diagnostic: bool) -> dict[str, Any]:
        rows = [
            r
            for r in pair_records
            if r.get("status") == "evaluated"
            and r.get("cohort") == cohort_name
            and (not exclude_same_domain or not r.get("same_risk_domain"))
            and (not exclude_diagnostic or not r.get("diagnostic_polled_venue"))
        ]
        summary: dict[str, Any] = {"pairs": len(rows)}
        for mode in ("baseline_unshielded", "baseline_shielded", "upgraded_track2"):
            mode_out: dict[str, Any] = {}
            for split_k in ("full_dev", "holdout"):
                aggs = [r["modes"][mode][split_k] for r in rows if mode in r["modes"] and r["modes"][mode][split_k]["windows"] > 0]
                pr = [a["print_verified_pnl_usd"] for a in aggs if a.get("print_verified_pnl_usd") is not None]
                reg_tot: dict[str, dict[str, Any]] = {}
                for reg_name in ("calm", "normal", "extreme_crash"):
                    r_items = [a["by_regime"][reg_name] for a in aggs if reg_name in a.get("by_regime", {})]
                    if r_items:
                        r_pr = [x["print_verified_pnl_usd"] for x in r_items if x.get("print_verified_pnl_usd") is not None]
                        reg_tot[reg_name] = {
                            "pair_windows": sum(x["windows"] for x in r_items),
                            "fills": sum(x["fills"] for x in r_items),
                            "quote_hedged_pnl_usd": round(sum(x["quote_hedged_pnl_usd"] for x in r_items), 2),
                            "print_verified_pnl_usd": round(sum(r_pr), 2) if r_pr else None,
                        }
                w03_items = [a["w03_crash_window"] for a in aggs if a.get("w03_crash_window")]
                mode_out[split_k] = {
                    "pair_windows": sum(a["windows"] for a in aggs),
                    "hours_covered": round(sum(a["hours_covered"] for a in aggs), 2),
                    "continuous_10h_windows": sum(a["continuous_10h_windows"] for a in aggs),
                    "fills": sum(a["fills"] for a in aggs),
                    "maker_notional_usd": round(sum(a["maker_notional_usd"] for a in aggs), 2),
                    "quote_hedged_pnl_usd": round(sum(a["quote_hedged_pnl_usd"] for a in aggs), 2),
                    "print_verified_pnl_usd": round(sum(pr), 2) if pr else None,
                    "funding_carry_usd": round(sum(a["funding_carry_usd"] for a in aggs), 2),
                    "pnl_plus_funding_usd": round(sum(a["pnl_plus_funding_usd"] for a in aggs), 2),
                    "blocked_toxic": sum(a["blocked_toxic"] for a in aggs),
                    "blocked_drift": sum(a["blocked_drift"] for a in aggs),
                    "blocked_l1_thin": sum(a["blocked_l1_thin"] for a in aggs),
                    "unhedged_stale_halts": sum(a["unhedged_stale_halts"] for a in aggs),
                    "max_pair_drawdown_usd": round(max((a["max_drawdown_usd"] for a in aggs), default=0.0), 2),
                    "by_regime": reg_tot,
                    "w03_crash_pnl_usd": round(sum(w["quote_hedged_pnl_usd"] for w in w03_items), 2) if w03_items else None,
                }
            if cohort_name == "USDG/USDC":
                fx_tot: dict[str, dict[str, float]] = {}
                for fx_k in ("0bp", "1bp", "2bp"):
                    fx_rows = [
                        r["modes"][mode]["cross_stablecoin_fx_sensitivity"][fx_k]
                        for r in rows
                        if mode in r["modes"] and r["modes"][mode].get("cross_stablecoin_fx_sensitivity")
                    ]
                    fx_tot[fx_k] = {
                        "full_dev_pnl_usd": round(sum(x["full_dev_pnl_usd"] for x in fx_rows), 2),
                        "holdout_pnl_usd": round(sum(x["holdout_pnl_usd"] for x in fx_rows), 2),
                    }
                mode_out["fx_sensitivity"] = fx_tot
            summary[mode] = mode_out
        return summary

    return {
        "schema": "track02_multivenue_arb_upgrade_v2_w01_w25",
        "splits": {"full_dev": "w01..w20 (ex w08 crypto)", "holdout": "w21..w25"},
        "discovered_campaigns": {
            dom: {"count": len(cs), "windows": [c["window"] for c in cs]}
            for dom, cs in campaigns_by_dom.items()
        },
        "protocol": {
            "step_ms": STEP_MS,
            "warmup_ms": WARMUP_MS,
            "decision_ms": TEN_HOURS_MS,
            "clip_usd": CLIP_USD,
            "q_max_clips": Q_MAX_CLIPS,
            "fill_rule": "strict trade-through prints only (no at-touch queue credit)",
        },
        "cohort_summaries": {
            "USDC_USDC_ws_distinct_domain": _cohort_summary("USDC/USDC", exclude_same_domain=True, exclude_diagnostic=True),
            "USDC_USDC_all_including_diagnostic": _cohort_summary("USDC/USDC", exclude_same_domain=True, exclude_diagnostic=False),
            "USDG_USDC_ws_distinct_domain": _cohort_summary("USDG/USDC", exclude_same_domain=True, exclude_diagnostic=True),
            "USDG_USDC_all_including_same_domain": _cohort_summary("USDG/USDC", exclude_same_domain=False, exclude_diagnostic=False),
        },
        "pairs": pair_records,
    }


def run_self_test() -> None:
    """Pure-stdlib synthetic verification of causal series, guards, FX drag, regimes & tail risk."""
    n = 120
    mk_bid = [99.99] * n
    mk_ask = [100.01] * n
    mk_age = [0] * n
    hd_bid = [99.99] * n
    hd_ask = [100.01] * n
    hd_bsz = [20.0] * n
    hd_asz = [20.0] * n
    hd_age = [0] * n
    ref_mid = [100.0] * n
    ref_age = [0] * n

    for k in range(10, 40):
        if k % 2 == 0:
            mk_bid[k], mk_ask[k] = 100.09, 100.11
        else:
            mk_bid[k], mk_ask[k] = 99.89, 99.91
    hd_bsz[60] = 2.0
    mk_trades = {
        "ts": [60 * STEP_MS + 100 + 40, 80 * STEP_MS + 100 + 40],
        "sgn": [-1, -1],
        "px": [99.85, 99.85],
        "sz": [15.0, 15.0],
    }
    hd_trades = {
        "ts": [80 * STEP_MS + 100 + 80],
        "sgn": [-1],
        "px": [99.98],
        "sz": [15.0],
    }
    causal = compute_causal_basis_series(mk_bid, mk_ask, mk_age, hd_bid, hd_ask, hd_age, vol_tau_ms=1_000)
    assert causal["prior_sigma_bps"][60] > 2.5, f"expected causal vol > 2.5bps, got {causal['prior_sigma_bps'][60]}"

    p_l1 = StrategyParams(name="test_l1", e0_bps=2.0, vol_edge_mult=0.5, min_hedge_l1_notional_usd=1_000.0)
    res_l1 = simulate_pair_arb(
        mk_bid=mk_bid,
        mk_ask=mk_ask,
        mk_age=mk_age,
        hd_bid=hd_bid,
        hd_ask=hd_ask,
        hd_bid_sz=hd_bsz,
        hd_ask_sz=hd_asz,
        hd_age=hd_age,
        ref_mid=ref_mid,
        ref_age=ref_age,
        causal=causal,
        mk_trades=mk_trades,
        hd_trades=hd_trades,
        t0_ms=0,
        tick=0.01,
        params=p_l1,
        maker_delay_ms=40,
        hedge_delay_ms=40,
        speedbump_ms=0,
        rtt_ms=40,
        maker_fee_bps=0.0,
        maker_taker_fee_bps=1.0,
        hedge_fee_bps=1.15,
        warmup_steps=50,
        decision_steps=60,
        cross_stablecoin_bps=2.0,
        maker_fr_bps_hr=-0.5,
        hedge_fr_bps_hr=0.5,
    )
    assert res_l1["blocked_l1_thin"] == 1, f"expected thin L1 block at k=60: {res_l1}"
    assert res_l1["fills"] == 1, f"expected fill at k=80: {res_l1}"
    assert res_l1["fx_drag_usd"] > 0.0, f"expected positive FX drag: {res_l1}"
    assert res_l1["print_verified_pnl_usd"] is not None
    assert res_l1["funding_available_legs"] == 2

    reg_map = assign_window_regimes({"w01": 1.0, "w02": 1.5, "w03": 8.0, "w04": 1.4})
    assert reg_map["w03"] == "extreme_crash"
    tail = compute_regime_and_tail_metrics([
        {"window": "w01", "window_num": 1, "regime": "calm", "vol_1s_bps": 1.0, "span_hours": 10.2, "fills": 10, "maker_notional_usd": 10000.0, "quote_hedged_pnl_usd": 5.0, "funding_carry_usd": 0.5},
        {"window": "w02", "window_num": 2, "regime": "normal", "vol_1s_bps": 1.5, "span_hours": 10.2, "fills": 12, "maker_notional_usd": 12000.0, "quote_hedged_pnl_usd": -3.0, "funding_carry_usd": 0.2},
        {"window": "w03", "window_num": 3, "regime": "extreme_crash", "vol_1s_bps": 8.0, "span_hours": 10.2, "fills": 20, "maker_notional_usd": 20000.0, "quote_hedged_pnl_usd": -4.0, "funding_carry_usd": 0.1},
    ])
    assert tail["max_drawdown_usd"] == 7.0
    assert tail["worst_10h_window"]["window"] == "w03"
    print(json.dumps({"self_test": "ok", "fills": res_l1["fills"], "blocked_l1_thin": res_l1["blocked_l1_thin"]}))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lakehouse", type=Path, default=Path(os.path.expanduser("~/aleph-tx-lakehouse")))
    ap.add_argument("--output", type=Path)
    ap.add_argument("--max-window", type=int, default=25)
    ap.add_argument("--self-test", action="store_true")
    args = ap.parse_args()

    if args.self_test:
        run_self_test()
        return 0

    out = evaluate_lakehouse(args.lakehouse, max_window=args.max_window)
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
