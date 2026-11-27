# Engineering Hardening: Shadow Execution Realism, Defense Scaling, and `OPT-01` Alignment (`2026-11-27`)

> **Operational Mode:** Strictly **`HOLD`** · Offline / read-only shadow, replay, and unit verification only. Zero live order placement, cancellations, deploys, or Tokyo restarts. Zero `crate::exchanges::*` imports in `src/strategy/`; `src/strategy/inventory_neutral_mm.rs`, `Cargo.lock`, `feeder/shm/`, `.env*`, `AGENT_HANDOFF.md`, and all 6 peer files remain untouched.

---

## 1. Safety & Monotonicity Fixes (`OPT-07` & `Track03ShadowSession`)

1. **Monotonic `SideDefense` Sizing ([`src/strategy/defense_fsm.rs`](../src/strategy/defense_fsm.rs)):**
   - Fixed `DefenseState::entry_size_scale` so escalating `Caution` $\rightarrow$ `SideDefense { blocked_side }` preserves `caution_size_scale` on the unblocked entry side (`Self::Normal => 1.0, Self::Caution | Self::SideDefense { .. } => caution_size_scale`), preventing an unblocked side's opening size from expanding back to `1.0`.
   - Regression test: `side_defense_preserves_monotonic_caution_size_scale` (verifies `0.50` unblocked `Sell` sizing under `SideDefense { blocked_side: Buy }` while keeping `ReduceOnlyExit`, `EmergencyHedge`, and `Cancel` permitted).
2. **Consistent Non-Fresh `vol_shock_ratio` Normalization ([`src/strategy/track_shadow.rs`](../src/strategy/track_shadow.rs)):**
   - Updated the `obs != FeedObservation::Fresh` branch in `Track03ShadowSession::on_target_l2_bbo` so `tel.sigma_bps_per_sqrt_s` is divided by `vol_base` (`quiet_regime_max_entry_vol_bps.filter(|&v| v > 0.0).unwrap_or_else(|| lead_cancel_threshold_bps.max(1.0))`), matching the `Fresh` branch identically.

---

## 2. Shadow Execution Realism Upgrades ([`src/strategy/track_shadow.rs`](../src/strategy/track_shadow.rs))

### 2.1 Track 02 `Caution` / `SideDefense` Opening Quote Scaling
- Wired `DefenseFsm::entry_size_scale` into `Track02ShadowSession::on_pair_books_with_l1` and `Track02ShadowSession::on_maker_trade_print` (both fresh-hedge and stale-hedge paths) via zero-allocation `latest_bid_defense_scale` / `latest_ask_defense_scale` state on `Track02PairRuntime`.
- Opening quotes (`!bid_is_reduce` / `!ask_is_reduce`) are scaled by `fsm.entry_size_scale(...)` (and suppressed when `<= 0.0`), while `ReduceOnly` and inventory-reducing legs (`inventory_clips < -1e-9` for `Bid`, `inventory_clips > 1e-9` for `Ask`) remain unthrottled (`1.0`).
- Unit test: `track02_defense_fsm_scales_caution_and_keeps_reducing_unthrottled`.

### 2.2 Track 03 Delayed, Depth-Clamped `ReduceOnlyIoc` Exits & Residual Reporting
- Added opt-in backward-compatible (`#[serde(default)]`) fields to `Track03ArmShadowConfig`:
  - `ioc_exit_delay_ms: u64` (default `0` for legacy immediate-IOC tests; set to `330` for `lighter_robinhood` Standard).
  - `max_l1_depth_fraction: f64` (default `0.50`, validated in `(0.0, 1.0]`).
- Added allocation-free `pending_ioc_exits` queue (`VecDeque` pre-allocated with capacity `16`) and `execute_ioc_exit` / `process_pending_ioc_exits` helpers on `Track03ArmRuntime`:
  - Schedules `ReduceOnlyIoc` fills after `ioc_exit_delay_ms` without duplicating in-flight exit quantity.
  - Executes matured IOC exits ONLY against fresh, valid, limit-crossing opposite touch books (`obs == FeedObservation::Fresh`), clamping fill size to `opposite_l1_sz * max_l1_depth_fraction` and incrementing `depth_clipped_exits` on partial/clipped fills.
  - Leaves unfilled stale or depth-clipped inventory open rather than silently marking it flat.
- Surfaced `depth_clipped_exits: u64` and `terminal_residual_notional_usd: f64` in both `Track03ArmReport` and `Track03ShadowReport` (summing per-arm unsigned `|pos_qty * mark_mid|`).
- Unit test: `track03_delayed_and_depth_clamped_ioc_exit_and_stale_terminal_residual`.

---

## 3. `OPT-01` Profile & Latency Alignment

| File | Alignment Applied |
| --- | --- |
| [`examples/track01_lead_lag_shadow.toml`](../examples/track01_lead_lag_shadow.toml) | Switched `tgt_exchange_id = 8` (`lighter_robinhood` USDG verified `0.00 / 0.00 bps` matching `taker_fee_bps = 0.0` and `taker_delay_ms = 330`), enabled `[defense]`, and documented `OPT-01` fee divergence vs `lighter` Premium (`0.40 / 2.80 bps`). |
| [`examples/track02_shielded_maker_hedge_shadow.toml`](../examples/track02_shielded_maker_hedge_shadow.toml) | Aligned `maker_exchange_id = 8` (`lighter_robinhood` USDG `0.00 bps` maker) paired with `arcus` (`hedge_exchange_id = 7`) verified `level_0` w/ 5% discount (`4.275 bp` Crypto, `2.1375 bp` RWA — fixing the mislabeled `2.30 bp` level_4 comment), set `cross_stablecoin = true`, `same_risk_domain = false`, `cross_stablecoin_drag_bps = 1.0`, raised `ETH`/`QQQ` `e0_bps` (`8.0`/`5.0` bp) to maintain `>= 0.5 bp` net expected edge, and added `[defense]`. |
| [`examples/track03_lighter_rh_standard_shadow.toml`](../examples/track03_lighter_rh_standard_shadow.toml) | Enabled frozen `G5_microprice_tfi` fields (`microprice_alpha_weight = 0.25`, `tfi_ewma_tau_ms = 2000.0`, `tfi_skew_bps = 0.0`, `tfi_veto_threshold = 0.75`), kept `reduce_only_penny_ticks = 0` (`G7`/`G8` disabled), and set `ioc_exit_delay_ms = 330` and `max_l1_depth_fraction = 0.50`. |
| [`examples/track03_queue_priority_mm_shadow.toml`](../examples/track03_queue_priority_mm_shadow.toml) | Updated header comments with exact `OPT-01` capability & fee provenance (`lighter` Premium `0.40 / 2.80 bps` @ `140 ms` taker / `0 ms` maker-cancel vs `lighter_robinhood` Standard `0.00 / 0.00 bps` @ `200 ms` maker-cancel / `300 ms` taker + ~30 ms RTT). |
| [`src/bin/track01_track02_shadow.rs`](../src/bin/track01_track02_shadow.rs) | Updated `all_four_example_shadow_configs_pass_check` (`defense_enabled == true` for Track 01 & Track 02 TOMLs while keeping all earlier default-off unit tests intact). |
| [`scripts/replay_rwa_volume_farming.py`](../scripts/replay_rwa_volume_farming.py) & [`scripts/test_replay_rwa_volume_farming.py`](../scripts/test_replay_rwa_volume_farming.py) | Updated `VENUE_LATENCY_MS["lighter"]` and `VENUE_LATENCY_MS["lighter_robinhood"]` exit delay from `220` to `330` ms (matching [`src/venues.rs`](../src/venues.rs) `300 ms` taker/non-post-only delay + ~30 ms Tokyo RTT) and added unit assertions. |

---

## 4. Verification Results

All checks ran inside the worktree after the final code/config edits:

| Check | Result |
| --- | --- |
| `make fmt-check` | Pass (`cargo fmt --check`, `gofmt -l .` clean) |
| `make check` | Pass (`cargo clippy --locked --features research-replay --lib --bins -- -D warnings`, `go vet ./...`) |
| `cargo test --locked --features research-replay --lib --bins` | Pass (`lib`: `1159 passed; 0 failed; 6 ignored`; `bins`: `21 + 9 + 11 + 0 + 3 + 7 + 11` passed, `0 failed`) |
| `python3 -S -m unittest -v scripts/test_replay_rwa_volume_farming.py scripts/test_eval_volume_farming_cost.py` | Pass (`17` tests `OK`) |
| `python3 -S scripts/eval_volume_farming_cost.py --fee-evidence ... --bookdiff ...` | Pass (`arcus_ant_verified`: `0/1.064`, `arcus_rwa_verified`: `0/2.1375`, `arcus_crypto_verified`: `1.425/4.275`, `lighter`: `0.4/2.8`, `lighter_robinhood`: `0/0`) |
| `track01_track02_shadow --track 1 --config examples/track01_lead_lag_shadow.toml --check` | Pass (`"Configuration OK"`, `arms=2`, `defense_enabled=true`) |
| `track01_track02_shadow --track 2 --config examples/track02_shielded_maker_hedge_shadow.toml --check` | Pass (`"Configuration OK"`, `pairs=3`, `defense_enabled=true`) |
| `track01_track02_shadow --track 3 --config examples/track03_queue_priority_mm_shadow.toml --check` | Pass (`"Configuration OK"`, `arms=1`, `defense_enabled=false`) |
| `track01_track02_shadow --track 3 --config examples/track03_lighter_rh_standard_shadow.toml --check` | Pass (`"Configuration OK"`, `arms=1`, `defense_enabled=true`) |
