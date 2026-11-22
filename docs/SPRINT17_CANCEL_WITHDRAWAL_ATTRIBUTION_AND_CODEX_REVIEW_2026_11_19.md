# Sprint 17: Withdrawal-Cause Cancel Attribution & Codex `17:41–19:28 CST` Review (2026-11-19)

> **Status**: Offline diagnostics only. All three live tracks remain **`HOLD`**. No live orders, cancellations, deployments, restarts or remote mutations. The change is attribution-only: quoting, cancel decisions, fills and cash are unchanged, and the extra report fields appear only when `diagnose_cancel_attribution = true`.

---

## 1. Why this sprint

The monthly two-level MM baseline (`t50_iso0`, 14 month-first windows) issues **~500k cancel requests**, while the Sprint 5/6 `ratchet + reentry` configuration on the same 14 windows issued **98,449**. Across the existing study ledgers, the jump lines up with three settings that were added together (Sprint 7 `reentry_depth1` onward):

| Ledger (14 monthly windows) | Cancel requests | Maker fills | Stress cash |
| --- | ---: | ---: | ---: |
| Sprint 5 `ratchet/reentry` | 98,449 | 200 | −$8.216 |
| Sprint 12/13 `t50_iso0` (= current baseline) | 499,956 | 178 | −$0.298 |
| `maker_grid_v2` one level / three levels | 267,461 / 715,331 | 129 / 144 | −$4.381 / −$4.002 |
| `mm_reserve` OFI 3→6 bp (`21`) | 503,221 | 134 | −$4.629 |

Cancel count scales almost linearly with `grid_levels` (≈134k per resting order slot), so the churn is per-order, not per-event. Candidates:

1. **`max_top_book_depth_fraction = 0.01`**: an entry side is zeroed whenever 1% of the local L1 notional falls below the minimum order (`build_quote_target`). Every L1 size flicker, and every new thin touch level, withdraws the whole side and later re-places it.
2. **`local_toxicity` withdraw/latch**: OFI or volatility trips zero a side.
3. Ordinary price and size requotes.

Before this sprint, causes 1 and 2 were both logged as the generic `side_withdrawn`, so the existing `cancel_attribution.by_reason` could not separate them. The Sprint 15 review also showed that `cancel_pending` fills (orders filled while a cancel was in flight) carry almost all of the 1 s adverse selection (e.g. `11`: −$2.898 of −$2.987). We had no way to see *which cancel cause* produced those fills.

## 2. `S17-1`: Code change (attribution only)

| File | Change |
| --- | --- |
| [components.rs](../src/strategy/inventory_neutral_mm/components.rs) | New `CancelReason` variants `DepthCapPriceImproved`, `DepthCapDepleted`, `ToxicityWithdrawn` (JSON keys `depth_cap_price_improved`, `depth_cap_depleted`, `toxicity_withdrawn`). |
| [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) | `calculate_pricing_inputs` records whether each fresh local touch price improved versus the previous fresh touch (`local_touch_improved`). `build_quote_target` labels a side that was quotable before the depth cap and becomes unquotable because of it as `DepthCapPriceImproved` (thin new better-priced level) or `DepthCapDepleted` (same/worse level with shrunken size). A side that was quotable after the depth cap and is zeroed by momentum or toxicity gates is labelled `ToxicityWithdrawn`. Everything else keeps the default `SideWithdrawn`. The new labels are part of `has_safety_cancel`, so deferral logic treats them exactly like the old `SideWithdrawn`. |
| [execution.rs](../src/strategy/inventory_neutral_mm/execution.rs) | Production path now calls `build_side_execution_plan_with_withdrawal_reason(..., reason)`. The label only applies to the zero-target-size withdrawal branch; the reduce-cap branch keeps `SideWithdrawn`. `build_side_execution_plan_full` is kept as a test-only wrapper that passes `SideWithdrawn`. |
| [recorded.rs](../src/strategy/inventory_neutral_mm/replay/recorded.rs) | Each queued order remembers its first attributed cancel reason. When a cancel-pending order fills, `by_reason[reason]` now also accumulates `cancel_pending_fills` and `cancel_pending_turnover_quote`. The global counters are unchanged. |

**Invariants**
- Quote sizes, cancel sets, deferral decisions, matching and cash are identical with or without the change (labels only; `has_safety_cancel` covers the new variants exactly as it covered `SideWithdrawn`).
- With `diagnose_cancel_attribution = false`, the report JSON is unchanged. The existing test `cancel_attribution_preserves_baseline_json_and_decomposes_reasons_and_queue_ages` still checks that enabling attribution leaves every other field byte-identical. It now also checks that per-reason pending fills sum to no more than the global `cancel_pending_fills` and that per-reason turnover is finite and non-negative.
- `utilization_floor_cap_causal_l1_depth_cap_and_cancel_reasons_work` now checks: thin bid L1 without touch improvement → `DepthCapDepleted`; with touch improvement → `DepthCapPriceImproved`; target sizes are identical in both cases; a resting bid in the withdrawn side is cancelled with the propagated label.

## 3. Requested Codex rerun (diagnostic, no promotion)

Rerun **only** the 19 frozen `t50_iso0` wallets on this build with `diagnose_cancel_attribution = true`. Cash, fills and all existing fields should match the original reports byte-for-byte apart from the added `cancel_attribution` block. Report, per cohort:

1. `by_reason.*.requests` (expect `depth_cap_*` to dominate if hypothesis 1 holds).
2. `by_reason.*.cancel_pending_fills` / `cancel_pending_turnover_quote` against the 1 s cancel-pending markout from the existing `cross_market_attribution` (−$2.898 on monthly).

**Decision rule, written before seeing data**: if `depth_cap_*` accounts for ≥50% of requests *and* of cancel-pending turnover, the next ablation is a depth-cap hysteresis (hold the side while 1% of a short rolling-max L1 stays ≥ min size) on the same 19 wallets, scored against the original `t50_iso0` with stress cash, maker turnover and 1 s cancel-pending markout. If not, the depth-cap path is ruled out and attention moves to requote thresholds. W09–W16 stay on the original frozen configs either way.

## 4. Review of Codex `17:41–19:28 CST`

| Item | Verdict |
| --- | --- |
| Timeout taker attribution and passive-exit transfer (57 runs) | **Agree.** The reduce-only cap and 32.5 s passive touch cut timeout taker volume from $9,069 to $5,563 and $1,835, but halved total maker turnover ($29,439 → ~$15,100). Monthly stress cost per $M rose from $10.14 to $111.26 and $134.34. Avoiding the taker leg does not help while exits need queue priority that the same orders lose. Not promoted. |
| `quant_lakehouse.py` retry cleanup (17:50) | **Agree.** The fix removes only the output directory created by the failing invocation and keeps already-published directories protected. This closes the `L1582` note from Sprint 16. |
| Menu upper bound and chronological selection (18:35) | **Agree with the framing.** Even picking the best configuration per window after the fact, 5 of 15 active MM windows stay above $2/M, and the arb upper bound across three active roles is −$0.183. Choosing from earlier dates only gives −$6.585 versus −$0.298 for the original. I will keep separating ex-ante-visible predictors from ex-post bounds in later reviews. |
| Four-asset weighting (46 dates, 276 wallets) | **Agree.** Rank and strength weighting change the sign of the improvement between the two adjacent two-week batches (+$0.57 vs −$0.69 for rank). The 28-day totals (−$0.904 / −$1.020 / −$0.846) do not support changing the frozen forward configs. `cross_sectional.rs` stays frozen. |
| Arb size scaling $50/$100/$150 | **Checked the three active wallets.** W05 ETH scales almost linearly (−$0.0453 / −$0.0922 / −$0.1403). W01 ETH role 1 (−$0.0386 / −$0.0773 / −$0.1533) and W06 BTC role 1 (−$0.0083 / −$0.0166 / −$0.0968) break at $150. In W06 the $150 wallet accepted 7 quotes instead of 5, so it fills at a different time from the smaller sizes. That makes the $150 run a different trade path, not a linear scale-up. The extra loss (−$0.0987 of −$0.1112 in mid drift) is consistent with that. No pre-entry visible signal in the current reports separates these cases; this stays a size-capacity limit, not a fixable defect. |
| W09 transfer / W10 progress | No issue found. Seals are recorded, transfer holds its lock, and E-drive and Drive receipts are still 0/3, so W09 results are correctly withheld. |

## 5. Mac verification gates (`2026-11-19 19:54 CST`, after final edit)

| Gate | Result |
| --- | --- |
| `make fmt-check` (`rustfmt --edition 2024` on the five modified files only) | PASS |
| `make check` | PASS |
| `cargo clippy --locked --all-targets -- -D warnings` | PASS, 0 warnings |
| `cargo clippy --locked --features research-replay --all-targets -- -D warnings` | PASS, 0 warnings |
| `cargo test --locked --quiet` | PASS, `619+5+8`, 6 ignored |
| `make test` (`research-replay` Rust + scripts) | PASS, `678+5+8`, 6 ignored; scripts `203` |
| `cd feeder && go test -race -count=1 ./...` | PASS, 5/5 packages uncached |
| `make test-python` | PASS, `42` |
| `make build` | PASS |
| `make trade MODE=check CONFIG=config.example.toml` | PASS |
| `dashboard/` | Untouched, not re-run |

Test counts are unchanged because the new checks extend two existing tests. File SHA-256 prefixes: `inventory_neutral_mm.rs` `384f21f1c314`, `components.rs` `6c18aec7d46b`, `execution.rs` `632a86b670b5`, `replay/recorded.rs` `e8f87db01de5`, `tests.rs` `4cbc222d509d`.
