# Three-Track Strategy/Engineering Hardening, Lakehouse `bbo` Upgrade & Aleph W25 Regression

**Date**: `2026-11-26` (20:58 CST) · **Operational Mode**: Strictly **`HOLD`** (zero live orders/cancels, zero Tokyo deploys/restarts)

This report documents the parallel engineering/audit completion across all three strategy tracks (`01_quant_profit`, `02_hedge_arbitrage`, `03_incentive_turnover`), takeover of Codex's Lakehouse `bbo` normalizer + Track 01 execution-clock patch, resolution of all 4 audit items from Gemini Round 122, and untouched **W25** real-data regressions on `aleph`.

---

## 1. Engineering & Audit Upgrades Landed

1. **Lakehouse `bbo` Normalizer (`scripts/quant_lakehouse.py`, Codex Takeover)**:
   - Added `_parse_bbo_side` and `channel == "bbo"` parsing for `raw_source == "arcus"` (`bestBid`/`bestAsk`, microsecond `timestamp // 1_000`, nullable `globalSequenceId` -> `venue_seq`, `depth_levels=1`, one-sided/zero-size empty handling, coexistence with `l2Orderbook`).
   - Added `chan == "bbo"` parsing for `raw_source == "hyperliquid"` (`hyperliquid`, `trade_xyz`, `entropy`; `data.bbo[0/1]` with nullable side and optional `time`).
   - Verified `14/14` in [test_quant_lakehouse.py](../scripts/test_quant_lakehouse.py) and `481/481` in `make test-research` with zero Bronze/Silver schema conflicts.
   - Note for downstream W26+ consumers: once `bbo` recording is authorized/enabled on Tokyo, `v_bbo_quotes` for `venue='arcus'` will interleave `bbo` (`depth_levels=1`, `venue_seq` populated) and `l2Orderbook` (`venue_seq=None`) in causal `(available_at_ms, sequence)` order.

2. **Rust Strategy Hardening (`src/strategy/basis_maker_hedge.rs` & `src/strategy/lead_lag_taker.rs`)**:
   - Fixed Gemini #122 item #4 in [basis_maker_hedge.rs](../src/strategy/basis_maker_hedge.rs): added `RefCancelShieldConfig.fail_closed_when_unready` (default `false` for backward compatibility; rejects unwarmed `<2` samples, stale `> max_book_age_ms`, or future `ref.ts_ms > now_ms` timestamps on both sides when `true`, while still preserving `allow_reduce_only_through_shield`).
   - Added pure-Rust `ShieldedPairEconomics` (`maker_fee_bps`, `hedge_taker_fee_bps`, `cross_stablecoin_drag_bps`, `expected_hedge_slip_bps`, `cross_stablecoin`, `same_risk_domain`) so execution adapters share one audited economic gate.
   - Hardened `now_ms.saturating_sub(...)` clock checks and added clock-regression/future-receipt tests in [lead_lag_taker.rs](../src/strategy/lead_lag_taker.rs).
   - Verified zero `crate::exchanges::*` coupling and zero `feeder/shm/` or frozen `inventory_neutral_mm.rs` modifications (`ce0930d3...`).

3. **Python Research Audit Fixes (`scripts/research_*.py`, `walkforward_lead_lag.py`, `eval_multivenue_three_tracks.py`, `sim_queue_priority_mm.py`)**:
   - Merged Codex `candidate.patch` into [research_lead_lag_quant.py](../scripts/research_lead_lag_quant.py) (`stale_entry_skips`, `deferred_exits`, `unsettled_exits`, preserving `exit_on_revert`) and propagated execution counters + `posthoc_w25` separation in [walkforward_lead_lag.py](../scripts/walkforward_lead_lag.py).
   - Fixed Gemini #122 item #2 in [research_lead_lag_hybrid.py](../scripts/research_lead_lag_hybrid.py): aligned docstring `L20` with timeout delay semantics, prevented silently dropping entered positions or overlapping `nxt` on stale timeout quotes, and added explicit `core_eth_sol_basket_usd` alongside the 6-arm basket.
   - Fixed Gemini #122 item #3 in [research_shielded_maker_hedge.py](../scripts/research_shielded_maker_hedge.py): enforced `hd_age[j] <= max_age_steps` after the 1,200-step hedge scan (`unhedged_stale_skips`, no partial/unhedged mutation), added `cross_stablecoin_bps` drag sensitivity (`1 bp` / `2 bp`), flagged `shield_is_self_hedge_venue`, and added `--max-window 25` untouched holdout reporting.
   - Added `holdout_w25` (`tokyo-{crypto,rwa,bulk}-*-w25`), midnight-crossing `utc_date + 1 day` parquet globbing, and Arcus `disable_bbo_sweep_fills` sensitivity in [eval_multivenue_three_tracks.py](../scripts/eval_multivenue_three_tracks.py) + [sim_queue_priority_mm.py](../scripts/sim_queue_priority_mm.py).

---

## 2. Real Aleph W25 Regression Results (Strict Track Separation)

All four runs executed read-only on `aleph` (`nice -n 10`, `:memory:` DuckDB, `memory_limit='2GB'`, `threads=2`), keeping W25 strictly separated from W19 selection and W20–W24 holdout.

### 2.1 Track 01 (`01_quant_profit`): Lead-Lag Taker Walk-Forward (`W01–W25` ex `W19`)

Source: [track01_w25_walkforward_20261126.json](../.agent-state/research/track01_w25_walkforward_20261126.json), [track01_hybrid_w25_20261126.json](../.agent-state/research/track01_hybrid_w25_20261126.json).

- **Clock-Freshness Triggering Confirmed in Historical Data**: Codex's freshness rules triggered on real gaps in `W10`, `W11`, `W12`, `W17` (e.g., `lighter BTC` W11 `2 stale_entry` / `8 deferred`; `lighter SOL` W11 `4 stale_entry` / `6 deferred`; `lighter ETH` W11 `2 deferred`), with `0` unsettled exits across all 23 windows and `0` stale skips/deferrals in W25.
- **Taker-Only vs Immediate Passive Exit (`$1,000` L1-depth-capped, unified `window_1000ms_mean` print verifier)**:

| Basket / Arm | W01–W24 (`22` win) USD | W20–W24 Fwd USD | W25 Untouched USD (`$1k` / `$2k`) | Median L1 (`W25`) | Verdict |
| --- | --- | --- | --- | --- | --- |
| **`lighter Core ETH + SOL` (`taker_only`)** | **`+$133.24`** (`16/22` pos, `t=2.29`, DD `\$16.31`) | **`+$12.69`** (`3/5` pos) | **`+$6.91` / `+$14.16`** | `$9,059` / `$1,123` | **Primary Profitable Pool** |
| `lighter ETH <- binance` (`taker_only`) | `+$67.89` (`17/22`, `t=2.20`) | `-0.0375 bp` print | `+$4.55` (`+6.25 bp` print, `17` tr) | `$9,059.4` | Core Winner |
| `lighter SOL <- binance` (`taker_only`) | `+$65.35` (`17/22`, `t=1.83`) | `+0.1872 bp` print | `+$2.37` (`+0.42 bp` print, `73` tr) | `$1,122.6` | Core Winner |
| `lighter BTC <- binance` (`taker_only`) | `-$189.68` (`7/22`, `t=-2.91`) | `-0.6580 bp` print (`0/5`) | `+$0.74` (`+0.24 bp` print, `213` tr) | `$5,033.9` | **Exclude** |
| `lighter_rh` ETH / SOL / BTC (`taker_only`) | `-$6.86` / `-$12.28` / `-$6.55` | shallow L1 drag | `-$0.05` (`-3.02 bp`) / `-$0.37` (`-2.53 bp`) / `+$1.11` (`-2.12 bp`) | `$42.9–$113.7` | L1 too shallow |
| Full 6-Arm Portfolio (unfiltered) | `-$82.20` (`8/22`, `t=-0.88`) | `-$110.42` (`0/5`) | `+$8.34` (`$17.25` at `$2k`) | Mixed | Rejected without filter |

- *Print-Estimator Alignment Note (resolving Gemini Round 124)*: [research_lead_lag_hybrid.py](../scripts/research_lead_lag_hybrid.py) was updated from `first_print_px` to `mean_print_px` (`window_1000ms_mean`), eliminating optimism on thin fills (`lighter_rh ETH` W25 now matches [walkforward_lead_lag.py](../scripts/walkforward_lead_lag.py) identically at `-3.0197 bp`, `lighter ETH` at `+6.2468 bp`, `lighter SOL` at `+0.4201 bp`, `lighter BTC` at `+0.2365 bp`, and `lighter_rh BTC` at `-2.1227 bp`).

### 2.2 Track 02 (`02_hedge_arbitrage`): Speed-Bump Maker + Pre-Fill Cancel Shield (`W15–W25` ex `W19`)

Source: [track02_w25_shielded_regression_20261126.json](../.agent-state/research/track02_w25_shielded_regression_20261126.json).

- **Critical Empirical Finding — `cross_stablecoin_bps` Sensitivity on Cohort A (`USDG/USDC`)**:
  - At `0 bp` conversion friction, Cohort A (`13` pairs) produces `+$452.63` quote / `+$160.86` print in `W20–W24` and **`+$47.96` quote / `+$17.89` print** in `W25 untouched` (`1,213` fills, `217` blocked toxic, `0` unhedged skips).
  - Under **`1 bp` (`0.01%`) FX/rebalance drag**, the 13-pair Cohort A total flips sharply negative: `-$97.42` (`W15–W18`), **`-$215.91` (`W20–W24`)**, and **`-$19.51` (`W25`)**! Only high-edge pairs stay positive in both `W20–W24` and `W25` under `1 bp` FX drag (`core>rh ETH`: `+$24.07` / `+$0.49`; `core>rh SOL`: `+$7.27` / `+$1.45`; `rh>core AMZN`: `+$17.50` / `+$0.88`; `rh>core AAPL`: `+$3.51` / `+$1.28`), and RWA Cohort A pairs use `lighter` itself as `ref_venue` (`shield_is_self_hedge_venue=True`).
- **Cohort B Cross-Risk-Domain (`USDC/USDC`, no stablecoin FX drag for `lighter>arcus`)**:
  - **Full 10-pair Cohort B**: `W20–W24` `+$537.80` quote / `-$2.00` print; `W25` **`+$69.62` quote / `-$5.66` print** (`892` fills, `17` blocked, `0` unhedged skips), dragged down by `lighter>arcus BTC` (`-$65.06` W20–24, `-$11.81` W25).
  - **Selected `lighter>arcus` Subset (`ETH`, `QQQ`, `NVDA`)**:

| Pair | W15–W18 (Quote / Print) | W20–W24 (Quote / Print) | W25 Untouched (Quote / Print) | W25 `arcus level_0 2.25 bp` |
| --- | --- | --- | --- | --- |
| `lighter>arcus ETH` | `+$69.43` / `+$47.93` | `+$108.70` / `+$20.49` | `+$21.34` / `-$2.12` (`88` fills) | n/a (crypto `2.30 bp`) |
| `lighter>arcus QQQ` | `+$15.37` / `+$11.44` | `+$18.54` / `+$12.87` | **`+$1.99` / `+$1.36`** (`21` fills) | **`+$0.74`** |
| `lighter>arcus NVDA` | `+$57.56` / `+$37.69` | `+$111.98` / `+$4.66` | `+$9.28` / `-$3.73` (`31` fills) | `+$7.07` (quote) |
| **3-Pair Selected Total** | **`+$142.36` / `+$97.06`** | **`+$239.22` / `+$38.02`** | `+$32.61` / `-$4.49` (`140` fills) | 10-win print **`+$130.59`** |

  - Note: `lighter>arcus SPY` also stayed positive in `W25` (`+$2.04` quote / `+$0.48` print, though `arcus level_0 2.25 bp` was `-$2.78`), while `lighter>arcus SOL` posted `+$10.68` quote / `+$9.87` print on W25 (`-$5.40` in W20–24). Strict asset allowlisting + `ShieldedPairEconomics` fee/FX gating are mandatory.

### 2.3 Track 03 (`03_incentive_turnover`): Multi-Venue W25 `10.25h` Untouched Holdout

Source: [multivenue_three_tracks_holdout_w25_20261126.json](../.agent-state/research/multivenue_three_tracks_holdout_w25_20261126.json).

All listed continuous WS venues ran **`10.25h`** continuously (`is_continuous_10h=True`). Selected highlights (`cost_per_m_usd`):

| Venue & Market | Baseline `A` (`$/M`, Vol) | Best Upgraded Arm `E_v3` / `F_v4` (`$/M`, Vol) | Evidence Mode & Caveats |
| --- | --- | --- | --- |
| `lighter QQQ` | **`-$7.04/M`** (`$765k`) | **`-$10.59/M` (`E`) / `-$12.29/M` (`F`)** (`$707k` / `$700k`) | `continuous_10h_ws_trades` (negative/profitable MM!) |
| `lighter SPY` | **`-$0.26/M`** (`$758k`) | **`-$4.41/M` (`E`) / `-$1.83/M` (`F`)** (`$553k` / `$543k`) | `continuous_10h_ws_trades` |
| `lighter_rh SPY` | `$7.10/M` (`$2.47M`) | **`$5.00/M` (`E`, `$1.29M`)** / `$5.21/M` (`F`, `$1.21M`) | `continuous_10h_ws_trades` (`USDG`) |
| `lighter_rh NVDA` | `$19.22/M` (`$1.46M`) | **`$8.01/M` (`F`, `$944k`)** (-58% cost!) | `continuous_10h_ws_trades` (`USDG`) |
| `lighter_rh ETH` | `$57.38/M` (`$543k`) | **`$26.53/M` (`E`, `$447k`)** / `$28.51/M` (`F`, `$440k`) | `continuous_10h_ws_trades` |
| `arcus QQQ` | `$135.60/M` (`$246k`) | **`$72.11/M` (`E`, `$78.6k`)** | `public_trades` (matches `bbo_sweep_disabled` identically; W25 still 200ms `l2Orderbook` throttled!) |
| `arcus SNDK` | `$277.73/M` (`$102k`) | **`$101.10/M` (`F`, `$41.6k`)** (-64% cost!) | Same 200ms W25 throttle caveat; awaiting authorized `W26+` `bbo` capture |
| `arcus BTC` | `$229.01/M` (`$329k`) | **`$149.06/M` (`F`, `$77.8k`)** | Same 200ms W25 throttle caveat |
| `extended ETH` | `$303.66/M` (`$32.8k`) | **`$67.48/M` (`F`, `$7.8k`)** (-78% cost!) | `continuous_10h_ws_trades` (small capacity) |
| `extended BTC` | `$243.97/M` (`$110k`) | **`$106.49/M` (`E`, `$17.2k`)** | `continuous_10h_ws_trades` |
| `trade_xyz` / `entropy SNDK` | `$424.66` / `$330.12` | `$330.16` (`F`) / `$239.78` (`B`) | `polled_bbo_diagnostic` (pre-`bbo` upgrade) |
| `bulk NVDA` / `SNDK` | `$98.41` / `$121.18` | `$90.90` (`E`) / `$113.79` (`F`) | `book_diff_diagnostic` only (no public trades) |

### 2.4 Track 02 True External Third-Venue RWA Shield Ablation (`--rwa-external-ref`)

Source: [track02_w25_rwa_ext_shield_20261126.json](../.agent-state/research/track02_w25_rwa_ext_shield_20261126.json).

Replacing `shield_is_self_hedge_venue=True` RWA pairs with strictly disjoint external references (`arcus` for `lighter_robinhood <-> lighter`, and `lighter_robinhood` / `lighter` for `-> arcus`, `shield_is_self_hedge_venue=False` across all 23 pairs) shows:
- **Cohort A RWA (`rh>core` shielded by `arcus`)**: Third-venue Arcus shielding preserves or improves print-verified P&L in `W20–W24` on `TSLA` (`+$14.07` vs `+$13.58`), `AAPL` (`+$15.64` vs `+$14.60`), and `AMZN` (`+$52.26` vs `+$52.16`), while `NVDA` softens (`+$4.56` vs `+$7.69`) due to Arcus's W25 `200 ms` L2 throttle (`blocked_toxic=56` vs `27`). Cohort A total across `13` pairs moves slightly up on print-verified P&L (`W15–W18`: `+$132.77`, `W20–W24`: `+$161.69`, `W25`: `+$17.34`), still flipping negative under `1 bp` `cross_stablecoin` drag (`-$96.41` / `-$216.22` / `-$19.91`).
- **Cohort B Selected (`lighter>arcus ETH/QQQ/NVDA` shielded by Binance/`lighter_robinhood`)**: `W15–W18` `+$96.79` print, `W20–W24` `+$38.01` print, `W25` `-$1.57` print (`10`-window sum **`+$133.23`**, quote `+$423.66`); `rh>arcus QQQ` (shielded by `lighter`) improves in `W20–W24` (`+$16.90` vs `+$16.04`).

---

## 3. Decoupled Shadow Adapters, Commit-Split & `W26+` Rollout Readiness

- **Decoupled Shadow/Replay Engine**: [track_shadow.rs](../src/strategy/track_shadow.rs) and read-only binary [track01_track02_shadow.rs](../src/bin/track01_track02_shadow.rs) enforce strict track separation (`DailyLossGuard`, L1 depth cap & symbol allowlist for Track 01; `RefCancelShieldConfig.fail_closed_when_unready=true`, `ShieldedPairEconomics` rejecting same-risk-domain / insufficient net-edge pairs, and `unhedged_stale_halts` fail-closed latching for Track 02), with zero `crate::exchanges::*` imports and zero order-submission clients.
- **Frozen Configurations**: [track01_lead_lag_shadow.toml](../examples/track01_lead_lag_shadow.toml) (`lighter ETH + SOL <- binance`) and [track02_shielded_maker_hedge_shadow.toml](../examples/track02_shielded_maker_hedge_shadow.toml) (`lighter>arcus ETH/QQQ/NVDA`, Arcus `level_0` fees `2.30`/`2.25 bp`).
- **Clean Commit-Split & Authorized Rollout Playbook**: See [COMMIT_SPLIT_AND_ROLLOUT_PLAN_2026_11_26.md](COMMIT_SPLIT_AND_ROLLOUT_PLAN_2026_11_26.md).
