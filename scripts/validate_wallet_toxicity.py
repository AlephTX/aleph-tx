#!/usr/bin/env python3
"""Causal statistical gate validator for Scenario 1 wallet toxicity scoring.

Implements volume-weighted aggregation across strictly non-overlapping 10-day
(`864_000_000 ms`) window splits to verify out-of-sample adverse-selection persistence
without leakage, `bool` coercion, or TWAP sub-account false positives.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import math
from pathlib import Path
import random
import sys
from typing import Any, Iterable, Sequence

DEFAULT_IS_WINDOW_MS = 864_000_000  # 10 days * 86_400_000 ms/day
DEFAULT_MIN_SPEARMAN_RHO = 0.30
DEFAULT_MIN_OOS_D10_MINUS_D5_BPS = 0.80
DEFAULT_MIN_OVERLAP_WALLETS = 10


def require_non_neg_int(val: Any, name: str) -> int:
    if isinstance(val, bool) or not isinstance(val, int):
        raise ValueError(f"{name} must be an integer, got {type(val).__name__}")
    if val < 0:
        raise ValueError(f"{name} must be >= 0, got {val}")
    return val


def require_finite_float(val: Any, name: str) -> float:
    if isinstance(val, bool) or not isinstance(val, (int, float)):
        raise ValueError(f"{name} must be a real number, got {type(val).__name__}")
    out = float(val)
    if not math.isfinite(out):
        raise ValueError(f"{name} must be finite, got {val}")
    return out


def require_positive_float(val: Any, name: str) -> float:
    out = require_finite_float(val, name)
    if out <= 0.0:
        raise ValueError(f"{name} must be > 0, got {out}")
    return out


@dataclass(frozen=True)
class WalletMarkoutSample:
    """Single historical fill adverse-selection observation (maker perspective)."""

    wallet_key: str
    ts_ms: int
    adverse_selection_bps: float
    is_twap_sub: bool = False
    notional_usd: float = 1.0

    def __post_init__(self) -> None:
        if not isinstance(self.wallet_key, str) or not self.wallet_key.strip():
            raise ValueError("wallet_key must be a non-empty string")
        require_non_neg_int(self.ts_ms, "ts_ms")
        require_finite_float(self.adverse_selection_bps, "adverse_selection_bps")
        if not isinstance(self.is_twap_sub, bool):
            raise ValueError("is_twap_sub must be bool")
        require_positive_float(self.notional_usd, "notional_usd")


def rank_average_ties(values: Sequence[float]) -> list[float]:
    """Return 1-based fractional ranks with average tie breaking."""
    n = len(values)
    if n == 0:
        return []
    indexed = sorted(enumerate(values), key=lambda item: item[1])
    ranks = [0.0] * n
    i = 0
    while i < n:
        j = i + 1
        while j < n and indexed[j][1] == indexed[i][1]:
            j += 1
        avg_rank = 0.5 * ((i + 1) + j)
        for k in range(i, j):
            ranks[indexed[k][0]] = avg_rank
        i = j
    return ranks


def spearman_rho(xs: Sequence[float], ys: Sequence[float]) -> float | None:
    """Compute Spearman rank correlation, returning None if undefined."""
    if len(xs) != len(ys):
        raise ValueError("xs and ys must have equal length")
    if len(xs) < 2:
        return None
    for v in xs:
        require_finite_float(v, "xs")
    for v in ys:
        require_finite_float(v, "ys")

    rx = rank_average_ties(xs)
    ry = rank_average_ties(ys)
    mean_x = sum(rx) / len(rx)
    mean_y = sum(ry) / len(ry)

    num = sum((a - mean_x) * (b - mean_y) for a, b in zip(rx, ry))
    den_x = sum((a - mean_x) ** 2 for a in rx)
    den_y = sum((b - mean_y) ** 2 for b in ry)
    denom = math.sqrt(den_x * den_y)
    if denom <= 0.0:
        return None
    return max(-1.0, min(1.0, num / denom))


def build_non_overlapping_windows(
    samples: Iterable[WalletMarkoutSample],
    window_ms: int = DEFAULT_IS_WINDOW_MS,
    *,
    exclude_twap_sub: bool = True,
) -> list[tuple[int, int, list[WalletMarkoutSample]]]:
    """Partition filtered samples into strictly non-overlapping `[start, end)` windows."""
    if require_non_neg_int(window_ms, "window_ms") <= 0:
        raise ValueError(f"window_ms must be > 0, got {window_ms}")

    filtered = [s for s in samples if not (exclude_twap_sub and s.is_twap_sub)]
    if not filtered:
        return []

    filtered.sort(key=lambda s: (s.ts_ms, s.wallet_key))
    t0 = filtered[0].ts_ms
    buckets: dict[int, list[WalletMarkoutSample]] = {}
    for sample in filtered:
        idx = (sample.ts_ms - t0) // window_ms
        buckets.setdefault(idx, []).append(sample)

    windows: list[tuple[int, int, list[WalletMarkoutSample]]] = []
    for idx in sorted(buckets):
        start_ms = t0 + idx * window_ms
        end_ms = start_ms + window_ms
        windows.append((start_ms, end_ms, buckets[idx]))
    return windows


def _wallet_means(samples: Iterable[WalletMarkoutSample]) -> dict[str, float]:
    """Compute volume-weighted mean adverse selection (bps) per wallet."""
    weighted_sums: dict[str, float] = {}
    weights: dict[str, float] = {}
    for s in samples:
        w = s.notional_usd
        weighted_sums[s.wallet_key] = weighted_sums.get(s.wallet_key, 0.0) + (
            s.adverse_selection_bps * w
        )
        weights[s.wallet_key] = weights.get(s.wallet_key, 0.0) + w
    return {k: weighted_sums[k] / weights[k] for k in weighted_sums}


def _assign_deciles(scores: dict[str, float]) -> dict[str, int]:
    """Assign 1..10 deciles deterministically using in-sample wallet scores."""
    ordered = sorted(scores.items(), key=lambda kv: (kv[1], kv[0]))
    n = len(ordered)
    if n == 0:
        return {}
    deciles: dict[str, int] = {}
    for idx, (wallet, _) in enumerate(ordered):
        decile = min(10, (idx * 10) // n + 1)
        deciles[wallet] = decile
    return deciles


def evaluate_wallet_toxicity_persistence(
    samples: Iterable[WalletMarkoutSample],
    *,
    window_ms: int = DEFAULT_IS_WINDOW_MS,
    min_spearman_rho: float = DEFAULT_MIN_SPEARMAN_RHO,
    min_oos_d10_minus_d5_bps: float = DEFAULT_MIN_OOS_D10_MINUS_D5_BPS,
    min_overlap_wallets: int = DEFAULT_MIN_OVERLAP_WALLETS,
    exclude_twap_sub: bool = True,
) -> dict[str, object]:
    """Run causal adjacent non-overlapping window validation and return structured metrics."""
    min_rho = require_finite_float(min_spearman_rho, "min_spearman_rho")
    min_spread = require_finite_float(min_oos_d10_minus_d5_bps, "min_oos_d10_minus_d5_bps")
    if require_non_neg_int(min_overlap_wallets, "min_overlap_wallets") < DEFAULT_MIN_OVERLAP_WALLETS:
        raise ValueError(
            f"min_overlap_wallets must be >= {DEFAULT_MIN_OVERLAP_WALLETS} for D5/D10 deciles"
        )

    raw_samples = list(samples)
    twap_excluded_count = sum(1 for s in raw_samples if exclude_twap_sub and s.is_twap_sub)
    windows = build_non_overlapping_windows(
        raw_samples,
        window_ms=window_ms,
        exclude_twap_sub=exclude_twap_sub,
    )

    pair_results: list[dict[str, object]] = []
    rhos: list[float] = []
    spreads: list[float] = []

    skipped_non_adjacent_pairs = 0
    for i in range(len(windows) - 1):
        is_start, is_end, is_rows = windows[i]
        oos_start, oos_end, oos_rows = windows[i + 1]
        if oos_start < is_end:
            raise AssertionError("overlapping windows detected")
        # Enforce temporal adjacency: the OOS window must begin exactly one `window_ms`
        # after the IS window start (i.e. consecutive bucket indices). Because empty
        # buckets are skipped when `windows` is built, two *list-consecutive* entries can
        # straddle an arbitrarily long data gap. Pairing across such a gap would measure
        # "persistence" over a discontinuity and violate the strict adjacent-split contract,
        # so skip (do not silently accept) any non-adjacent pair.
        if oos_start != is_end:
            skipped_non_adjacent_pairs += 1
            continue

        is_scores = _wallet_means(is_rows)
        oos_scores = _wallet_means(oos_rows)
        overlap = sorted(set(is_scores) & set(oos_scores))
        if len(overlap) < min_overlap_wallets:
            continue

        is_overlap_scores = {k: is_scores[k] for k in overlap}
        deciles = _assign_deciles(is_overlap_scores)
        rho = spearman_rho(
            [is_scores[k] for k in overlap],
            [oos_scores[k] for k in overlap],
        )
        d10_vals = [oos_scores[k] for k in overlap if deciles[k] == 10]
        d5_vals = [oos_scores[k] for k in overlap if deciles[k] == 5]
        if rho is None or not d10_vals or not d5_vals:
            continue

        d10_mean = sum(d10_vals) / len(d10_vals)
        d5_mean = sum(d5_vals) / len(d5_vals)
        d10_minus_d5 = d10_mean - d5_mean
        rhos.append(rho)
        spreads.append(d10_minus_d5)
        pair_results.append(
            {
                "pair_index": len(pair_results),
                "is_window_ms": [is_start, is_end],
                "oos_window_ms": [oos_start, oos_end],
                "overlap_wallets": len(overlap),
                "spearman_rho": round(rho, 6),
                "oos_d10_mean_bps": round(d10_mean, 6),
                "oos_d5_mean_bps": round(d5_mean, 6),
                "oos_d10_minus_d5_bps": round(d10_minus_d5, 6),
            }
        )

    mean_rho = (sum(rhos) / len(rhos)) if rhos else None
    mean_spread = (sum(spreads) / len(spreads)) if spreads else None
    passed = (
        len(pair_results) >= 1
        and mean_rho is not None
        and mean_spread is not None
        and mean_rho >= min_rho
        and mean_spread >= min_spread
    )

    return {
        "schema_version": 2,
        "window_ms": window_ms,
        "non_overlapping_windows": len(windows),
        "valid_window_pairs": len(pair_results),
        "skipped_non_adjacent_pairs": skipped_non_adjacent_pairs,
        "twap_sub_excluded": twap_excluded_count,
        "thresholds": {
            "min_spearman_rho": min_rho,
            "min_oos_d10_minus_d5_bps": min_spread,
            "min_overlap_wallets": min_overlap_wallets,
        },
        "mean_spearman_rho": round(mean_rho, 6) if mean_rho is not None else None,
        "mean_oos_d10_minus_d5_bps": round(mean_spread, 6) if mean_spread is not None else None,
        "passed": passed,
        "pairs": pair_results,
    }


def generate_synthetic_cohort(
    *,
    persistent: bool = True,
    num_wallets: int = 50,
    num_windows: int = 3,
    fills_per_wallet: int = 4,
    window_ms: int = DEFAULT_IS_WINDOW_MS,
    seed: int = 42,
) -> list[WalletMarkoutSample]:
    """Generate synthetic wallet cohorts for `--self-test` and regression tests."""
    rng = random.Random(seed)
    base_ts = 1_700_000_000_000
    samples: list[WalletMarkoutSample] = []
    wallet_latents = [(i / max(1, num_wallets - 1)) * 2.4 - 0.8 for i in range(num_wallets)]

    for win in range(num_windows):
        win_start = base_ts + win * window_ms
        for wid in range(num_wallets):
            wallet_key = f"w_{wid:03d}"
            latent = wallet_latents[wid] if persistent else rng.uniform(-0.8, 1.6)
            for k in range(fills_per_wallet):
                ts = win_start + (wid * fills_per_wallet + k + 1) * 1_000
                noise = rng.gauss(0.0, 0.25)
                samples.append(
                    WalletMarkoutSample(
                        wallet_key=wallet_key,
                        ts_ms=ts,
                        adverse_selection_bps=latent + noise,
                        is_twap_sub=False,
                        notional_usd=1_000.0,
                    )
                )
        samples.append(
            WalletMarkoutSample(
                wallet_key="twap_sub_001",
                ts_ms=win_start + 500,
                adverse_selection_bps=5.0,
                is_twap_sub=True,
                notional_usd=5_000.0,
            )
        )
    return samples


def load_samples_from_json(path: Path) -> list[WalletMarkoutSample]:
    if not path.is_file():
        raise ValueError(f"input file does not exist or is not a file: {path}")
    raw_text = path.read_text(encoding="utf-8")
    stripped = raw_text.strip()
    if not stripped:
        return []
    if stripped.startswith("["):
        rows = json.loads(stripped)
        if not isinstance(rows, list):
            raise ValueError("JSON root must be an array")
    else:
        rows = [json.loads(line) for line in stripped.splitlines() if line.strip()]
    out: list[WalletMarkoutSample] = []
    for r in rows:
        if not isinstance(r, dict):
            raise ValueError("each sample row must be a JSON object")
        is_twap = r.get("is_twap_sub", False)
        if not isinstance(is_twap, bool):
            raise ValueError("is_twap_sub must be bool")
        notional = r.get("notional_usd", 1.0)
        out.append(
            WalletMarkoutSample(
                wallet_key=r["wallet_key"],
                ts_ms=require_non_neg_int(r["ts_ms"], "ts_ms"),
                adverse_selection_bps=require_finite_float(
                    r["adverse_selection_bps"], "adverse_selection_bps"
                ),
                is_twap_sub=is_twap,
                notional_usd=require_positive_float(notional, "notional_usd"),
            )
        )
    return out


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, help="Input JSON or JSONL wallet markout samples")
    parser.add_argument("--window-ms", type=int, default=DEFAULT_IS_WINDOW_MS)
    parser.add_argument("--min-spearman-rho", type=float, default=DEFAULT_MIN_SPEARMAN_RHO)
    parser.add_argument(
        "--min-oos-d10-minus-d5-bps",
        type=float,
        default=DEFAULT_MIN_OOS_D10_MINUS_D5_BPS,
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run built-in synthetic persistent-vs-shuffled verification and emit JSON",
    )
    args = parser.parse_args(argv)

    if args.self_test:
        if args.input is not None:
            parser.error("--self-test cannot be combined with --input")
        persistent_samples = generate_synthetic_cohort(persistent=True)
        persistent_report = evaluate_wallet_toxicity_persistence(
            persistent_samples,
            window_ms=args.window_ms,
            min_spearman_rho=args.min_spearman_rho,
            min_oos_d10_minus_d5_bps=args.min_oos_d10_minus_d5_bps,
        )
        shuffled_samples = generate_synthetic_cohort(persistent=False)
        shuffled_report = evaluate_wallet_toxicity_persistence(
            shuffled_samples,
            window_ms=args.window_ms,
            min_spearman_rho=args.min_spearman_rho,
            min_oos_d10_minus_d5_bps=args.min_oos_d10_minus_d5_bps,
        )
        assert persistent_report["passed"] is True, persistent_report
        assert persistent_report["twap_sub_excluded"] == 3, persistent_report
        assert shuffled_report["passed"] is False, shuffled_report
        payload = {
            "self_test": "ok",
            "persistent": persistent_report,
            "shuffled": shuffled_report,
        }
        print(json.dumps(payload, indent=2, allow_nan=False))
        return 0

    if not args.input:
        parser.error("either --input or --self-test is required")

    samples = load_samples_from_json(args.input)
    report = evaluate_wallet_toxicity_persistence(
        samples,
        window_ms=args.window_ms,
        min_spearman_rho=args.min_spearman_rho,
        min_oos_d10_minus_d5_bps=args.min_oos_d10_minus_d5_bps,
    )
    print(json.dumps(report, indent=2, allow_nan=False))
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
