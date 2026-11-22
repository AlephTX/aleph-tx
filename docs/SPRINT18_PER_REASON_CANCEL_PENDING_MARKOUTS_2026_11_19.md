# Sprint 18: Per-Reason Cancel-Pending Markouts & L1-Unavailable Label (2026-11-19)

> **Status**: Offline diagnostics only. All three live tracks remain **`HOLD`**. No live orders, cancellations, deployments, restarts or remote mutations. Quoting, cancel decisions, matching and cash are unchanged. The new fields appear only inside `cancel_attribution`, which is present only when `diagnose_cancel_attribution = true`.

---

## 1. What Codex found (20:31 CST)

Codex rebuilt Sprint 17 on isolated Linux (112 verified source files, 5 matching the delivered hashes) and replayed the 19 frozen `t50_iso0` wallets twice. All 19 diagnostics-off outputs matched the existing reports byte-for-byte. All 19 diagnostics-on outputs also matched once the new `cancel_attribution` block was removed. [Codex report](../.agent-state/research/GEMINI17_REPLAY_AND_HOLDING_CLOCKS_2026_11_19.md)

Pre-registered rule from Sprint 17: run a depth-cap hysteresis ablation only if `depth_cap_*` accounts for **at least half of both** cancel requests and cancel-pending turnover.

| Cohort | Depth-cap share of requests | Depth-cap share of cancel-pending turnover | Rule met |
| --- | ---: | ---: | --- |
| Month-first (14) | 337,891 / 499,956 (67.6%) | $2,244.68 / $13,469.07 (16.7%) | No |
| Independent July (2) | 64.6% | 100% (2 fills) | Yes, but n = 2 |
| Taiwan (2) | 71.5% | 40.8% | No |
| Tokyo W07 (1) | 78.8% | 32.5% | No |
| All (19) | 68.7% | 20.4% | **No** |

**Conclusion**: the depth cap causes most of the cancel volume, but not the toxic cancel-pending fills. Per the pre-registered rule, **the depth-cap hysteresis ablation is not run**. The cancel count by itself is not the loss driver.

Two new leads from the same data:

1. **`bound_breach`**: on month-first windows, only 242 requests but 62 cancel-pending fills and $6,345 of turnover (47.1%). Normal price/size requotes (312 and 1,124 requests) have zero cancel-pending fills. Across all 19 wallets, `adverse_retreat` (34 fills, $2,737) and `risk_or_spread_cancel_all` (12 fills, $2,717) are the next largest.
2. **Missing piece**: the 1 s markout (−$2.898 on month-first cancel-pending fills) was only available in aggregate, so a turnover share could not be read as a loss share.

Codex also showed a labelling gap: a missing or stale local L1 zeroed the entry side and was labelled `depth_cap_depleted`, the same as a real same-level depth drop.

## 2. `S18` changes (diagnostics only)

| ID | File | Change |
| --- | --- | --- |
| `S18-1` | [recorded.rs](../src/strategy/inventory_neutral_mm/replay/recorded.rs), [cancel_timing_tests.rs](../src/strategy/inventory_neutral_mm/replay/recorded/cancel_timing_tests.rs) | Each pending markout now carries the order's first attributed cancel reason. When a cancel-pending fill is marked at 1 s / 5 s / 30 s, the same mid and fresh-publication rule as the global `maker_markouts` is used to add it to `by_reason[reason].cancel_pending_markouts[h]` (`horizon_ms`, `marked_fills`, `marked_turnover`, `markout_quote`). Fills without an attributed reason are not included, so per-reason fills are at most the global count. |
| `S18-2` | [components.rs](../src/strategy/inventory_neutral_mm/components.rs), [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) | New label `depth_cap_l1_unavailable` when the depth cap zeroes a side because no fresh local L1 is available. `depth_cap_depleted` now means only an observed, thinner same/worse touch. The new label is part of `has_safety_cancel`, so behaviour is identical to before. |

**Tests (extended, not new):**
- `cancel_attribution_preserves_baseline_json_and_decomposes_reasons_and_queue_ages`: every per-reason markout carries the global horizons. Per-reason marked fills never exceed the global cancel-pending marked fills. When every cancel-pending fill is attributed, the per-reason values sum to the global `cancel_pending_markout_quote`.
- `utilization_floor_cap_causal_l1_depth_cap_and_cancel_reasons_work`: with no local L1, both entry sides are zero and labelled `DepthCapL1Unavailable`.

## 3. Requested Codex rerun (diagnostic only)

Same 19 frozen `t50_iso0` wallets on this build, attribution on. Expected: byte-identical outputs apart from the `cancel_attribution` block. Report per cohort:

1. `by_reason.*.cancel_pending_markouts` at 1 s / 5 s / 30 s, with sums checked against the global `cancel_pending_markout_quote`.
2. `depth_cap_l1_unavailable` vs `depth_cap_depleted` request counts.

**Pre-registered decision rule (before data):**
- If `bound_breach` accounts for **≥ 40% of the month-first 1 s cancel-pending markout loss**, the next step is a bound-breach study. `bound_breach` fires when a resting order falls outside the cancel profit bound (`cancel_min_profit_bps`) after the fair value moves. The study would compare, on the same 19 wallets, the current bound with a pre-emptive variant that widens the bound by the observed one-second drift before the order is placed. Score: stress cash, maker turnover and 1 s cancel-pending markout. No parameter sweep beyond one pre-declared variant.
- If no single reason reaches 40%, record that the cancel-pending loss is spread across reasons and stop the cancel-cause line. Remaining MM effort then goes to fill-time adverse selection independent of cancels.
- W09–W16 stay on their original frozen configs either way.

## 4. Review of Codex's other 20:31 findings

| Item | Verdict |
| --- | --- |
| Four-asset holding clock split at 18:00 UTC | **Agree.** Each wallet's two halves sum exactly to the settled 10 h stress cash. In the adjacent two weeks, the strength-weighted reversal loses −$1.740 in the first five hours and −$0.039 in the second; in month-first windows it loses in both halves (−$1.693 / −$1.789). An early exit would need verified exit depth, size and fees at 18:00, and still loses on month-first. Not a promotion candidate. `cross_sectional.rs` stays frozen. |
| Arb $150 phase decomposition | **Agree, and this corrects my Sprint 17 reading.** In all three active wallets the first entry fill happens at the same time for $50 / $100 / $150. So the $150 deviation is not a different entry path. Of the extra −$0.0987 mid-drift loss, −$0.0799 comes from the **equal-size paired** phase: the venue basis moves while both legs are held. W06 BTC alone loses $0.0717 there. Shorter first-hedge time cannot fix this. Larger size simply holds more basis risk for the same time, and the current reports have no pre-entry signal for it. Recorded as a capacity limit. |
| W09 transfer / W10 | No issue. Receipts stay pending and results are correctly withheld until sign-off. |

## 5. Mac verification gates (`2026-11-19 20:41 CST`, after final edit)

| Gate | Result |
| --- | --- |
| `make fmt-check` (rustfmt on the five modified files only) | PASS |
| `make check` | PASS |
| `cargo clippy --locked --all-targets -- -D warnings` / `--features research-replay` | PASS, 0 warnings each |
| `cargo test --locked --quiet` | PASS, `619+5+8`, 6 ignored |
| `make test` (`research-replay` Rust + scripts) | PASS, `678+5+8`, 6 ignored; scripts `203` |
| `cd feeder && go test -race -count=1 ./...` | PASS, 5/5 packages |
| `make test-python` | PASS, `42` |
| `make build`, `make trade MODE=check CONFIG=config.example.toml` | PASS |
| `dashboard/` | Untouched, not re-run |

SHA-256 prefixes: `inventory_neutral_mm.rs` `1e063834daa7`, `components.rs` `dc9244766b4d`, `replay/recorded.rs` `f26f6f19eafb`, `replay/recorded/cancel_timing_tests.rs` `f3f4c8aa74bd`, `tests.rs` `06c6d8bfde0c`. (`execution.rs` unchanged since Sprint 17: `632a86b670b5`.)
