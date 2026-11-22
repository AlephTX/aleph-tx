#!/usr/bin/env python3
"""Offline, receipt-causal short-horizon forecast audit; never submits orders.

One frozen model predicts 1s midpoint returns from touch microprice displacement
and the preceding 1s return. All transforms are fitted on the training partition.
Missing books, stale intervals, late labels and split boundaries remain explicit.
Output contains aggregate public-book statistics, never raw tape or fill records.
"""

import argparse
from collections import Counter, deque
from dataclasses import asdict, dataclass
import hashlib
import json
import math
from pathlib import Path


def sha256(path):
    h = hashlib.sha256()
    with Path(path).open("rb") as f:
        for block in iter(lambda: f.read(4 * 1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def write_once(path, value):
    path = Path(path)
    text = json.dumps(value, indent=2, sort_keys=True, allow_nan=False) + "\n"
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        if path.read_text() != text:
            raise ValueError("existing output differs; use a new version directory")
    else:
        with path.open("x") as f:
            f.write(text)


@dataclass(frozen=True)
class Sample:
    at_ms: int
    label_at_ms: int
    partition: int
    features: tuple
    target_bps: float


def touch(book, receipt_ms):
    """Validate L1 only; downstream analysis must not claim full-depth coverage."""
    source = book["timestamp_ms"]
    if source == 0:
        if book["bids"] or book["asks"]:
            raise ValueError("unknown book timestamp with nonempty depth")
        return None
    if not isinstance(source, int) or source < 0 or source > receipt_ms:
        raise ValueError("future or invalid book source time")
    if not book["bids"] or not book["asks"]:
        return None
    bid, bq = book["bids"][0]
    ask, aq = book["asks"][0]
    if not all(math.isfinite(v) and v > 0 for v in (bid, ask, bq, aq)) or bid >= ask:
        raise ValueError("invalid touch prices or sizes")
    mid = (bid + ask) / 2
    # Opposite-size weighting; same-size VWAP has the opposite imbalance sign.
    micro = (ask * bq + bid * aq) / (bq + aq)
    return source, mid, (micro / mid - 1) * 10000


def extract(events, start_ms, end_ms, *, split_ms=None, delay_ms=0,
            max_age_ms=1000, max_label_lateness_ms=1000):
    """Sample ASOF features on a 1s grid and mature labels on later book receipts.

    Equal-time books are available before that time's grid decision. Labeling uses
    the first eligible book in tape order, just as the replay markout boundary does.
    A gap shorter than the grid spacing still invalidates pending labels. A fresh
    label must have source >= due, and both source and receipt inside its partition.
    """
    if not (0 <= delay_ms and max_age_ms > 0 and max_label_lateness_ms >= 0):
        raise ValueError("invalid clock policy")
    if end_ms <= start_ms or (end_ms - start_ms) % 1000:
        raise ValueError("invalid grid window")
    if split_ms is not None and (not start_ms < split_ms < end_ms
                                 or (split_ms - start_ms) % 1000):
        raise ValueError("invalid partition boundary")
    edges = [start_ms, end_ms] if split_ms is None else [start_ms, split_ms, end_ms]
    counts = [Counter() for _ in edges[:-1]]
    samples, pending = [], deque()
    grid = start_ms - 1000  # Feature-only warm-up; never a training target.
    book = None
    previous = None
    last_receipt = None
    last_book_source = None
    fresh_epoch = False

    def partition(at):
        return 0 if split_ms is None or at < split_ms else 1

    def invalidate():
        nonlocal previous, fresh_epoch
        previous = None
        fresh_epoch = False
        while pending:
            counts[pending.popleft()[3]]["label_gap"] += 1

    def emit(at):
        nonlocal previous, fresh_epoch
        p = partition(at)
        if at >= start_ms:
            counts[p]["grid_points"] += 1
        if book is None or not 0 <= at - book[0] <= max_age_ms:
            if at >= start_ms:
                counts[p]["feature_book_unavailable"] += 1
            invalidate()
            return
        _, mid, micro = book
        momentum = None if previous is None else (mid / previous - 1) * 10000
        previous = mid
        fresh_epoch = True
        if at < start_ms:
            return
        if momentum is None:
            counts[p]["feature_history_unavailable"] += 1
            return
        due = at + 1000
        if due >= edges[p + 1]:
            counts[p]["split_boundary_withheld"] += 1
            return
        pending.append((at, due, mid, p, (micro, momentum)))
        counts[p]["forecast_candidates"] += 1

    for event in events:
        receipt = event["at_ms"]
        if not isinstance(receipt, int) or receipt < 0:
            raise ValueError("invalid receipt time")
        if last_receipt is not None and receipt < last_receipt:
            raise ValueError("tape receipt times decrease")
        last_receipt = receipt
        if event["kind"] != "book":
            continue
        available = receipt + delay_ms
        while grid < min(available, end_ms):
            emit(grid)
            grid += 1000
        if available >= end_ms:
            break
        current = touch(event["lighter"], receipt)
        # Detect sub-grid stale intervals and explicit missing-book transitions.
        if current is None or (
            fresh_epoch and book is not None and available - book[0] > max_age_ms
        ) or (current is not None and last_book_source is not None
              and current[0] < last_book_source):
            invalidate()
        book = current
        if current is not None:
            last_book_source = current[0]
        if current is None or available - current[0] > max_age_ms:
            invalidate()
            continue
        fresh_epoch = True
        while pending:
            at, due, mid, p, features = pending[0]
            if available >= edges[p + 1]:
                counts[p]["label_after_partition"] += 1
            elif available > due + max_label_lateness_ms:
                counts[p]["label_expired"] += 1
            elif current[0] < due:
                break
            else:
                target = (current[1] / mid - 1) * 10000
                samples.append(Sample(at, available, p, features, target))
                counts[p]["labelled"] += 1
            pending.popleft()
    while grid < end_ms:
        emit(grid)
        grid += 1000
    while pending:
        counts[pending.popleft()[3]]["label_unavailable_at_cutoff"] += 1
    for i, c in enumerate(counts):
        expected = (edges[i + 1] - edges[i]) // 1000
        assert c["grid_points"] == expected
        assert c["grid_points"] == sum(c[k] for k in [
            "feature_book_unavailable", "feature_history_unavailable",
            "split_boundary_withheld", "forecast_candidates"])
        assert c["forecast_candidates"] == sum(c[k] for k in [
            "labelled", "label_gap", "label_after_partition", "label_expired",
            "label_unavailable_at_cutoff"])
    return samples, [dict(c) for c in counts]


@dataclass(frozen=True)
class Model:
    mean_x: tuple
    scale_x: tuple
    mean_y: float
    beta: tuple
    ridge: float
    training_rows: int

    def predict(self, features):
        if len(features) != 2 or not all(math.isfinite(v) for v in features):
            raise ValueError("invalid forecast features")
        return self.mean_y + math.fsum(
            b * (x - m) / s
            for b, x, m, s in zip(self.beta, features, self.mean_x, self.scale_x))


def fit(samples, ridge=0.001):
    if len(samples) < 3 or not math.isfinite(ridge) or ridge <= 0:
        raise ValueError("insufficient training rows or invalid ridge")
    if not all(math.isfinite(v) for r in samples for v in (*r.features, r.target_bps)):
        raise ValueError("nonfinite training row")
    n = len(samples)
    mean_x = tuple(math.fsum(r.features[j] for r in samples) / n for j in range(2))
    scale = tuple(math.sqrt(math.fsum((r.features[j] - mean_x[j]) ** 2
                                     for r in samples) / n) for j in range(2))
    if min(scale) <= 1e-12:
        raise ValueError("unidentified training feature; no fallback model")
    mean_y = math.fsum(r.target_bps for r in samples) / n
    z = [tuple((x - m) / s for x, m, s in zip(r.features, mean_x, scale)) for r in samples]
    a = math.fsum(x[0] ** 2 for x in z) + ridge * n
    b = math.fsum(x[0] * x[1] for x in z)
    d = math.fsum(x[1] ** 2 for x in z) + ridge * n
    c = [math.fsum(x[j] * (r.target_bps - mean_y) for x, r in zip(z, samples))
         for j in range(2)]
    det = a * d - b * b
    if not det > 0 or not math.isfinite(det):
        raise ValueError("singular training system")
    return Model(mean_x, scale, mean_y, ((d*c[0]-b*c[1])/det,
                                       (a*c[1]-b*c[0])/det), ridge, n)


def score(samples, model):
    predictions = {
        "zero": lambda r: 0.0,
        "training_mean": lambda r: model.mean_y,
        "touch_microprice": lambda r: r.features[0],
        "frozen_linear": lambda r: model.predict(r.features),
    }
    out = {"rows": len(samples), "metrics": {}}
    for name, predict in predictions.items():
        errors = [(predict(r), r.target_bps) for r in samples]
        n = len(errors)
        sse = math.fsum((p - y) ** 2 for p, y in errors)
        nonzero = [(p, y) for p, y in errors if y != 0]
        out["metrics"][name] = {
            "sse_bps2": sse, "mse_bps2": sse / n if n else None,
            "mae_bps": math.fsum(abs(p - y) for p, y in errors) / n if n else None,
            "direction_hits": sum(p * y > 0 for p, y in nonzero),
            "nonzero_target_rows": len(nonzero),
            "zero_target_rows": n - len(nonzero),
        }
    return out


def combined(scores):
    n = sum(s["rows"] for s in scores)
    return {"rows": n, "mse_bps2": {
        k: math.fsum(s["metrics"][k]["sse_bps2"] for s in scores) / n if n else None
        for k in ["zero", "training_mean", "touch_microprice", "frozen_linear"]}}


def run(design_path, config_root, output):
    design_path, output = Path(design_path), Path(output)
    design = json.loads(design_path.read_text())
    freeze = json.loads(design_path.with_name("design.freeze.json").read_text())
    if freeze["sha256"] != sha256(design_path):
        raise ValueError("protocol changed after freeze")
    if not (design["grid_ms"] == design["horizon_ms"] == 1000
            and design["training_first_ms"] == 21600000):
        raise ValueError("unsupported frozen model protocol")
    training_keys = design["training_jobs"]
    jobs = sorted(design["jobs"], key=lambda j: j["key"] not in training_keys)
    training, validation, receipts = [], [], []
    rows = []
    model = None
    for j in jobs:
        cfg_path = Path(config_root) / (j["key"] + ".json")
        if sha256(cfg_path) != j["enabled"] or sha256(j["tape"]) != j["tape_sha"]:
            raise ValueError("input fingerprint mismatch")
        cfg = json.loads(cfg_path.read_text())
        start, end = cfg.get("trade_from_ms", cfg["start_ms"]), cfg["end_ms"]
        if end - start != 36000000 or start != j["trade_from_ms"]:
            raise ValueError("requires the registered continuous 10h unit")
        is_train = j["key"] in training_keys
        with Path(j["tape"]).open() as f:
            samples, coverage = extract(map(json.loads, f), start, end,
                split_ms=start + design["training_first_ms"] if is_train else None,
                delay_ms=cfg.get("market_data_delay_ms", 0),
                max_age_ms=design["max_age_ms"],
                max_label_lateness_ms=design["max_label_lateness_ms"])
        receipt = {"key": j["key"], "market": j["market"], "role": j["role"],
                   "tape_sha256": j["tape_sha"], "config_sha256": j["enabled"],
                   "start_ms": start, "end_ms": end,
                   "warmup_ms": j["warmup_ms"], "drain_ms": j["drain_ms"],
                   "market_data_delay_ms": cfg.get("market_data_delay_ms", 0),
                   "coverage": coverage}
        receipts.append(receipt)
        if is_train:
            fit_rows = [s for s in samples if s.partition == 0]
            test_rows = [s for s in samples if s.partition == 1]
            training.extend(fit_rows)
            validation.append((receipt, fit_rows, test_rows))
            if len(validation) == len(training_keys):
                model = fit(training)
                # Freeze trained numbers before validation scoring or later tape reads.
                write_once(output / "model.json", {
                    "schema": "causal_book_forecast_model.v1", "model": asdict(model),
                    "design_sha256": sha256(design_path), "code_sha256": sha256(__file__),
                    "training": [v[0] for v in validation]})
                for rec, train_rows, test_rows in validation:
                    rows.append({**rec, "fit": score(train_rows, model),
                                 "validation": score(test_rows, model)})
        else:
            if model is None:
                raise ValueError("frozen training incomplete")
            rows.append({**receipt, "assessment": score(samples, model)})
        print("Verified forecast unit", len(receipts), "of", len(jobs), flush=True)
    if model is None:
        raise ValueError("no frozen model")
    train_scores = [r["fit"] for r in rows if "fit" in r]
    val_scores = [r["validation"] for r in rows if "validation" in r]
    later = [r for r in rows if r["role"] == "later_historical_selection"]
    aggregate = {"fit": combined(train_scores), "validation": combined(val_scores),
                 "later": combined([r["assessment"] for r in later])}
    def better(s, competitors):
        return s["rows"] > 0 and all(s["mse_bps2"]["frozen_linear"] < s["mse_bps2"][c]
                                     for c in competitors)
    gates = {
        "training_rows": all(s["rows"] >= 18000 for s in train_scores),
        "validation_rows": all(s["rows"] >= 12000 for s in val_scores),
        "training_validation_coverage": all(c.get("labelled", 0) / c["grid_points"] >= .90
                                            for r in rows if "fit" in r for c in r["coverage"]),
        "validation_pooled": better(aggregate["validation"], ["zero", "touch_microprice"]),
        "validation_each_market": all(better(combined([s]), ["zero", "touch_microprice"])
                                      for s in val_scores),
        "later_pooled": better(aggregate["later"], ["zero"]),
    }
    nonempty = [r for r in later if r["assessment"]["rows"]]
    wins = sum(better(combined([r["assessment"]]), ["zero"]) for r in nonempty)
    gates["later_window_breadth"] = bool(nonempty) and wins / len(nonempty) >= .60
    by_market = {m: combined([r["assessment"] for r in later if r["market"] == m])
                 for m in sorted({r["market"] for r in later})}
    gates["later_btc_eth_each"] = all(better(by_market.get(m, {"rows": 0}), ["zero"])
                                      for m in ["BTC", "ETH"])
    write_once(output / "summary.json", {
        "schema": "causal_book_forecast_audit.v1", "design_sha256": sha256(design_path),
        "code_sha256": sha256(__file__), "model_sha256": sha256(output / "model.json"),
        "rows": rows, "aggregates": aggregate, "later_by_market": by_market,
        "later_window_wins": wins, "later_nonempty_windows": len(nonempty), "gates": gates,
        "predictive_gate_passed": all(gates.values()), "economic_promotion": False,
        "new_predictive_models": 1, "new_economic_variants": 0,
        "limitations": ["historical selection, not untouched holdout", "1s books are not independent trades",
                        "no fill conditioning, fees, queues, funding or complete-cycle cash in forecast MSE",
                        "continuous 10h source units; 6h/4h partitions are prediction diagnostics only",
                        "native L1 data only; trade-feed outages are not book availability evidence"]})
    print("Predictive gate", "PASS" if all(gates.values()) else "FAIL", flush=True)


if __name__ == "__main__":
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--design", type=Path, required=True)
    p.add_argument("--config-root", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    args = p.parse_args()
    run(args.design, args.config_root, args.output)
