#!/usr/bin/env python3
"""Walk-forward the W19-frozen Track 01 lead-lag arms across every other Tokyo crypto window.

Reads frozen parameters from a `track01_lead_lag_v1` JSON (never re-selects), discovers
all `tokyo-crypto-*-wNN` campaigns on the lakehouse, and replays each frozen arm per
window. Windows after W19 are labelled `forward_holdout`; windows before W19 are
`historical_regression` (out-of-sample for these parameters but older regime). W19 itself
is excluded (selection). Windows still being ingested can be skipped with --exclude.
Aggregates only.
"""

from __future__ import annotations

import argparse
import glob
import json
import math
import re
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

import lakehouse_grid as lg  # noqa: E402
import research_lead_lag_quant as ll  # noqa: E402


def discover_crypto_windows(lakehouse: Path) -> dict[int, str]:
    pat = str(Path(lakehouse) / "parquet/bbo_quotes/collector_region=tokyo/venue=lighter_robinhood/utc_date=*/campaign_id=tokyo-crypto-*")
    out: dict[int, str] = {}
    for d in glob.glob(pat):
        cid = d.rsplit("campaign_id=", 1)[1]
        m = re.search(r"-w(\d+)$", cid)
        if m:
            out[int(m.group(1))] = cid
    return dict(sorted(out.items()))


def summarize(values: list[float]) -> dict:
    if not values:
        return {"windows": 0}
    n = len(values)
    mean = sum(values) / n
    sd = math.sqrt(sum((v - mean) ** 2 for v in values) / (n - 1)) if n > 1 else 0.0
    return {
        "windows": n,
        "positive_windows": sum(v > 0 for v in values),
        "mean_of_window_means_bps": round(mean, 4),
        "t_across_windows": round(mean / (sd / math.sqrt(n)), 2) if sd > 0 else None,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--lakehouse", type=Path, default=Path.home() / "aleph-tx-lakehouse")
    ap.add_argument("--frozen", type=Path, required=True)
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--pairs", default="lighter ETH,lighter SOL,lighter BTC,lighter_rh ETH,lighter_rh BTC")
    ap.add_argument("--exclude", default="19,25", help="window numbers to skip")
    args = ap.parse_args()
    frozen_doc = json.loads(args.frozen.read_text())
    wanted = [s.strip() for s in args.pairs.split(",") if s.strip()]
    excl = {int(x) for x in args.exclude.split(",") if x.strip()}
    windows = {w: c for w, c in discover_crypto_windows(args.lakehouse).items() if w not in excl}
    con = lg.connect()
    out = {"schema": "track01_lead_lag_walkforward_v1", "frozen_source": args.frozen.name,
           "excluded_windows": sorted(excl), "pairs": []}
    for rec in frozen_doc["pairs"]:
        label = rec["label"]
        if rec.get("status") != "evaluated" or not any(label.startswith(w) for w in wanted):
            continue
        pair = next(p for p in ll.PAIRS if p[5] == label)
        fz = rec["frozen_params"]
        rows = []
        for w, cid in windows.items():
            lg.WINDOWS[f"w{w:02d}"] = {"crypto": cid, "rwa": "", "split": ""}
            data = ll.load_pair(con, args.lakehouse, f"w{w:02d}", pair)
            if data is None:
                rows.append({"window": w, "status": "missing"})
                continue
            r = ll.run_variant(data, pair, fz["theta_bps"], fz["hold_steps"], fz["tau_s"], with_prints=True,
                                exit_on_revert=bool(fz.get("exit_on_revert", False)))
            r250 = ll.run_variant(data, pair, fz["theta_bps"], fz["hold_steps"], fz["tau_s"],
                                  extra_delay_ms=250, with_prints=True,
                                  exit_on_revert=bool(fz.get("exit_on_revert", False)))
            rows.append({
                "window": w,
                "label": "posthoc_w25" if w == 25 else ("forward_holdout" if w > 19 else "historical_regression"),
                "continuous_10h": data["continuous_10h"],
                "trades": r["trades"],
                "stale_entry_skips": r.get("stale_entry_skips", 0),
                "deferred_exits": r.get("deferred_exits", 0),
                "unsettled_exits": r.get("unsettled_exits", 0),
                "net_bps_mean": r["net_bps_mean"],
                "net_bps_total": r["net_bps_total"],
                "print_verified_net_bps_mean": r.get("print_verified_net_bps_mean"),
                "print_verified_trades": r.get("print_verified_trades"),
                "plus250_net_bps_mean": r250["net_bps_mean"],
                "plus250_print_verified_net_bps_mean": r250.get("print_verified_net_bps_mean"),
            })
            del data
        ok = [x for x in rows if x.get("trades", 0) >= 5]
        w25_row = next((x for x in rows if x.get("window") == 25 and "trades" in x), None)
        w25_posthoc_print = None
        if w25_row is not None:
            w25_posthoc_print = {
                "window": 25,
                "trades": w25_row["trades"],
                "print_verified_trades": w25_row.get("print_verified_trades"),
                "print_verified_net_bps_mean": w25_row.get("print_verified_net_bps_mean"),
                "net_bps_mean": w25_row["net_bps_mean"],
                "plus250_print_verified_net_bps_mean": w25_row.get("plus250_print_verified_net_bps_mean"),
                "stale_entry_skips": w25_row.get("stale_entry_skips", 0),
                "deferred_exits": w25_row.get("deferred_exits", 0),
                "unsettled_exits": w25_row.get("unsettled_exits", 0),
                "note": "posthoc_diagnostic_only_not_preregistered_forward",
            }
        out["pairs"].append({
            "label": label,
            "frozen_params": fz,
            "windows": rows,
            "summary_quote": summarize([x["net_bps_mean"] for x in ok]),
            "summary_print_verified": summarize([x["print_verified_net_bps_mean"] for x in ok
                                                 if x["print_verified_net_bps_mean"] is not None]),
            "summary_plus250_print": summarize([x["plus250_print_verified_net_bps_mean"] for x in ok
                                                if x["plus250_print_verified_net_bps_mean"] is not None]),
            "w20_w24_forward_only_print": summarize([x["print_verified_net_bps_mean"] for x in ok
                                                     if 20 <= x["window"] <= 24 and x["print_verified_net_bps_mean"] is not None]),
            "forward_only_print": summarize([x["print_verified_net_bps_mean"] for x in ok
                                             if x["window"] > 19 and x["print_verified_net_bps_mean"] is not None]),
            "w25_posthoc_print": w25_posthoc_print,
        })
        print(json.dumps({"pair": label, **out["pairs"][-1]["summary_print_verified"]}), flush=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(out, indent=2, allow_nan=False) + "\n", encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
