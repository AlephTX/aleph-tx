# Track 01 P0 Exit Visible-Book Depth Conservation & Controlled Single-Gate Evaluation (`W01–W27`)

**Date:** `2026-11-27` (CST) · **Operational Mode:** Strictly **`HOLD`** (zero live orders/cancellations, zero Tokyo deploys/restarts, read-only `nice -n 10` on `aleph` lakehouse)

**Verdict: `NO-GO / HOLD`.**
1. **P0 Exit Depth Conservation Fixed & Verified**: [research_track01_profitable_alphas.py](../scripts/research_track01_profitable_alphas.py) (`_execute_exit`) now tracks `rem_avail_by_quote` keyed by displayed quote timestamp (`q_ts`) so a single touch is never reused across `1 s` exit loop steps or double-counted across maker partial exit + taker fallback on the same snapshot, and enforces the strict declared cutoff `ts_ms[j_cur] + TAKER_DELAY_MS <= sig.t1_ms`. Synthetic tests (`7/7` PASS) confirm the `$200` L1 bid @ `50%` cap counterexample fills `$100` and holds `$400` terminal residual until an explicit new book snapshot arrives. Across historical continuous `W01–W27` (`500 ms` BBO buckets), before-vs-after net PnL changes by at most `+$0.26` (`$0.00` terminal residual), proving the `w26_w27_oos` failure (`-$35.18` RH / `-$77.18` Lighter Prem) is genuine signal decay rather than an exit-depth bug.
2. **Roadmap `§4` Oasis-Style Controlled Single-Gate Experiment (`OverreactionReversion` `lb15m_r60_rf0_f0` RH `0/0`)**: Freezing a single causal tight-spread gate (`spread_bps <= 5.0 bps`) purely on `selection_dev` (`w01–w20`, no multi-parameter grid search) lifts `selection_dev` from `+$182.49` to `+$217.70` (`+$35.21`, stationary bootstrap 95% CI `[+$11.06, +$61.44]`), **but degrades untouched `holdout` (`w21–w25`) by `-$9.91`** (`+$115.15` vs `+$125.06`, 95% CI `[-$23.55, +$3.74]`) and leaves **`w26_w27_oos` negative at `-$34.68`** vs `-$35.18` (`+$0.51`, `w27` worst window `-$31.74`), with combined OOS (`w21–w27`) `delta = -$9.40` (95% CI `[-$24.53, +$3.53]`, `CSCV PBO = 0.4286` across the 2 arms / `0.5857` across the 4 dev-screened single-factor directions).

---

## 1. P0 Exit Visible-Book Depth Conservation Fix

Implemented in `_execute_exit` inside [research_track01_profitable_alphas.py](../scripts/research_track01_profitable_alphas.py) and tested in [test_research_track01_profitable_alphas.py](../scripts/test_research_track01_profitable_alphas.py) against the peer counterexample (`track01_exit_visible_book_budget_peer_20261127`):

- **Per-touch remaining depth budget (`rem_avail_by_quote`)**:
  - When `maker_postonly` executes a passive chase fill `sl` on quote `q_o[0]`, `rem_avail_by_quote[q_o[0]]` is initialized from `L1_DEPTH_CAP_FRAC * rp * rs` and decremented by `sl`.
  - In the subsequent `1 s` IOC taker loop, if `_taker_fill` returns `q_ts = f_out[2]` already in `rem_avail_by_quote`, available depth is clamped to `min(L1_DEPTH_CAP_FRAC * f_out[1], rem_avail_by_quote[q_ts])`, preventing repeated `0.5 * L1` fills against an unchanged BBO snapshot as well as combined maker + taker over-consumption on the same touch.
  - Note: on real `500 ms` bucketed lakehouse BBO streams without exchange book-sequence IDs, a newly timestamped `500 ms` bucket updates `q_ts` and refreshes the `0.5 * L1` budget; true order-ID queue depletion requires native book sequence metadata (as noted in the peer fixture review).
- **Strict declared cutoff (`ts_ms[j_cur] + TAKER_DELAY_MS <= sig.t1_ms`)**:
  - Replaces `t_ex = min(sig.t1_ms, ts_ms[j_cur] + TAKER_DELAY_MS)` with an explicit break when `ts_ms[j_cur] + TAKER_DELAY_MS > sig.t1_ms`, keeping unexecuted post-cutoff exits in `terminal_residual_notional_usd` priced conservatively via `_latest_at(sig.bbo, sig.bbo_ts, sig.t1_ms)`.
- **Deterministic unit test coverage** (`test_research_track01_profitable_alphas.py`):
  - Single `$200` L1 bid @ `50%` cap over `5 s` steps -> `$100.00` fill (`last_fill_out = 1330`) + `$400.00` residual (vs legacy `$500.00` / `$0.00`).
  - Maker partial exit (`$60.00`) + taker fallback (`$40.00`) on the same `$200` touch -> `$100.00` combined fill + `$400.00` residual.
  - Explicit new book update (`$1,000` L1 bid at `t = 3000`) -> unblocks remaining `$400.00` (`$500.00` total, `$0.00` residual).
  - Exit arrival past declared cutoff (`2000 + 330 > 2200`) -> `$0.00` taker fill, `$500.00` terminal residual MTM (`-$5.00`).

---

## 2. Part A: Exact Before-vs-After P0 Rescore Across `W01–W27`

Evaluated via [research_track01_controlled_gate_eval.py](../scripts/research_track01_controlled_gate_eval.py) across all `W01–W27` windows (`19` `selection_dev` `w01–w20`, `5` `holdout` `w21–w25`, `2` `w26_w27_oos` `w26–w27`; strictly excluding `w08` and `-w28`). Fee models loaded from `.agent-state/research/account_fee_evidence_20261127.json` (`sha256: 4d3a9c1aeae5358bb9c55ab448db392ac3edb0a89244f74c23d89817c3359d92`): `lighter_robinhood` `0.0/0.0 bps` (`USDG`) and `lighter` `0.40/2.80 bps` verified Premium (`0.0/0.0 bps` Standard counterfactual). Redacted output stored outside Git at `.agent-state/research/roadmap_upgrade_20261127/track01_depth_and_single_gate_w01_w27.json` (`sha256: 0e7802b2e2df6e2d29ceb8d8e88d8b250ddede0a0e34e00af457f0113939ddb8`, `SECRET_HITS=0`, `W28_CAMPAIGN_HITS=0`).

| Frozen Arm (`$500` Taker) | Split | Before `Realized` / `MTM` ($) | After `Realized` / `MTM` ($) | After `Prem 0.40/2.80` ($) | Before -> After `Res Notl` / `Res Tr` | `Clipped Exits` (Before -> After) | Delta `After - Before` (`0/0` / `Prem` $) |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| `Overreaction` `lb15m_r60_rf0_f0` RH (`0/0`) | `selection_dev` | `+182.5426 / +182.5426` | `+182.4925 / +182.4925` | `+182.4925` | `$0.00 / 0` -> `$0.00 / 0` | `742 -> 742` | `-0.0501 / -0.0501` |
| `Overreaction` `lb15m_r60_rf0_f0` RH (`0/0`) | `holdout` | `+125.0551 / +125.0551` | `+125.0551 / +125.0551` | `+125.0551` | `$0.00 / 0` -> `$0.00 / 0` | `284 -> 284` | `0.0000 / 0.0000` |
| `Overreaction` `lb15m_r60_rf0_f0` RH (`0/0`) | `w26_w27_oos` | `-35.1894 / -35.1894` | `-35.1833 / -35.1833` | `-35.1833` | `$0.00 / 0` -> `$0.00 / 0` | `28 -> 28` | `+0.0061 / +0.0061` |
| `Overreaction` `lb15m_r60_rf0_f0` Lighter (`0.40/2.80`) | `selection_dev` | `+172.2719` (`0/0`) | `+172.5090` (`0/0`) | **`-301.8363`** | `$0.00 / 0` -> `$0.00 / 0` | `764 -> 764` | `+0.2371 / +0.2624` |
| `Overreaction` `lb15m_r60_rf0_f0` Lighter (`0.40/2.80`) | `holdout` | `+121.1670` (`0/0`) | `+121.1766` (`0/0`) | **`-54.4363`** | `$0.00 / 0` -> `$0.00 / 0` | `313 -> 313` | `+0.0096 / +0.0096` |
| `Overreaction` `lb15m_r60_rf0_f0` Lighter (`0.40/2.80`) | `w26_w27_oos` | `-50.4447` (`0/0`) | `-50.4575` (`0/0`) | **`-77.1790`** | `$0.00 / 0` -> `$0.00 / 0` | `32 -> 32` | `-0.0128 / -0.0128` |
| `OracleMom` `d25_h30m_f1` RH (`rwa_only` = `all`) | `selection_dev` | `-74.5440 / -74.5440` | `-74.5535 / -74.5535` | `-74.5535` | `$0.00 / 0` -> `$0.00 / 0` | `159 -> 159` | `-0.0095 / -0.0095` |
| `OracleMom` `d25_h30m_f1` RH (`rwa_only` = `all`) | `holdout` | `-8.2834 / -8.2834` | `-8.2834 / -8.2834` | `-8.2834` | `$0.00 / 0` -> `$0.00 / 0` | `21 -> 21` | `0.0000 / 0.0000` |
| `OracleMom` `d25_h30m_f1` RH (`rwa_only` = `all`) | `w26_w27_oos` | `-18.0968 / -18.0968` | `-18.0968 / -18.0968` | `-18.0968` | `$0.00 / 0` -> `$0.00 / 0` | `43 -> 43` | `0.0000 / 0.0000` |
| `OracleMom` `d25_h30m_f1` Lighter (`rwa_only` = `all`) | `selection_dev` | `-66.5374` (`0/0`) | `-66.5355` (`0/0`) | **`-168.2533`** | `$0.00 / 0` -> `$0.00 / 0` | `184 -> 183` | `+0.0019 / +0.0019` |
| `OracleMom` `d25_h30m_f1` Lighter (`rwa_only` = `all`) | `holdout` | `-9.8495` (`0/0`) | `-9.8495` (`0/0`) | **`-23.5298`** | `$0.00 / 0` -> `$0.00 / 0` | `17 -> 17` | `0.0000 / 0.0000` |
| `OracleMom` `d25_h30m_f1` Lighter (`rwa_only` = `all`) | `w26_w27_oos` | `-34.6410` (`0/0`) | `-34.6504` (`0/0`) | **`-65.2303`** | `$0.00 / 0` -> `$0.00 / 0` | `43 -> 43` | `-0.0094 / -0.0094` |

*(In `selection_dev` on `lighter` `Overreaction` `lb15m_r60_rf0_f0`, pre-P0 vs post-P0 volume shifted by `-$90.88` from `$1,694,180.99` to `$1,694,090.11` and `Prem` improved `+$0.2624`; across all continuous `W01–W27` windows, `500 ms` BBO updates arrive rapidly enough before `t1_ms` that every clipped exit completes cleanly without terminal residual. Note that the baseline `Overreaction` numbers here match [TRACK01_DIRECTIONAL_ALPHA_PROFITABILITY_2026_11_27.md](TRACK01_DIRECTIONAL_ALPHA_PROFITABILITY_2026_11_27.md) prior to P0.)*

---

## 3. Part B: Oasis-Style Single-Factor Controlled Experiment (`§4`)

Following the research protocol in `docs/ALEPHTX_RESEARCH_AND_EXECUTION_ROADMAP_2026_11_27.md` (`§4`), we evaluated **one** causal single-factor gate frozen purely on `selection_dev` (`w01–w20`) against `baseline` and `flat` on `OverreactionReversion` (`lb15m_r60_rf0_f0`, `lighter_robinhood` `0.0/0.0 bps`, `$500` clip, `exec_mode = "taker"`, P0-fixed `_execute_exit`).

- **Pure `selection_dev` Gate Freeze (zero OOS peeking, zero grid search)**:
  On `selection_dev` (`w01–w20`), baseline entries with `spread_bps > 5.0 bps` (`160 / 2,243` trades) lost `-$49.47`, whereas `spread_bps <= 5.0 bps` (`2,083` trades) earned `+$231.96`. Comparing the three pre-specified single-factor directions purely on `selection_dev` with independent position/cooldown/depth state (`baseline: +$182.49`, `tight_spread_le_5bp: +$217.70`, `us_rth_utc: +$216.62`, `l1_depth_ge_1000: +$215.91`), we froze **`single_gate = (spread_bps <= 5.0 bps)`** (`tight_spread_le_5bp`) prior to inspecting `holdout` or `w26_w27_oos`.
- **Shared Candidate Set & Independent Arm State**:
  Both `baseline` and `single_gate` (plus `flat`) observe the exact same `978,539` candidate `(win, symbol, t_dec)` events across `W01–W27` (`700,336` `selection_dev`, `252,937` `holdout`, `25,266` `w26_w27_oos`), identical visible timestamps, `$500` clip cap, and `15m` max-hold / `45 bps` SL exit rules, while each arm maintains its own position, `5m` cooldown, and `rem_avail_by_quote` L1 depth budget.

### 3.1 Controlled Arm Comparison (`baseline` vs `single_gate` vs `flat`)

| Split (`Windows`) | Arm | `Candidate` -> `Checked` -> `Gate Rej` -> `Filled` | `Realized` / `MTM 0/0` ($) | `+1.0 bp` Stress ($) | `Worst Win` ($) | `Max DD` ($) | `Res Notl` ($) | `Win SR` / `PSR(0)` | `Ex-Top1` (`Top1`) |
| :--- | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: | :--- |
| `selection_dev` (`19`) | `baseline` | `700,336 -> 2,244 -> 0 -> 2,243` | `+182.49 / +182.49` | `+2.43` | `-95.91` (`w14`) | `147.58` | `$0.00` | `+0.216 / 0.818` | `+94.24` (`NVDA +88.25`) |
| `selection_dev` (`19`) | `single_gate` | `700,336 -> 3,323 -> 1,088 -> 2,234` | **`+217.70 / +217.70`** | **`+38.67`** | `-84.65` (`w14`) | `135.66` | `$0.00` | `+0.262 / 0.874` | `+117.12` (`NVDA +100.58`) |
| `selection_dev` (`19`) | `flat` | `0 -> 0 -> 0 -> 0` | `0.00 / 0.00` | `0.00` | `0.00` | `0.00` | `$0.00` | `0.000 / 0.000` | `0.00` (`None`) |
| `holdout` (`5`) | `baseline` | `252,937 -> 761 -> 0 -> 761` | **`+125.06 / +125.06`** | **`+65.15`** | `-6.67` (`w21`) | `6.67` | `$0.00` | `+1.197 / 0.952` | `+92.85` (`NVDA +32.20`) |
| `holdout` (`5`) | `single_gate` | `252,937 -> 1,334 -> 572 -> 762` | `+115.15 / +115.15` | `+55.45` | `-8.98` (`w21`) | `8.98` | `$0.00` | `+1.037 / 0.950` | `+82.95` (`NVDA +32.20`) |
| `holdout` (`5`) | `flat` | `0 -> 0 -> 0 -> 0` | `0.00 / 0.00` | `0.00` | `0.00` | `0.00` | `$0.00` | `0.000 / 0.000` | `0.00` (`None`) |
| `w26_w27_oos` (`2`) | `baseline` | `25,266 -> 93 -> 0 -> 93` | `-35.18 / -35.18` | `-42.13` | `-32.16` (`w27`) | `35.18` | `$0.00` | `0.000 / 0.000` | `-35.18` (`AAPL 0.00`) |
| `w26_w27_oos` (`2`) | `single_gate` | `25,266 -> 99 -> 7 -> 92` | `-34.68 / -34.68` | `-41.61` | `-31.74` (`w27`) | `34.68` | `$0.00` | `0.000 / 0.000` | `-34.68` (`AAPL 0.00`) |
| `w26_w27_oos` (`2`) | `flat` | `0 -> 0 -> 0 -> 0` | `0.00 / 0.00` | `0.00` | `0.00` | `0.00` | `$0.00` | `0.000 / 0.000` | `0.00` (`None`) |
| `oos_w21_w27` (`7`) | `baseline` | `278,203 -> 854 -> 0 -> 854` | **`+89.87 / +89.87`** | **`+23.02`** | `-32.16` (`w27`) | `35.18` | `$0.00` | `+0.456 / 0.846` | `+63.94` (`NVDA +25.94`) |
| `oos_w21_w27` (`7`) | `single_gate` | `278,203 -> 1,433 -> 579 -> 854` | `+80.47 / +80.47` | `+13.84` | `-31.74` (`w27`) | `34.68` | `$0.00` | `+0.410 / 0.830` | `+54.54` (`NVDA +25.94`) |
| `all_w01_w27` (`26`) | `baseline` | `978,539 -> 3,098 -> 0 -> 3,097` | `+272.36 / +272.36` | `+25.45` | `-95.91` (`w14`) | `147.58` | `$0.00` | `+0.261 / 0.898` | `+158.18` (`NVDA +114.18`) |
| `all_w01_w27` (`26`) | `single_gate` | `978,539 -> 4,756 -> 1,667 -> 3,088` | `+298.17 / +298.17` | `+52.51` | `-84.65` (`w14`) | `135.66` | `$0.00` | `+0.290 / 0.931` | `+171.66` (`NVDA +126.52`) |

### 3.2 Stationary Block Bootstrap 95% CI (`delta = single_gate - baseline`) & Multiple Testing

Politis–Romano (1994) stationary block bootstrap (`mean_block_len = 3.0` windows, `n_boot = 5,000`, `seed = 20261127`) over paired per-window `delta(single_gate - baseline)`:

| Split (`N` Windows) | Observed Total Delta (`0/0` $) | Total `95% CI` (`0/0` $) | Mean/Win `95% CI` (`0/0` $) | `P_boot(Delta > 0)` | Observed Delta (`+1.0 bp` $) | `+1.0 bp` Total `95% CI` ($) |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| `selection_dev` (`19`) | **`+$35.2088`** | `[+$11.0565, +$61.4373]` | `[+$0.5819, +$3.2335]` | `0.9978` | `+$36.2348` | `[+$11.4887, +$63.0343]` |
| `holdout` (`5`) | **`-$9.9053`** | `[-$23.5497, +$3.7391]` | `[-$4.7099, +$0.7478]` | `0.1080` | `-$9.7017` | `[-$23.2270, +$3.8236]` |
| `w26_w27_oos` (`2`) | `+$0.5068` | `[+$0.1890, +$0.8246]` | `[+$0.0945, +$0.4123]` | `1.0000` | `+$0.5243` | `[+$0.1890, +$0.8596]` |
| `oos_w21_w27` (`7`) | **`-$9.3985`** | `[-$24.5293, +$3.5304]` | `[-$3.5042, +$0.5043]` | `0.1142` | `-$9.1774` | `[-$24.1322, +$3.6118]` |
| `all_w01_w27` (`26`) | `+$25.8103` | `[-$13.0278, +$62.7903]` | `[-$0.5011, +$2.4150]` | `0.8990` | `+$27.0574` | `[-$12.0188, +$64.4154]` |

- **Multiple Testing (`DSR` & `CSCV PBO`)**:
  - **2-arm controlled comparison (`baseline`, `single_gate`)**:
    - `selection_dev` (`19` windows): `N_trials_raw = 2`, `N_eff = 1.0043`, `DSR_raw = 0.8580`, `DSR_eff = 0.8740`, `CSCV PBO = 0.2000` (`70` splits, `16` windows used).
    - `all_w01_w27` (`26` windows): `N_trials_raw = 2`, `N_eff = 1.0061`, `DSR_raw = 0.9229`, `DSR_eff = 0.9306`, **`CSCV PBO = 0.4286`** (`70` splits, `24` windows used).
  - **4-arm discovery-adjusted accounting (`baseline`, `single_gate`, `screen_us_rth_utc`, `screen_l1_ge_1000`)**:
    - `selection_dev` (`19` windows): `N_trials_raw = 4`, `N_eff = 1.5450`, `DSR_raw = 0.9765`, `DSR_eff = 0.9921`, `CSCV PBO = 0.2143`.
    - `all_w01_w27` (`26` windows): `N_trials_raw = 4`, `N_eff = 1.5822`, `DSR_raw = 0.9944`, `DSR_eff = 0.9992`, **`CSCV PBO = 0.5857`**.

### 3.3 Conditional Markout Attribution (`1m / 5m / 15m / 60m`)

| Cohort | Split | Group (`N`) | `1m` Mid (`n`) | `5m` Mid (`n`) | `15m` Mid (`n`) | `60m` Mid (`n`) | `15m` vs `px_in` | Realized (`bp`) |
| :--- | :--- | :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| Filled Trades | `selection_dev` | `baseline` (`2,243`) | `-0.5330` (`2,241`) | `+1.2963` (`2,222`) | `+3.1938` (`2,182`) | `+7.6565` (`1,999`) | `+2.0292` | `+1.7424` |
| Filled Trades | `selection_dev` | `single_gate` (`2,234`) | `-0.5335` (`2,232`) | `+1.4555` (`2,214`) | `+3.6135` (`2,174`) | `+7.9600` (`1,993`) | `+2.5595` | `+2.0800` |
| Filled Trades | `holdout` | `baseline` (`761`) | `+1.3110` (`761`) | `+1.8156` (`759`) | **`+5.3623`** (`747`) | `-12.0652` (`675`) | `+4.1274` | **`+4.6579`** |
| Filled Trades | `holdout` | `single_gate` (`762`) | `+0.9376` (`762`) | `+1.7546` (`760`) | `+4.5949` (`749`) | `-12.5524` (`677`) | `+3.4707` | `+3.6867` |
| Filled Trades | `w26_w27_oos` | `baseline` (`93`) | `-0.5301` (`91`) | `-6.0140` (`91`) | `-7.1675` (`89`) | `+14.1619` (`68`) | `-8.3861` | `-10.8117` |
| Filled Trades | `w26_w27_oos` | `single_gate` (`92`) | `-0.6437` (`90`) | `-6.0621` (`90`) | `-6.8370` (`88`) | `+15.7430` (`67`) | `-8.0079` | `-10.3955` |
| Declustered (`60s`) Candidates | `selection_dev` | `gate_pass` (`13,549`) | `-0.1305` (`13,532`) | `+0.2674` (`13,452`) | `+0.5673` (`13,296`) | `+1.8469` (`11,921`) | `-0.5667` | — |
| Declustered (`60s`) Candidates | `selection_dev` | `gate_fail` (`1,138`) | `-2.4692` (`1,138`) | **`-6.5074`** (`1,133`) | **`-4.1657`** (`1,123`) | `+19.7943` (`1,015`) | `-7.8374` | — |
| Declustered (`60s`) Candidates | `holdout` | `gate_pass` (`4,688`) | `-0.0776` (`4,687`) | `+1.5566` (`4,677`) | `+8.1410` (`4,615`) | `-6.6747` (`4,176`) | `+6.9901` | — |
| Declustered (`60s`) Candidates | `holdout` | `gate_fail` (`553`) | `-0.5388` (`552`) | `-4.4258` (`552`) | **`+22.3874`** (`548`) | `-34.3114` (`522`) | `+18.7967` | — |
| Declustered (`60s`) Candidates | `w26_w27_oos` | `gate_pass` (`527`) | `-0.1385` (`523`) | `-1.6017` (`507`) | `-3.2370` (`461`) | `+4.6274` (`355`) | `-4.3129` | — |
| Declustered (`60s`) Candidates | `w26_w27_oos` | `gate_fail` (`14`) | `-0.9261` (`14`) | `+1.7087` (`14`) | `-3.8488` (`14`) | `+22.0134` (`11`) | `-6.9113` | — |

**Causal Attribution**: In `selection_dev`, `gate_fail` (`spread_bps > 5.0`) exhibited strongly negative `5m` (`-6.51 bp`) and `15m` (`-4.17 bp`) mid markouts, so blocking it raised `selection_dev` net cash by `+$35.21`. In untouched `holdout` (`w21–w25`), however, `gate_fail` experienced a `+22.39 bp` `15m` bounce (`+18.80 bp` vs entry price), so rejecting wide-spread triggers lowered `holdout` net cash by `-$9.91`. More fundamentally, in `w26_w27_oos` even tight-spread (`gate_pass`) `5m` and `15m` markouts flipped negative (`-1.60 bp` and `-3.24 bp`), confirming regime decay in the underlying `15m` overreaction-reversion signal.

### 3.4 Regression Verdict Enforcement (`NO-GO / HOLD`)

| Regression Check | Result | Observed Value |
| :--- | :---: | :--- |
| `holdout_mtm_positive` | PASS | `+$115.15 > $0.00` |
| `holdout_plus_1bp_positive` | PASS | `+$55.45 > $0.00` |
| `w26_w27_oos_mtm_positive` | **FAIL** | **`-$34.68 <= $0.00`** |
| `w26_w27_oos_delta_positive` | PASS | `+$0.51 > $0.00` |
| `oos_w21_w27_delta_clears_threshold` (`>= $10.00`) | **FAIL** | **`-$9.40 < $10.00`** |
| `oos_w21_w27_boot_ci95_lower_gt_zero` | **FAIL** | **`-$24.53 <= $0.00`** |
| `holdout_zero_residual` | PASS | `$0.00` |
| `w26_w27_zero_residual` | PASS | `$0.00` |

**Final Decision: `NO-GO / HOLD`.** Do not promote `OverreactionReversion` (with or without `tight_spread_le_5bp`) or `OracleDiscoveryMomentum` to live or shadow deployment.

---

## 4. Verification Summary

- `python3 -S -m unittest -v scripts/test_research_track01_profitable_alphas.py scripts/test_research_track01_controlled_gate_eval.py`: **11/11 tests PASS** (including isolated `python3 -S` with `duckdb`/`numpy`/`pandas`/`pyarrow`/`scipy` blocked, P0 exit depth conservation unit tests, controlled arm tests, stationary bootstrap determinism, and `NO-GO / HOLD` regression checks).
- `python3 -S scripts/research_track01_profitable_alphas.py --self-test` & `python3 -S scripts/research_track01_controlled_gate_eval.py --self-test`: **PASS**.
- `git diff --check`: clean.
- Redacted artifact `.agent-state/research/roadmap_upgrade_20261127/track01_depth_and_single_gate_w01_w27.json`: `SECRET_HITS = 0`, `W28_CAMPAIGN_HITS = 0`.
