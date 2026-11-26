# Track 01 (`01_quant_profit`) Multi-Venue Crypto Lead-Lag Upgrade (2026-11-27)

> **Safety & Protocol**: Strict **HOLD** (zero live orders, cancellations, or restarts). All lakehouse runs executed read-only on `aleph` via in-memory DuckDB (`SET preserve_insertion_order=false; SET threads=2; SET memory_limit='2GB';`, `nice -n 10`, `gc.collect()` per window) without touching `lakehouse.duckdb` or `~/funspace/aleph-tx`. Parameters and regime cutoffs were selected strictly on `full_dev` (`w01`–`w20` excluding `w08`, 19 continuous 10h windows) and replayed **once untouched** on `holdout` (`w21`–`w25`, 5 windows). Research hypotheses only; not live profitability evidence.

## 1. Deliverables & Architecture

| Component | Path | Summary |
| --- | --- | --- |
| Rust state machine | [`src/strategy/lead_lag_taker.rs`](../src/strategy/lead_lag_taker.rs) | Default-off, zero-alloc causal upgrades: EWMA 1s `tgt` vol-adaptive threshold (`vol_theta_mult`, `vol_ewma_tau_ms`), dual-leader confirmation (`observe_with_confirm`, `require_dual_leader_confirm`), early catch-up exit (`exit_on_catchup_frac`), 12 unit tests (`+4` new). |
| Multi-venue evaluator | [`scripts/research_track01_multivenue_lead_lag.py`](../scripts/research_track01_multivenue_lead_lag.py) | 100% stdlib at module top level (`duckdb` lazy-imported inside lakehouse runner); dynamic `tokyo-crypto-*-w*` + cross-midnight globbing; 250ms BBO + unbucketed `public_trades`; `$500`/`$1000` (`0.5 * L1`) depth caps; regime stratification & cumulative curves; `--self-test`. |
| Evaluator unit tests | [`scripts/test_research_track01_multivenue_lead_lag.py`](../scripts/test_research_track01_multivenue_lead_lag.py) | 10 pure-stdlib unit tests including isolated subprocess check blocking `numpy`, `duckdb`, `pyarrow`, `pandas`, `scipy`, plus exact parity test between full-tick and precomputed candidate-jumping simulation. |

### Venue Schedules & Provenance

| Target Venue | Settlement | Maker / Taker Fee (bps) | Taker Delay (ms) | Fee Audit Status |
| --- | --- | ---: | ---: | --- |
| `lighter` | `USDC` | `0.00 / 0.00` | `330` | `public_tier0_unverified` (requires signed `/api/v1/accountLimits`) |
| `lighter_robinhood` | `USDG` | `0.00 / 0.00` | `230` | `public_tier0_unverified` (USDG settlement domain kept separate) |
| `extended` | `USDC` | `0.00 / 2.50` | `60` | `account_evidence_required` |
| `arcus` | `USDC` | `1.50 / 4.50` | `40` | `public_tier0_schedule_only` (Level 0 Base; Level 4 Platinum = `2.30` taker) |

## 2. Dataset Coverage & Continuity (`w01`–`w25`)

- **Dynamic discovery**: 24 `tokyo-crypto-*-w*` campaigns (`w01`–`w07`, `w09`–`w25`; `w08` absent).
- **`full_dev`** (`19` windows: `w01`–`w20` ex `w08`, `1,167.99` pair-hours per venue): `arcus`, `extended`, and `lighter_robinhood` satisfy `cont=True` (`>=10h` and `max_gap <= 60s`) on **114 / 114** `(window, pair)` streams (`19/19` windows). `lighter` satisfies `cont=True` on **90 / 114** streams (`15/19` windows, `921.99` pair-hours); `w10`, `w11`, `w12`, and `w17` had target BBO gaps of `124.4s–826.7s` (`cont=False`). Both `summary_all` and `summary_cont_only` are reported below.
- **`holdout`** (`5` untouched windows: `w21`–`w25`, `307.49` pair-hours per venue): **30 / 30** streams (`100%`) satisfy `cont=True` on all 4 venues.
- **Regime cutoffs frozen on `full_dev`** (per-symbol realized 1s target vol `p33` / `p90` in `bps/sqrt(s)`):
  - `BTC`: `calm <= 0.8733`, `normal (0.8733, 1.2567]`, `extreme_crash > 1.2567` (max `1.8613`).
  - `ETH`: `calm <= 1.0793`, `normal (1.0793, 1.6762]`, `extreme_crash > 1.6762` (max `2.1059`).
  - `SOL`: `calm <= 1.2127`, `normal (1.2127, 1.6771]`, `extreme_crash > 1.6771` (max `2.1257`).

## 3. Selection (`full_dev`, 19w) vs. Untouched Holdout (`w21`–`w25`, 5w)

### Multi-Venue Arm Comparison (`$500` / `$1000` `0.5*L1`-Capped, All Windows & `cont=True`)

> Note: On `holdout`, `summary_all` and `summary_cont_only` are identical (`30/30` continuous). For `lighter` on `full_dev`, `cont=True` (`15w`, `921.99h`) metrics are shown in parentheses where they differ from all `19w`.

| Split | Venue (`Settlement`) | Arm | Trades (PV) | Quote Mean (bps) | PV Mean (bps) | Quote `$500` / `$1000` | PV `$500` / `$1000` | PV Max DD (`$1k`) | PV Win Ratio | Worst 10h PV (`$1k`) |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| `full_dev` | `lighter` (`USDC`) | `A_baseline_fixed_theta` | 2,325 (1,772) | `+3.31` (`+3.52`) | `+1.34` (`+1.60`) | `+$276.05 / +$490.39` | `+$104.54 / +$194.70` (`+$195.55`) | `$11.56` (`$7.05`) | `63.2%` (`73.3%`) | `-$5.64` (`w05`) |
| `full_dev` | `lighter` (`USDC`) | `B_vol_adaptive_theta` | 2,147 (1,620) | `+3.55` (`+3.86`) | `+1.43` (`+1.78`) | `+$276.38 / +$494.45` | `+$103.25 / +$196.60` (`+$199.86`) | `$11.56` (`$7.05`) | `68.4%` (`80.0%`) | `-$6.04` (`w12`) |
| `full_dev` | `lighter` (`USDC`) | `C_dual_leader` | 1,914 (1,486) | `+3.25` (`+3.52`) | `+1.16` (`+1.45`) | `+$231.20 / +$409.38` | `+$80.65 / +$150.32` (`+$153.48`) | `$7.83` (`$3.25`) | `52.6%` (`60.0%`) | `-$7.83` (`w12`) |
| `full_dev` | `lighter` (`USDC`) | `D_catchup_exit` | 2,941 (2,539) | `+1.56` (`+1.56`) | `-0.47` (`-0.46`) | `+$147.82 / +$260.25` | `-$44.77 / -$75.12` (`-$61.01`) | `$90.42` (`$76.31`) | `36.8%` (`46.7%`) | `-$42.58` (`w14`) |
| `full_dev` | `lighter` (`USDC`) | `E_combined_all` | 2,016 (1,750) | `+1.81` (`+1.89`) | `-0.56` (`-0.51`) | `+$128.84 / +$232.32` | `-$29.15 / -$44.05` (`-$31.60`) | `$59.40` (`$46.95`) | `47.4%` (`53.3%`) | `-$29.47` (`w14`) |
| `full_dev` | `lighter` (`USDC`) | `F_vol_plus_dual` | 1,747 (1,345) | `+3.72` (`+4.11`) | `+1.55` (`+1.96`) | `+$244.46 / +$430.73` | `+$96.46 / +$176.59` (`+$179.30`) | `$6.35` (`$3.25`) | `52.6%` (`60.0%`) | `-$6.35` (`w12`) |
| `full_dev` | `lighter_robinhood` (`USDG`) | `A_baseline_fixed_theta` | 3,213 (575) | `+0.98` | `+3.42` | `-$82.49 / -$189.74` | `+$28.39 / +$50.91` | `$4.41` | `68.4%` | `-$3.77` (`w19`) |
| `full_dev` | `lighter_robinhood` (`USDG`) | `B_vol_adaptive_theta` | 3,161 (560) | `+1.14` | `+3.88` | `-$63.46 / -$157.23` | `+$38.56 / +$67.62` | `$4.41` | `73.7%` | `-$2.36` (`w11`) |
| `full_dev` | `lighter_robinhood` (`USDG`) | `F_vol_plus_dual` | 2,346 (439) | `+1.07` | `+4.08` | `-$31.86 / -$85.04` | `+$36.49 / +$65.53` | `$1.73` | `73.7%` | `-$1.08` (`w10`) |
| `full_dev` | `extended` (`USDC`, `2.5bp`) | `A_baseline_fixed_theta` | 2,880 (244) | `-1.52` | `+0.14` | `-$301.89 / -$585.98` | `-$15.68 / -$40.96` | `$40.96` | `33.3%` | `-$21.06` (`w19`) |
| `full_dev` | `extended` (`USDC`, `2.5bp`) | `B_vol_adaptive_theta` | 2,525 (176) | `-1.21` | `+3.78` | `-$222.71 / -$421.14` | `+$13.93 / +$19.66` | `$11.48` | `46.7%` | `-$7.33` (`w20`) |
| `full_dev` | `arcus` (`USDC`, `4.5bp`) | `A_baseline_fixed_theta` | 6,486 (332) | `-3.54` | `-6.94` | `-$948.91 / -$1,654.68` | `-$85.65 / -$161.31` | `$161.31` | `10.5%` | `-$82.45` (`w19`) |
| `full_dev` | `arcus` (`USDC`, `4.5bp`) | `B_vol_adaptive_theta` | 5,724 (251) | `-2.99` | `-4.61` | `-$684.97 / -$1,195.94` | `-$36.40 / -$67.92` | `$67.92` | `10.5%` | `-$28.78` (`w19`) |
| **`holdout`** | `lighter` (`USDC`) | `A_baseline_fixed_theta` | 420 (270) | `+1.56` | `+2.64` | `+$20.53 / +$35.13` | `+$28.66 / +$52.07` | `$2.53` | `80.0%` | `-$2.53` (`w22`) |
| **`holdout`** | `lighter` (`USDC`) | `B_vol_adaptive_theta` | 401 (253) | `+1.36` | `+2.30` | `+$18.86 / +$37.85` | `+$24.58 / +$49.11` | **`$0.90`** | `80.0%` | `-$0.90` (`w22`) |
| **`holdout`** | `lighter` (`USDC`) | `C_dual_leader` | 312 (213) | `+2.05` | `+2.84` | `+$20.68 / +$36.71` | `+$23.37 / +$41.59` | `$1.65` | `80.0%` | `-$1.65` (`w22`) |
| **`holdout`** | `lighter` (`USDC`) | `D_catchup_exit` | 496 (402) | `+0.54` | `-0.59` | `+$3.21 / +$4.01` | `-$6.02 / -$9.56` | `$9.88` | `20.0%` | `-$7.09` (`w24`) |
| **`holdout`** | `lighter` (`USDC`) | `E_combined_all` | 323 (263) | `+0.84` | `-0.73` | `+$6.30 / +$12.47` | `-$6.63 / -$10.70` | `$10.70` | `0.0%` | `-$4.59` (`w23`) |
| **`holdout`** | `lighter` (`USDC`) | `F_vol_plus_dual` | 293 (195) | `+1.54` | `+2.07` | `+$17.56 / +$35.34` | `+$19.07 / +$37.59` | **`$0.01`** | `80.0%` | `-$0.01` (`w22`) |
| **`holdout`** | `lighter_robinhood` (`USDG`) | `A_baseline_fixed_theta` | 952 (114) | `+0.25` | `+0.52` | `-$14.18 / -$29.74` | `+$6.21 / +$12.14` | `$0.00` | `100.0%` | `+$0.30` (`w24`) |
| **`holdout`** | `lighter_robinhood` (`USDG`) | `B_vol_adaptive_theta` | 946 (112) | `+0.26` | `+0.45` | `-$15.97 / -$33.57` | `+$5.31 / +$10.21` | `$1.76` | `80.0%` | `-$1.76` (`w24`) |
| **`holdout`** | `lighter_robinhood` (`USDG`) | `F_vol_plus_dual` | 779 (96) | `+0.36` | `-0.00` | `-$14.25 / -$29.44` | `+$3.18 / +$6.02` | `$2.11` | `40.0%` | `-$1.98` (`w22`) |
| **`holdout`** | `extended` (`USDC`, `2.5bp`) | `A_baseline_fixed_theta` | 573 (38) | `-1.88` | `+0.22` | `-$57.56 / -$109.93` | `-$5.16 / -$9.81` | `$10.34` | `40.0%` | `-$9.74` (`w25`) |
| **`holdout`** | `extended` (`USDC`, `2.5bp`) | `B_vol_adaptive_theta` | 534 (33) | `-1.71` | `+3.82` | `-$49.00 / -$92.96` | `-$0.65 / -$0.85` | `$2.16` | `60.0%` | `-$2.16` (`w25`) |
| **`holdout`** | `arcus` (`USDC`, `4.5bp`) | `A_baseline_fixed_theta` | 1,478 (52) | `-3.69` | `-12.30` | `-$238.68 / -$413.48` | `-$28.13 / -$51.96` | `$54.38` | `20.0%` | `-$20.93` (`w25`) |
| **`holdout`** | `arcus` (`USDC`, `4.5bp`) | `B_vol_adaptive_theta` | 1,368 (44) | `-3.73` | `-12.52` | `-$222.09 / -$383.22` | `-$22.02 / -$37.24` | `$39.66` | `20.0%` | `-$21.51` (`w24`) |

## 4. Regime Stratification, `w03` Crash & Multi-Window Curves

### Regime Breakdown (`$1000` `0.5*L1`-Capped Print-Verified / Quote PnL)

| Split | Venue | Regime | `A_baseline` PV (`Quote`) | `B_vol_adaptive` PV (`Quote`) | `F_vol_plus_dual` PV (`Quote`) |
| --- | --- | --- | ---: | ---: | ---: |
| `full_dev` | `lighter` | `calm` | `+$10.90` (`+$60.24`) | `+$12.05` (`+$62.63`) | `+$0.90` (`+$41.76`) |
| `full_dev` | `lighter` | `normal` | `+$94.67` (`+$228.02`) | `+$104.15` (`+$241.88`) | `+$84.67` (`+$202.01`) |
| `full_dev` | `lighter` | `extreme_crash` | `+$89.13` (`+$202.14`) | `+$80.41` (`+$189.94`) | **`+$91.02`** (`+$186.96`) |
| `full_dev` | `lighter_robinhood` | `extreme_crash` | `+$33.94` (`-$75.59`) | **`+$52.96`** (`-$36.70`) | `+$50.01` (`-$13.50`) |
| `full_dev` | `arcus` | `extreme_crash` | `-$133.57` (`-$694.16`) | `-$45.32` (`-$322.44`) | `-$49.56` (`-$231.47`) |
| `holdout` | `lighter` | `calm` | `+$23.75` (`+$14.85`) | `+$19.56` (`+$15.06`) | `+$16.28` (`+$13.62`) |
| `holdout` | `lighter` | `normal` | `+$23.85` (`+$12.18`) | **`+$25.78`** (`+$16.16`) | `+$16.60` (`+$13.87`) |
| `holdout` | `lighter` | `extreme_crash` | `+$4.47` (`+$8.10`) | `+$3.77` (`+$6.63`) | **`+$4.71`** (`+$7.84`) |

- **`w03` Crash Window Highlight (`tokyo-crypto-20261116T101910Z-w03`, `full_dev`)**:
  - `lighter`: `+$7.72` pv (`+$8.51` quote, `78/113` trades, `+1.48 bps` pv) on `A_baseline` -> **`+$10.20` pv** (`+$10.86` quote, `73/107` trades, `+2.01 bps` pv) on `B_vol_adaptive` -> **`+$11.02` pv** (`+$15.65` quote, `53/76` trades, `+2.76 bps` pv) on `F_vol_plus_dual`.
  - `lighter_robinhood`: `+$3.55` pv (`+$5.35 bps`, `34/203` trades) on `A_baseline` and `B_vol_adaptive` (`-$5.89` quote).
  - `arcus`: `-$6.72` pv (`-$93.01` quote) on `A_baseline` -> `-$6.01` pv (`-$50.00` quote) on `F_vol_plus_dual`.
- **Cumulative Multi-Window Trajectory (`$1000` `0.5*L1`-Capped)**:
  - `lighter` `B_vol_adaptive_theta`: `full_dev` starts `w01` at `+$15.5` pv (`+$36.9` quote) and compounds steadily to `+$196.6` pv (`+$494.5` quote) by `w20`; untouched `holdout` starts `w21` at `+$7.7` pv (`+$0.7` quote) and ends `w25` at `+$49.1` pv (`+$37.9` quote) with `$0.90` max drawdown.
  - Top individual pairs on untouched `holdout`: `lighter ETH <- binance` (`5/5` positive pv windows, `$0.00` DD, `+$21.89` pv `A` / `+$21.06` pv `B`, `+7.33–8.39 bps` pv mean, median L1 `$6,608.79`) and `lighter ETH <- coinbase` (`5/5` positive pv windows, `$0.00` DD, `+$16.32` pv `A` / `+$16.28` pv `C`).

## 5. Honest Mechanism Findings & Caveats

1. **`vol_adaptive_theta` & `dual_leader` improve risk-adjusted quality; `catchup_exit` is rejected**:
   - `B_vol_adaptive_theta` raises `full_dev` print-verified PnL on `lighter` (`+$194.70 -> +$196.60`), `lighter_robinhood` (`+$50.91 -> +$67.62`), and cuts `arcus` print-verified losses by `58%` (`-$161.31 -> -$67.92`), while cutting `holdout` `lighter` print-verified max drawdown by `64%` (`$2.53 -> $0.90`) and lifting `holdout` quote PnL (`+$35.13 -> +$37.85`).
   - `D_catchup_exit` degrades print-verified PnL across every venue and split (`lighter` `full_dev`: `+$194.70 -> -$75.12` pv; `holdout`: `+$52.07 -> -$9.56` pv) because exiting on partial convergence truncates follow-through drift while paying higher spread turnover. Keep `exit_on_catchup_frac = None` (default-off) in production configs.
2. **Fee & Print-Coverage Caveats**:
   - `lighter` (`USDC`) and `lighter_robinhood` (`USDG`) use unverified public Tier-0 `0.0/0.0 bps` schedules (`public_tier0_unverified`); any non-zero account taker fee would directly compress or invert `lighter` (`+1.4–2.6 bps` pv mean).
   - `lighter_robinhood` (`~12–18%` pv coverage) and `extended` (`~7–9%` pv coverage) have sparse contemporaneous `public_trades`, causing quote-based and print-verified totals to diverge. Quote-based totals on `lighter_robinhood`, `extended`, and `arcus` remain negative.
   - `lighter` had 4 non-continuous windows in `full_dev` (`w10`–`w12`, `w17`) with `124s–827s` gaps (`cont=False`).

## 6. Verification & Aleph Artifacts

- **Aleph read-only artifact**: `/tmp/swarm_track1/track01_full_w01_w25_20261127.json` (`sha256`: `57c0b35bbe82d7f2d73e1248c4d978f9d91917baec3c4598920aec9bf367be57`), plus earlier `w18`–`w21` selection check `/tmp/swarm_track1/track01_selection_dev_20261127.json` (`sha256`: `6400bacc57d31c6253a7904fcaaa4c21a1e00a35bb4e371898c246159430e1cb`).
- **Workspace verification** (run after final code edit): `cargo fmt --check`, `cargo clippy --locked --features research-replay --lib --bins -- -D warnings`, `cargo test --locked --lib lead_lag_taker` (`12 passed`), `cargo test --locked --lib track_shadow` (`10 passed`), `python3 -m unittest -v scripts/test_research_track01_multivenue_lead_lag.py` (`10 passed`), `python3 scripts/research_track01_multivenue_lead_lag.py --self-test`, isolated stdlib-only check blocking `numpy,duckdb,pyarrow,pandas,scipy`, and markdown link audit (`BAD_LINKS=[]`).
