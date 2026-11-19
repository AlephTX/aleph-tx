#!/usr/bin/env python3
"""Aggregate recorded-replay `calibration` telemetry into calibrator input (`S10-1`).

Reads one or more `RecordedMakerReport` JSON files produced with
`diagnose_cross_market = true` and `calibration_telemetry = true`, sums the
distance-exposure bins and per-horizon markout histograms, pools realized
variance as `sum(r^2) / sum(seconds)`, and writes the payload accepted by
`calibrate_mm_markout_and_as.calibrate_mm_parameters`.

Research contract: every run carries a required `--split-label` and the SHA-256
of each input so calibrated parameters can be frozen from selection windows
before any holdout window is scored. The kappa fit is endogenous to the
distances the replayed strategy actually quoted; sparse bins are dropped by
`--min-bin-exposure-secs` rather than extrapolated.
"""
import argparse
import hashlib
import json
import math
from pathlib import Path

from calibrate_mm_markout_and_as import calibrate_mm_parameters, fit_exponential_kappa

HORIZON_LABELS = {1_000: "1s", 5_000: "5s", 30_000: "30s", 60_000: "60s"}
ALLOWED_SPLITS = ("selection_dev", "historical_regression", "diagnostic")


def _extract_window_bounds(doc):
    """Extract optional `(market_key, start_ms, end_ms)` from a RecordedMakerReport or wrapper."""
    if not isinstance(doc, dict):
        return None, None, None
    market = doc.get("market") or doc.get("symbol")
    start = doc.get("trade_from_ms") if doc.get("trade_from_ms") is not None else doc.get("start_ms")
    end = doc.get("end_ms")
    for key in ("report", "result", "recorded"):
        sub = doc.get(key)
        if isinstance(sub, dict):
            sub_m, sub_s, sub_e = _extract_window_bounds(sub)
            if market is None:
                market = sub_m
            if start is None:
                start = sub_s
            if end is None:
                end = sub_e
    return (
        str(market) if market is not None else "__default__",
        int(start) if start is not None else None,
        int(end) if end is not None else None,
    )


def validate_non_overlapping_windows(entries):
    """Reject distinct reports whose `[start_ms, end_ms)` intervals overlap for the same market (`S12-3`).

    Each entry is `(path_label, market_key, start_ms, end_ms)`. When `end_ms` is present,
    requires strict half-open disjointness (`prev_end <= next_start`). When only `start_ms`
    is present, rejects identical `start_ms` within the same market.
    """
    by_market = {}
    for label, market, start_ms, end_ms in entries:
        if start_ms is None:
            continue
        if end_ms is not None and end_ms < start_ms:
            raise ValueError(f"invalid window bounds in {label}: start_ms={start_ms} > end_ms={end_ms}")
        by_market.setdefault(market, []).append((start_ms, end_ms, label))
    for market, spans in by_market.items():
        spans.sort(key=lambda item: (item[0], item[1] if item[1] is not None else item[0]))
        for (s1, e1, l1), (s2, e2, l2) in zip(spans, spans[1:]):
            if e1 is not None and s2 < e1:
                raise ValueError(
                    f"overlapping calibration windows for market {market}: "
                    f"{l1} [{s1}, {e1}) overlaps {l2} [{s2}, {e2})"
                )
            if e1 is None and s1 == s2:
                raise ValueError(
                    f"overlapping calibration start_ms={s1} for market {market}: {l1} and {l2}"
                )


def compute_leave_one_window_stability(
    labeled_blocks,
    *,
    min_bin_exposure_secs=60.0,
    min_total_fills=20.0,
):
    """Assess single-window fill concentration and leave-one-window kappa stability (`S12-3`)."""
    full_agg = aggregate_calibration_reports([b for _, b in labeled_blocks])
    admitted_indices = {
        i
        for i, b in enumerate(full_agg["distance_bins"])
        if b["hi_bps"] is not None and b["exposure_secs"] >= min_bin_exposure_secs
    }
    per_window_fills = {}
    total_admitted = 0
    for label, block in labeled_blocks:
        w_fills = sum(
            int(block["distance_bins"][i]["fill_count"]) for i in admitted_indices
        )
        per_window_fills[label] = w_fills
        total_admitted += w_fills
    max_share = (
        max(per_window_fills.values()) / total_admitted
        if total_admitted > 0 and per_window_fills
        else None
    )

    loo = {}
    kappas = []
    if len(labeled_blocks) >= 2:
        for idx, (drop_label, _) in enumerate(labeled_blocks):
            subset = [b for j, (_, b) in enumerate(labeled_blocks) if j != idx]
            sub_agg = aggregate_calibration_reports(subset)
            sub_bins = [
                {
                    "distance_bps": 0.5 * (b["lo_bps"] + float(b["hi_bps"])),
                    "fill_count": b["fill_count"],
                    "exposure_secs": b["exposure_secs"],
                }
                for b in sub_agg["distance_bins"]
                if b["hi_bps"] is not None and b["exposure_secs"] >= min_bin_exposure_secs
            ]
            sub_fills = sum(int(b["fill_count"]) for b in sub_bins)
            try:
                fit = fit_exponential_kappa(sub_bins, min_total_fills=min_total_fills)
                kappas.append(fit["kappa_per_bps"])
                loo[drop_label] = {
                    "state": "fit",
                    "admitted_fills": sub_fills,
                    "exposed_bins": len(sub_bins),
                    "kappa_per_bps": fit["kappa_per_bps"],
                }
            except ValueError as exc:
                loo[drop_label] = {
                    "state": "withheld",
                    "admitted_fills": sub_fills,
                    "exposed_bins": len(sub_bins),
                    "cause": str(exc),
                }
    return {
        "windows_total": len(labeled_blocks),
        "total_admitted_fills": total_admitted,
        "admitted_fills_by_window": per_window_fills,
        "max_single_window_fill_share": max_share,
        "leave_one_window_fit_count": len(kappas),
        "leave_one_window_withheld_count": len(loo) - len(kappas),
        "leave_one_window_kappa_min_per_bps": min(kappas) if kappas else None,
        "leave_one_window_kappa_max_per_bps": max(kappas) if kappas else None,
        "leave_one_window": loo,
    }


def _find_calibration(doc):
    if isinstance(doc, dict):
        if isinstance(doc.get("calibration"), dict) and "distance_bins" in doc["calibration"]:
            return doc["calibration"]
        for key in ("report", "result", "recorded"):
            if key in doc:
                found = _find_calibration(doc[key])
                if found is not None:
                    return found
    return None


def aggregate_calibration_reports(reports):
    """Sum compatible calibration blocks. Raises on incompatible binning."""
    if not reports:
        raise ValueError("no calibration reports supplied")
    first = reports[0]
    model = {k: first.get(k) for k in ("exposure_model", "fill_distance_model")}
    n_bins = len(first["distance_bins"])
    agg_bins = [
        {
            "lo_bps": float(b["lo_bps"]),
            "hi_bps": b["hi_bps"],
            "exposure_secs": 0.0,
            "fill_count": 0,
            "fill_turnover": 0.0,
        }
        for b in first["distance_bins"]
    ]
    hist = {}
    sum_sq = 0.0
    sigma_secs = 0.0
    sigma_samples = 0
    unpriced = 0
    for rep in reports:
        if any(rep.get(k) != v for k, v in model.items()):
            raise ValueError("incompatible resting-exposure or fill-distance models")
        if (
            len(rep["distance_bins"]) != n_bins
            or float(rep["distance_bin_bps"]) != float(first["distance_bin_bps"])
            or float(rep["markout_bin_bps"]) != float(first["markout_bin_bps"])
            or float(rep["markout_min_bps"]) != float(first["markout_min_bps"])
        ):
            raise ValueError("incompatible calibration binning across reports")
        for agg, b in zip(agg_bins, rep["distance_bins"]):
            if (float(b["lo_bps"]), b["hi_bps"]) != (agg["lo_bps"], agg["hi_bps"]):
                raise ValueError("incompatible distance bin edges across reports")
            agg["exposure_secs"] += float(b["exposure_secs"])
            agg["fill_count"] += int(b["fill_count"])
            agg["fill_turnover"] += float(b["fill_turnover"])
        for h in rep["markout_histograms"]:
            label = HORIZON_LABELS.get(int(h["horizon_ms"]))
            if label is None:
                raise ValueError(f"unsupported markout horizon {h['horizon_ms']}")
            slot = hist.setdefault(
                label,
                {
                    "bin_bps": float(rep["markout_bin_bps"]),
                    "min_bps": float(rep["markout_min_bps"]),
                    "counts": [0] * len(h["counts"]),
                    "underflow": 0,
                    "overflow": 0,
                },
            )
            if len(slot["counts"]) != len(h["counts"]):
                raise ValueError("incompatible markout histogram width")
            slot["counts"] = [a + int(c) for a, c in zip(slot["counts"], h["counts"])]
            slot["underflow"] += int(h["underflow"])
            slot["overflow"] += int(h["overflow"])
        sum_sq += float(rep["sum_sq_log_return"])
        sigma_secs += float(rep["sigma_observed_secs"])
        sigma_samples += int(rep["sigma_samples"])
        unpriced += int(rep["unpriced_fills"])
    sigma = math.sqrt(sum_sq / sigma_secs) if sigma_secs > 0 else None
    return {
        **model,
        "distance_bins": agg_bins,
        "markout_histograms_by_horizon": hist,
        "sigma_per_sqrt_sec": sigma,
        "sigma_observed_secs": sigma_secs,
        "sigma_samples": sigma_samples,
        "unpriced_fills": unpriced,
    }


def build_calibrator_payload(agg, *, min_bin_exposure_secs=60.0, **params):
    """Map aggregated telemetry to `calibrate_mm_parameters` keyword arguments."""
    depth_bins = []
    dropped = 0
    for b in agg["distance_bins"]:
        if b["hi_bps"] is None:
            dropped += 1  # open-ended overflow bin has no distance
            continue
        if b["exposure_secs"] < min_bin_exposure_secs:
            dropped += 1
            continue
        depth_bins.append(
            {
                "distance_bps": 0.5 * (b["lo_bps"] + float(b["hi_bps"])),
                "fill_count": b["fill_count"],
                "exposure_secs": b["exposure_secs"],
            }
        )
    if agg["sigma_per_sqrt_sec"] is None:
        raise ValueError("no realized variance observed; cannot derive gamma")
    payload = {
        "depth_bins": depth_bins,
        "markout_histograms_by_horizon": agg["markout_histograms_by_horizon"],
        "sigma_per_sqrt_sec": agg["sigma_per_sqrt_sec"],
    }
    payload.update(params)
    return payload, dropped


def strategy_overlay(calibration):
    """Strategy keys to merge into a frozen `RecordedMakerConfig.strategy` arm.

    Only calibrated keys are emitted; `markout_reserve` must already exist in
    the base config (its `max_widen_bps`, `close_cost_bps`, and sample age stay
    frozen there), and `local_toxicity.enable_local_markout` must be enabled
    for Core-only replays.
    """
    rec = calibration["recommended_config"]
    return {
        "as_kappa": rec["as_kappa"],
        "as_gamma": rec["as_gamma"],
        "as_risk_budget_bps_at_max_pos": rec["as_risk_budget_bps_at_max_pos"],
        "markout_reserve": {"quantile_floor_p": rec["markout_reserve_quantile_floor_p"]},
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", nargs="+", type=Path)
    parser.add_argument("--split-label", required=True, choices=ALLOWED_SPLITS)
    parser.add_argument("--min-bin-exposure-secs", type=float, default=60.0)
    parser.add_argument("--time-horizon-sec", type=float, default=1.0)
    parser.add_argument("--max-pos-for-as", type=float, required=True)
    parser.add_argument("--target-risk-budget-bps", type=float, required=True)
    parser.add_argument("--maker-fee-bps", type=float, default=0.0)
    parser.add_argument("--min-profit-bps", type=float, default=1.0)
    parser.add_argument("--quantile-p", type=float, default=0.5)
    parser.add_argument("--min-markout-samples", type=int, default=20)
    parser.add_argument("--min-total-fills", type=float, default=20.0)
    parser.add_argument(
        "--allow-overlapping-windows",
        action="store_true",
        help="allow distinct input files with overlapping [start_ms, end_ms) windows",
    )
    parser.add_argument(
        "--max-single-window-fill-share",
        type=float,
        default=None,
        help="optional fail-closed cap (0, 1] on the largest single-window fill share",
    )
    parser.add_argument("--calibrate", action="store_true", help="also run the calibrator")
    parser.add_argument("--output", type=Path, default=None)
    args = parser.parse_args()

    blocks = []
    labeled_blocks = []
    window_entries = []
    provenance = []
    seen = {}
    for path in args.reports:
        raw = path.read_bytes()
        digest = hashlib.sha256(raw).hexdigest()
        if digest in seen:
            raise SystemExit(f"duplicate calibration report: {path} matches {seen[digest]}")
        seen[digest] = path
        doc = json.loads(raw)
        block = _find_calibration(doc)
        if block is None:
            raise SystemExit(f"{path}: no calibration block (enable calibration_telemetry)")
        market, start_ms, end_ms = _extract_window_bounds(doc)
        blocks.append(block)
        labeled_blocks.append((str(path), block))
        window_entries.append((str(path), market, start_ms, end_ms))
        prov = {"path": str(path), "sha256": digest}
        if start_ms is not None:
            prov["start_ms"] = start_ms
        if end_ms is not None:
            prov["end_ms"] = end_ms
        if market != "__default__":
            prov["market"] = market
        provenance.append(prov)
    if not args.allow_overlapping_windows:
        try:
            validate_non_overlapping_windows(window_entries)
        except ValueError as exc:
            raise SystemExit(str(exc))
    agg = aggregate_calibration_reports(blocks)
    stability = compute_leave_one_window_stability(
        labeled_blocks,
        min_bin_exposure_secs=args.min_bin_exposure_secs,
        min_total_fills=args.min_total_fills,
    )
    if args.max_single_window_fill_share is not None:
        cap = float(args.max_single_window_fill_share)
        if not (0.0 < cap <= 1.0):
            raise SystemExit("max-single-window-fill-share must be in (0, 1]")
        share = stability["max_single_window_fill_share"]
        if share is not None and share > cap:
            raise SystemExit(
                f"single-window fill concentration {share:.3f} exceeds cap {cap:.3f}"
            )
    payload, dropped = build_calibrator_payload(
        agg,
        min_bin_exposure_secs=args.min_bin_exposure_secs,
        time_horizon_sec=args.time_horizon_sec,
        max_pos_for_as=args.max_pos_for_as,
        target_risk_budget_bps_at_max_pos=args.target_risk_budget_bps,
        maker_fee_bps=args.maker_fee_bps,
        min_profit_bps=args.min_profit_bps,
        quantile_p=args.quantile_p,
        min_markout_samples=args.min_markout_samples,
        min_total_fills=args.min_total_fills,
    )
    out = {
        "schema_version": 1,
        "split_label": args.split_label,
        "inputs": provenance,
        "aggregate": {k: agg[k] for k in ("exposure_model", "fill_distance_model", "sigma_observed_secs", "sigma_samples", "unpriced_fills")},
        "dropped_distance_bins": dropped,
        "stability": stability,
        "calibrator_payload": payload,
    }
    if args.calibrate:
        result = calibrate_mm_parameters(**payload)
        out["calibration"] = result
        out["strategy_overlay"] = strategy_overlay(result)
    text = json.dumps(out, indent=2, sort_keys=True)
    if args.output:
        args.output.write_text(text + "\n")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
