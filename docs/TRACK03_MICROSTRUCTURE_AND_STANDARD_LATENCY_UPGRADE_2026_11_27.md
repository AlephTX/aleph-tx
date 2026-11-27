# Track 03 (`03_incentive_turnover`): Microstructure Upgrades & Lighter Standard (`0/0 bps`) Latency Evaluation (`W01–W26`)

**Date:** `2026-11-27` (CST) · **Operational Mode:** Strictly **`HOLD`** (zero live orders/cancels, zero Tokyo deploys/restarts, zero `crate::exchanges::*` in `src/strategy/`, `rebate_bps = 0.0`)

This report documents the Track 03 (`03_incentive_turnover` / `QueuePriorityMmEngine`) empirical root-cause latency diagnosis, Rust microstructure upgrades, Round-2 iterative optimization (`selection_dev w01–w20`), and frozen validation on untouched `holdout (w21–w25)` and `w26_oos` across Aleph's `W01–W26` lakehouse (`25` Crypto windows, `20` RWA windows), plus closure of Gemini #137 in RWA volume-farming replay.

---

## 1. Executive Verdict

1. **Latency & Toxic-Flow Root Cause (Stage A, `selection_dev w01–w20`)**:
   - Raising Lighter Standard (`0/0 bps`) maker/cancel latency from fast `(36, 33) ms` to realistic speed-bump `(200, 300) ms` causes **cancel-in-flight toxic fill share (`cif`)** to surge **6.9x** on Crypto `lighter` (`7.2% -> 49.5%`, `1s` markout `-0.390 bps`), **6.7x** on Crypto `lighter_robinhood` (`7.3% -> 48.8%`, `1s` markout `-0.305 bps`), **7.7x** on RWA `lighter` (`3.2% -> 24.7%`, `1s` markout `-0.406 bps`), and **6.8x** on RWA `lighter_robinhood` (`2.9% -> 19.8%`, `1s` markout `-0.424 bps`).
   - Simultaneously, consecutive same-side sweep clustering (`<= 2s` between same-side fills) reaches **`36.9%`** on Crypto `lighter` (`1s` markout `-0.278 bps`) and **`26.0%`** on RWA `lighter_robinhood`, while forced 15s IOC unwinds (`tslp`) drive **`39.7%`** of RWA `lighter` exit notional (`-$303.82`) and **`51.0%`** of Crypto `lighter_robinhood` exit notional (`-$234.24`).
   - Consequently, RWA `lighter` (`0/0` counterfactual) flips from net-profitable **`-$5.76/M`** at `(36, 33) ms` to **`+$5.67/M`** at `(200, 300) ms` and **`+$9.15/M`** at `(300, 400) ms`.
2. **Rust Microstructure Upgrades & Frozen `W01–W26` Generalization (Stage B, `o200_c300` & `o300_c300`)**:
   - **Pre-Move L1 Microprice + Signed TFI Skew & Veto (`G5_microprice_tfi`)** + **Post-Fill Self-Adverse-Selection Stepback (`G6_self_fill_stepback`)** + **Patient Passive-Pennying Unwind (`G7_patient_passive_unwind`)** (`G8_combined`) consistently reduce forced IOC `tslp` and same-side clustering across `selection_dev`, untouched `holdout (w21–w25)`, and `w26_oos`:
     - **Crypto `lighter` (`USDC`, `0/0` `lighter_standard_zero_scenario`)**: `G8_combined` lowers double-sided cost at `(200, 300)` from `+$22.10/M -> +$19.05/M` (`-13.8%`, `selection_dev`), `+$21.63/M -> +$20.04/M` (`-7.4%`, `holdout`), and `+$14.91/M -> +$13.06/M` (`-12.4%`, `w26_oos`); at `(300, 300)` from `+$17.94/M -> +$15.64/M` (`selection_dev`), `+$16.04/M -> +$15.70/M` (`holdout`), and `+$13.03/M -> +$11.68/M` (`w26_oos`).
     - **Crypto `lighter_robinhood` (`USDG`, `0/0` `verified_account_tier`)**: `G8_combined` cuts `selection_dev` cost `+$26.40/M -> +$23.51/M` (`-10.9%` at `(200, 300)`) and `+$28.70/M -> +$24.13/M` (`-15.9%` at `(300, 300)`), holds `+$33.81/M` vs `+$34.04/M` (`holdout`), and `G5_microprice_tfi` cuts `w26_oos` cost `+$30.56/M -> +$24.12/M` (`-21.1%`).
     - **RWA `lighter` (`USDC`, `0/0` `lighter_standard_zero_scenario`)**: `G8_combined` cuts `(200, 300)` cost **-74.5%** in `selection_dev` (`+$5.68/M -> +$1.45/M`, `s: 0.397 -> 0.280`), **-6.3%** in `holdout` (`+$13.41/M -> +$12.56/M`), and **-66.7%** in `w26_oos` (`+$4.53/M -> +$1.51/M`), with `AAPL` achieving **`-$10.48/M` (`+$55.18` net, `80%` positive windows)** in `selection_dev`, **`-$0.08/M` (`+$0.10` net, `50%` positive)** in `holdout`, and **`-$14.99/M` (`+$4.90` net)** in `w26_oos`.
     - **RWA `lighter_robinhood` (`USDG`, `0/0` `verified_account_tier`)**: `G5_microprice_tfi` lowers `(200, 300)` cost **-19.4%** in `selection_dev` (`+$10.94/M -> +$8.82/M`), **-13.3%** in `holdout` (`+$12.00/M -> +$10.41/M`), and **-44.0%** in `w26_oos` (`+$3.84/M -> +$2.15/M`, with `AAPL` flipping net-positive to **`-$1.51/M` / `+$0.43`** on `G5` and **`-$1.94/M` / `+$0.65`** on `G8`).
3. **Go / No-Go Verdict:** **Strict `HOLD`.** At `rebate_bps = 0.0`, aggregate Crypto and RWA cohorts remain net-cost (`+$1.45/M` to `+$33.81/M`) under realistic `200–300 ms` Standard latency, and `lighter` (`USDC`) remains on `0.4 / 2.8 bps` Premium unless explicitly switched to Standard (`lighter_standard_zero_scenario`).

---

## 2. Gemini #137 Closure (`replay_rwa_volume_farming.py`)

Updated [replay_rwa_volume_farming.py](../scripts/replay_rwa_volume_farming.py) and [test_replay_rwa_volume_farming.py](../scripts/test_replay_rwa_volume_farming.py):
- `build_default_targets` now checks verified account profiles (`arcus_rwa_verified`, `lighter_verified`, `lighter_robinhood_verified`) **before** tier-0 schedule fallbacks (`arcus_rwa_tier0`, `lighter_robinhood_public_tier0`, `lighter_public_tier0`).
- Added explicit `--assume-lighter-standard-zero` CLI / `assume_lighter_standard_zero` API toggle overriding Lighter venues to `0.0 / 0.0 bps` with explicit `account_fee_status = "lighter_standard_zero_scenario"` provenance.
- Verified with `11/11` stdlib-only unit tests (`python3 -S -m unittest -v scripts/test_replay_rwa_volume_farming.py`) and `--self-test`.

---

## 3. Rust Engine Upgrades (`src/strategy/queue_priority_mm.rs`)

Added three backward-compatible, default-off (`#[serde(default)]`, zero hot-path allocations, zero `crate::exchanges::*`, preserving `quiet_regime_gate` from `f49111e`) mechanisms to `QueuePriorityMmConfig` and `QueuePriorityMmEngine` in [queue_priority_mm.rs](../src/strategy/queue_priority_mm.rs):

| Feature | Config Knobs (Default Off) | Mechanism & Safety Invariants |
|---|---|---|
| **Pre-Move L1 Microprice + Signed TFI Skew & Veto** | `microprice_alpha_weight = 0.0`, `tfi_ewma_tau_ms = 0.0`, `tfi_skew_bps = 0.0`, `tfi_veto_threshold = 0.0` | Computes size-weighted L1 microprice offset relative to mid and causal volume-weighted signed Trade-Flow Imbalance (`tfi_norm in [-1, 1]`) via `on_trade_print` / `on_market_trade` (resets cleanly on `> max_book_age_ms` gaps). Shifts directional fair value and pulls/blocks endangered **opening** orders (`tfi_adverse_veto`) before mid jumps, **never** blocking `reduce_only` exits (`reducing_edge_floor_bps` accounts for skew). |
| **Post-Fill Self-Adverse-Selection Stepback** | `self_fill_stepback_bps = 0.0`, `self_fill_decay_ms = 0` | On passive fills (`on_fill`), adds exponentially decaying same-side opening stepback (compounding up to `3x` on rapid sweep clusters) to `opening_req_half_bps`, leaving `reduce_only` exits completely exempt. |
| **Patient Passive-Pennying Unwind** | `reduce_only_penny_ticks = 0` | Allows `reduce_only` exits on `spread_ticks >= 2` to step up to `reduce_only_penny_ticks` ticks inside wide spreads (strictly post-only clamped to `[best_bid + tick, best_ask - tick]`) and reprice resting touch exits once when strictly improving exit price. |

All `28/28` unit tests in `strategy::queue_priority_mm::tests` pass (`microstructure_tfi_self_fill_and_reduce_penny_defaults_remain_off_and_validate`, `microprice_and_tfi_skew_and_veto_pull_endangered_openings_before_mid_jumps`, `self_fill_stepback_widens_opening_requotes_and_decays_without_blocking_exits`, `patient_passive_pennying_unwind_steps_inside_wide_spread_non_crossing`, plus all 24 existing G4/lead-lag/ALO tests).

---

## 4. Stage A: Root-Cause Cash & Latency Diagnostics (`selection_dev w01–w20`)

Executed via [research_track03_microstructure_latency_mm.py](../scripts/research_track03_microstructure_latency_mm.py) (`--mode stage-a --split selection_dev`, `1,320` rows, `non_flat_count = 0`, `max_gross_err = 3.81e-09`, `rebate_bps = 0.0`) across `lighter` (`USDC`, `lighter_standard_zero_scenario`) and `lighter_robinhood` (`USDG`, `verified_account_tier`):

| Domain & Venue | `(order, cancel)` ms | `ntl` ($) | `$/M` Double-Sided | `spr` ($) | `drft` ($) | `pex` ($) | `tslp` ($) | `s` | `cif` Share (`1s` mk) | `cluster` Share (`1s` mk) | `1s` mk (bp) |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|---|---:|
| Crypto `lighter` (`USDC`) | `(36, 33)` | `$14.70M` | **+3.20** | `+429.8` | `-740.8` | `+247.4` | `-30.5` | `0.074` | `0.072` (`-0.270`) | `0.380` (`-0.056`) | `+0.001` |
| Crypto `lighter` (`USDC`) | `(100, 200)` | `$19.99M` | **+13.09** | `+402.6` | `-1108.5` | `+219.6` | `-37.1` | `0.065` | `0.354` (`-0.319`) | `0.409` (`-0.171`) | `-0.104` |
| Crypto `lighter` (`USDC`) | `(200, 300)` | `$16.44M` | **+22.10** | `+220.3` | `-990.3` | `+90.0` | `-46.6` | `0.102` | **`0.495`** (`-0.390`) | **`0.369`** (`-0.278`) | `-0.182` |
| Crypto `lighter` (`USDC`) | `(300, 300)` | `$12.04M` | **+17.92** | `+205.3` | `-674.4` | `+93.1` | `-55.4` | `0.167` | `0.482` (`-0.319`) | `0.329` (`-0.246`) | `-0.134` |
| Crypto `lighter` (`USDC`) | `(300, 400)` | `$15.29M` | **+21.02** | `+195.5` | `-867.4` | `+89.5` | `-60.3` | `0.141` | **`0.563`** (`-0.326`) | `0.349` (`-0.281`) | `-0.170` |
| Crypto `lighter_robinhood` (`USDG`) | `(36, 33)` | `$6.28M` | **+4.76** | `+386.0` | `-393.6` | `+145.0` | `-197.3` | `0.452` | `0.073` (`-0.164`) | `0.162` (`+0.153`) | `+0.216` |
| Crypto `lighter_robinhood` (`USDG`) | `(100, 200)` | `$7.69M` | **+16.33** | `+366.3` | `-529.3` | `+148.2` | `-236.4` | `0.443` | `0.352` (`-0.245`) | `0.174` (`-0.050`) | `+0.061` |
| Crypto `lighter_robinhood` (`USDG`) | `(200, 300)` | `$6.73M` | **+26.40** | `+253.5` | `-470.4` | `+96.0` | `-234.2` | `0.510` | **`0.488`** (`-0.305`) | `0.154` (`-0.204`) | `-0.036` |
| Crypto `lighter_robinhood` (`USDG`) | `(300, 300)` | `$5.68M` | **+28.67** | `+213.5` | `-393.1` | `+73.0` | `-219.1` | `0.567` | `0.493` (`-0.302`) | `0.136` (`-0.228`) | `-0.031` |
| Crypto `lighter_robinhood` (`USDG`) | `(300, 400)` | `$6.68M` | **+30.67** | `+212.0` | `-453.7` | `+80.6` | `-248.9` | `0.546` | **`0.567`** (`-0.309`) | `0.148` (`-0.299`) | `-0.083` |
| RWA `lighter` (`USDC`) | `(36, 33)` | `$16.00M` | **-5.76** | `+705.6` | `-566.3` | `+326.2` | `-281.0` | `0.367` | `0.032` (`-0.209`) | `0.201` (`+0.194`) | `+0.197` |
| RWA `lighter` (`USDC`) | `(100, 200)` | `$16.67M` | **+0.97** | `+628.6` | `-658.5` | `+294.1` | `-296.4` | `0.370` | `0.169` (`-0.351`) | `0.200` (`+0.102`) | `+0.121` |
| RWA `lighter` (`USDC`) | `(200, 300)` | `$15.96M` | **+5.67** | `+526.0` | `-641.1` | `+237.8` | `-303.8` | `0.397` | **`0.247`** (`-0.406`) | `0.190` (`+0.054`) | `+0.065` |
| RWA `lighter` (`USDC`) | `(300, 300)` | `$14.78M` | **+6.67** | `+479.7` | `-581.9` | `+206.4` | `-301.2` | `0.424` | `0.244` (`-0.392`) | `0.181` (`+0.072`) | `+0.077` |
| RWA `lighter` (`USDC`) | `(300, 400)` | `$15.47M` | **+9.15** | `+457.5` | `-638.0` | `+205.5` | `-308.0` | `0.415` | **`0.291`** (`-0.403`) | `0.184` (`+0.021`) | `+0.042` |
| RWA `lighter_robinhood` (`USDG`) | `(36, 33)` | `$26.63M` | **+2.57** | `+988.9` | `-1372.1` | `+461.9` | `-215.8` | `0.172` | `0.029` (`-0.381`) | `0.286` (`+0.058`) | `+0.044` |
| RWA `lighter_robinhood` (`USDG`) | `(100, 200)` | `$27.61M` | **+7.80** | `+927.4` | `-1561.3` | `+429.4` | `-226.3` | `0.172` | `0.138` (`-0.430`) | `0.283` (`+0.001`) | `-0.011` |
| RWA `lighter_robinhood` (`USDG`) | `(200, 300)` | `$25.72M` | **+10.94** | `+796.5` | `-1480.0` | `+354.6` | `-233.8` | `0.192` | **`0.198`** (`-0.424`) | `0.260` (`-0.031`) | `-0.041` |
| RWA `lighter_robinhood` (`USDG`) | `(300, 300)` | `$23.78M` | **+11.31** | `+733.3` | `-1352.7` | `+317.4` | `-236.0` | `0.210` | `0.198` (`-0.434`) | `0.244` (`-0.033`) | `-0.042` |
| RWA `lighter_robinhood` (`USDG`) | `(300, 400)` | `$24.81M` | **+13.83** | `+716.3` | `-1469.3` | `+310.6` | `-244.0` | `0.207` | **`0.237`** (`-0.434`) | `0.249` (`-0.065`) | `-0.065` |

---

## 5. Stage B: Round-2 Optimization & Frozen `Holdout (W21–W25)` / `W26` Validation

### 5.1 Round-2 Parameter Selection (Strictly on `selection_dev w01–w20`)

Initial aggressive Round-2 candidates (`microprice_alpha_weight = 0.65`, `tfi_skew_bps = 1.50`, `tfi_veto_threshold = 0.60`, `self_fill_stepback_bps = 1.25`, `reduce_only_penny_ticks = 2`) improved Crypto `lighter` by **-15.6%** (`+$22.10/M -> +$18.65/M` at `(200, 300)`), but over-churned wide-spread RWA quotes and sacrificed `pex` (`+$354.6 -> +$26.3` on `lighter_robinhood` RWA `G7`). After sensitivity sweeps on `selection_dev`, we **froze**:
- `G5_microprice_tfi`: `microprice_alpha_weight = 0.25`, `tfi_ewma_tau_ms = 2000.0`, `tfi_skew_bps = 0.0`, `tfi_veto_threshold = 0.75`
- `G6_self_fill_stepback`: `self_fill_stepback_bps = 0.50`, `self_fill_decay_ms = 1500`
- `G7_patient_passive_unwind`: `reduce_only_penny_ticks = 1`
- `G8_combined`: `G5 + G6 + G7` combined

### 5.2 Frozen Cross-Split Results at `(200, 300) ms` and `(300, 300) ms` (`rebate_bps = 0.0`)

#### Crypto (`BTC, ETH, SOL`) — `selection_dev` (`19w`), `holdout` (`5w`), `w26_oos` (`1w`)

| Split | Venue (`Sett.`, Status) | Lat. | `G4_baseline` `$/M` (`ntl`) | `G5_microprice_tfi` `$/M` (`s`) | `G6_self_fill` `$/M` (`cl`) | `G7_penny1` `$/M` (`s`) | **`G8_combined`** `$/M` (`ntl`, `s`, `cl`) |
|---|---|---|---:|---:|---:|---:|---|
| `selection_dev` | `lighter` (`USDC`, `std_zero`) | `o200_c300` | `+22.10` (`$16.43M`) | `+20.61` (`0.085`) | `+22.05` (`0.350`) | `+20.85` (`0.112`) | **`+19.05`** (`$14.15M`, `0.091`, `0.350`) |
| `selection_dev` | `lighter` (`USDC`, `std_zero`) | `o300_c300` | `+17.94` (`$12.04M`) | `+17.09` (`0.133`) | `+17.86` (`0.311`) | **`+15.48`** (`0.206`) | **`+15.64`** (`$9.77M`, `0.161`, `0.303`) |
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o200_c300` | `+26.40` (`$6.73M`) | `+26.08` (`0.449`) | `+25.94` (`0.133`) | `+25.49` (`0.507`) | **`+23.51`** (`$4.29M`, `0.445`, `0.125`) |
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o300_c300` | `+28.70` (`$5.68M`) | `+26.18` (`0.509`) | `+27.94` (`0.118`) | `+27.18` (`0.571`) | **`+24.13`** (`$3.48M`, `0.510`, `0.109`) |
| `holdout` | `lighter` (`USDC`, `std_zero`) | `o200_c300` | `+21.63` (`$4.08M`) | `+20.20` (`0.095`) | `+22.10` (`0.308`) | `+22.64` (`0.132`) | **`+20.04`** (`$3.43M`, `0.104`, `0.311`) |
| `holdout` | `lighter` (`USDC`, `std_zero`) | `o300_c300` | `+16.04` (`$2.82M`) | `+17.95` (`0.166`) | `+16.79` (`0.267`) | `+15.71` (`0.244`) | **`+15.70`** (`$2.32M`, `0.203`, `0.258`) |
| `holdout` | `lighter_robinhood` (`USDG`) | `o200_c300` | `+34.04` (`$1.49M`) | `+39.53` (`0.457`) | `+35.47` (`0.133`) | `+34.65` (`0.507`) | **`+33.81`** (`$0.97M`, `0.466`, `0.128`) |
| `holdout` | `lighter_robinhood` (`USDG`) | `o300_c300` | `+32.54` (`$1.28M`) | `+33.10` (`0.509`) | **`+32.26`** (`0.127`) | `+34.35` (`0.585`) | `+33.11` (`$0.76M`, `0.533`, `0.111`) |
| `w26_oos` | `lighter` (`USDC`, `std_zero`) | `o200_c300` | `+14.91` (`$1.15M`) | `+13.43` (`0.110`) | `+15.56` (`0.286`) | `+15.95` (`0.139`) | **`+13.06`** (`$1.01M`, `0.109`, `0.287`) |
| `w26_oos` | `lighter` (`USDC`, `std_zero`) | `o300_c300` | `+13.03` (`$0.91M`) | `+14.33` (`0.160`) | `+12.85` (`0.250`) | `+12.31` (`0.233`) | **`+11.68`** (`$0.72M`, `0.189`, `0.244`) |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o200_c300` | `+30.56` (`$0.39M`) | **`+24.12`** (`0.471`) | `+30.59` (`0.132`) | `+31.54` (`0.521`) | `+29.73` (`$0.22M`, `0.494`, `0.107`) |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o300_c300` | `+28.56` (`$0.35M`) | **`+25.19`** (`0.514`) | `+27.96` (`0.122`) | `+28.40` (`0.574`) | `+25.58` (`$0.20M`, `0.537`, `0.103`) |

#### RWA (`AAPL, SPY, QQQ, NVDA, TSLA`) — `selection_dev` (`15w`), `holdout` (`4w`), `w26_oos` (`1w`)

| Split | Venue (`Sett.`, Status) | Lat. | `G4_baseline` `$/M` (`ntl`) | **`G5_microprice_tfi`** `$/M` (`s`, `tslp`) | `G6_self_fill` `$/M` (`cl`) | `G7_penny1` `$/M` (`s`) | **`G8_combined`** `$/M` (`ntl`, `s`, `tslp`) |
|---|---|---|---:|---|---|---:|---|
| `selection_dev` | `lighter` (`USDC`, `std_zero`) | `o200_c300` | `+5.68` (`$15.96M`) | `+3.48` (`0.343`, `-197.7`) | `+5.60` (`0.163`) | `+2.48` (`0.329`) | **`+1.45`** (`$12.81M`, `0.280`, `-170.2`) |
| `selection_dev` | `lighter` (`USDC`, `std_zero`) | `o300_c300` | `+6.67` (`$14.78M`) | `+4.43` (`0.370`, `-195.2`) | `+6.12` (`0.156`) | `+4.29` (`0.361`) | **`+2.57`** (`$11.61M`, `0.310`, `-170.5`) |
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o200_c300` | `+10.94` (`$25.71M`) | **`+8.82`** (`0.174`, `-184.3`) | `+11.61` (`0.217`) | `+12.10` (`0.131`) | `+11.34` (`$24.82M`, `0.108`, `-129.0`) |
| `selection_dev` | `lighter_robinhood` (`USDG`) | `o300_c300` | `+11.31` (`$23.78M`) | **`+9.13`** (`0.190`, `-184.9`) | `+12.23` (`0.202`) | `+12.90` (`0.154`) | `+11.87` (`$22.14M`, `0.129`, `-135.3`) |
| `holdout` | `lighter` (`USDC`, `std_zero`) | `o200_c300` | `+13.41` (`$3.37M`) | `+13.04` (`0.410`, `-49.7`) | `+13.83` (`0.163`) | `+13.15` (`0.422`) | **`+12.56`** (`$2.46M`, `0.374`, `-45.3`) |
| `holdout` | `lighter` (`USDC`, `std_zero`) | `o300_c300` | `+13.78` (`$3.03M`) | `+12.56` (`0.441`, `-47.8`) | `+14.03` (`0.156`) | `+12.94` (`0.463`) | **`+11.50`** (`$2.19M`, `0.424`, `-45.8`) |
| `holdout` | `lighter_robinhood` (`USDG`) | `o200_c300` | `+12.00` (`$6.72M`) | **`+10.41`** (`0.171`, `-52.2`) | `+12.42` (`0.196`) | `+12.04` (`0.150`) | `+12.13` (`$6.14M`, `0.129`, `-42.2`) |
| `holdout` | `lighter_robinhood` (`USDG`) | `o300_c300` | `+11.93` (`$6.10M`) | **`+10.60`** (`0.187`, `-51.0`) | `+12.63` (`0.180`) | `+12.93` (`0.178`) | `+12.73` (`$5.41M`, `0.153`, `-43.8`) |
| `w26_oos` | `lighter` (`USDC`, `std_zero`) | `o200_c300` | `+4.53` (`$0.93M`) | `+3.50` (`0.412`, `-13.7`) | `+4.91` (`0.152`) | `+2.73` (`0.414`) | **`+1.51`** (`$0.68M`, `0.359`, `-12.2`) |
| `w26_oos` | `lighter` (`USDC`, `std_zero`) | `o300_c300` | `+6.40` (`$0.89M`) | `+4.32` (`0.436`, `-13.6`) | `+7.05` (`0.147`) | `+3.92` (`0.441`) | **`+3.02`** (`$0.63M`, `0.391`, `-12.5`) |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o200_c300` | `+3.84` (`$1.61M`) | **`+2.15`** (`0.237`, `-15.0`) | `+2.52` (`0.202`) | `+4.97` (`0.178`) | `+4.91` (`$1.53M`, `0.144`, `-10.3`) |
| `w26_oos` | `lighter_robinhood` (`USDG`) | `o300_c300` | `+4.53` (`$1.52M`) | **`+2.65`** (`0.247`, `-14.7`) | `+4.39` (`0.189`) | `+6.33` (`0.203`) | `+4.87` (`$1.41M`, `0.159`, `-10.6`) |

#### Top RWA Symbol Highlights at `(200, 300) ms`

- **`lighter AAPL` (`USDC`, `lighter_standard_zero_scenario`)**:
  - `selection_dev`: `G4 -$4.66/M` (`+$30.92`, `60%` pos) → `G5 -$6.59/M` (`+$32.95`, `67%` pos) → **`G8 -$10.48/M` (`+$55.18`, `80%` pos)**.
  - `holdout`: `G4 +$1.69/M` (`-$2.81`) → **`G8 -$0.08/M` (`+$0.10`, `50%` pos)**.
  - `w26_oos`: `G4 -$9.04/M` (`+$3.94`) → `G5 -$11.21/M` (`+$3.62`) → **`G8 -$14.99/M` (`+$4.90`, `100%` pos)**.
- **`lighter_robinhood AAPL` (`USDG`, `verified_account_tier`)**:
  - `selection_dev`: `G4 +$12.46/M` → **`G5 +$7.82/M`** (`-37.2%`), `G8 +$8.85/M`.
  - `holdout`: `G4 +$10.18/M` → **`G5 +$5.17/M`** (`-49.2%`), `G8 +$7.88/M`.
  - `w26_oos`: `G4 +$2.88/M` → **`G5 -$1.51/M` (`+$0.43`)**, **`G8 -$1.94/M` (`+$0.65`, `100%` pos)**.

---

## 6. Artifact Provenance & Verification

All pulled JSON artifacts reside untracked under `/Users/metaverse/funspace/aleph-tx/.agent-state/research/strategy_upgrade_20261127/` (`LEAK_COUNT = 0`, `terminal_flat_all = True`, `non_flat_count = 0`, `max_gross_err <= 3.81e-09`, `max_net_err = 0.0`, `rebate_bps = 0.0`):
- `track03_stage_a_selection_dev.json` (`1,320` rows, `sha256: b2ab2cdf87abdb34db0dc555d27ab33fdeec29b742d619bdad48a2a9f6ba2824`)
- `track03_stage_b_selection_dev.json` (`2,640` rows, `sha256: 84faa4d492f51f907c7aba05818e095c4d2e1494875fe5cd5459bf4b9182b8f4`)
- `track03_stage_b_holdout.json` (`700` rows, `sha256: 20b4442f5d0503a70c0b8e376e4ce66e0b9a18dd6777ca8e575ae4c1c08ad5f4`)
- `track03_stage_b_w26_oos.json` (`160` rows, `sha256: 1049f18559d1026703909b56886e78a193f1e5fe57f92b66d4fc3651596b83c6`)
