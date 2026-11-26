# 场景一刷量：无返佣下的中性盈利边际研究与回测执行方案

- 日期：2026-11-27
- 作者：Claude（只读调研 + 回测，未改动任何核心代码）
- 交付对象：Gemini（核对后执行）
- 状态：**调研结论 + 执行方案**。三轨仍 HOLD（零实盘下单/撤单/重启）。

> 本文只依据本地已有数据与已有脚本得出结论。所有"业界实践"均基于公开市场微结构知识，**未经外部链接二次核对**（本环境网络受限），已逐条标注以便 Gemini 复核。所有数字可由本文第 7 节的命令复算。

---

## 0. 本文要回答的问题

用户的三点诉求：
1. 结合业界实践，多币种、多窗口全面测试刷量场景的成本极限。
2. 明确"纯亏钱"是否结构性必然，还是选错了标的/口径。
3. 给出"结合量化参数获取少额中性收益"的可行路径。

一句话结论先行：**在 BTC/ETH @ Hyperliquid 上刷量，结构性亏钱是真的；但那是选错了标的，不是策略无解。** 费后为正的中性做市毛边确实存在且跨窗口稳定，集中在 **RWA 股票标的（DRAM/SNDK/NVDA）+ 零 maker 费场所（Arcus/Lighter）**。但 RWA 的"成交后逆向选择漂移"尚无实测，代理指标提示它很可能同样吃掉毛边。因此本文**不承诺** RWA 盈利，而是把它定为"最有希望、但必须用真实 replay 验证 drift"的唯一主攻方向。

---

## 1. 关键结论

### 1.1 瓶颈不是手续费，是负毛边（在 BTC/ETH 上）

几千刀本金够不到任何返佣档（T5 需场所成交额占比 ≥1.5%，T6 ≥4.0%，已单独申请无回复）。因此无返佣 $/M 成本公式里 rebate 项**整条归零**：

```
cost_per_million (USD/M) = (taker_bp × s − gross_bp) / 2 × 100
  s        = taker 出场占比（被动入场、taker 出场的比例）
  gross_bp = ex-fee 毛边 = (spr+drft+pex+tslp)/notional × 1e4
  volume   = 2 × exit_notional（双边）
```

在 BTC/ETH @ core_hl（HL 场所）的 P1 bookdiff 实测中，7 个 arm 的 `gross_bp` **全为负**（−0.41 ~ −0.52bp）。亏损主项是 `drft`（成交后逆向漂移，−0.18 ~ −0.29bp），不是手续费。

### 1.2 无返佣下的真实极限（BTC/ETH）

- 当前 7 arm 最优为 S1_markout：Arcus ANT 档（taker 1.12bp）约 **+30.6 USD/M**。
- 把 taker 出场占比 s→0（全被动出场）时，成本 = −gross_bp/2×100，**与场所无关**，地板约 **+20 USD/M**（S1_markout）。
- **负成本（盈利刷量）在 BTC/ETH + 当前策略 + 当前数据下结构性不可能**，因为做市本身在向逆向选择漏血（gross_bp<0）。

### 1.3 但费后毛边为正的机会确实存在（多币种多窗口）

对 `multivenue_three_tracks_{sel_w19,holdout_w24,holdout_w25}` 的 `half_spread_minus_fee_bps` 全面统计：**全部场所 61/69 组（币种×场所×窗口）为正；仅看零 maker 费场所为 51/57 为正**（第 3 节表与第 7 节复算命令用的是后者，maker-0 口径）。跨三窗口稳定。核心：机会在 RWA + 零 maker 费场所，不在 BTC/ETH @ HL。

### 1.4 口径矛盾已对齐（这是最容易误判的点）

- `multivenue` 的 `half_spread_minus_fee_bps` = **挂单静态毛边**（报价价差的一半减 maker 费），**不含**成交后 drift。
- `bookdiff` 的 `spr` = **实际成交捕获的价差**，≈0；差额全被 `drft` 吃走。
- 两者不矛盾：我们挂出的价差是赚的（静态 +0.3~1.4bp），但只在价格要穿过来时才成交（逆向选择），实际捕获≈0。
- **核心工程问题从"费率"转为"如何在安静时段也被动成交、在毒性时段不补单"。**

### 1.5 波动率关系：用户直觉对一半

- ✅ 波动越大，静态毛边越大：vol<1.0 → +0.24bp；vol≥2.0 → +0.70bp（价差随波动变宽）。
- ⚠️ 但 drift 涨得更快：bookdiff 六窗实测 `corr(high_vol_ticks, net_bp) = −0.73 (BTC) / −0.46 (ETH)`，波动越高净值越差。
- 结论：盈利不靠追极端行情，而靠**在中低波动、宽相对价差的标的上安静捕获静态毛边**。RWA 恰好满足（`spr/vol` 比值高、绝对波动不极端）。

---

## 2. 数据版图与可用性（决定了什么能真跑、什么是缺口）

| 数据集 | 内容 | 币种 | 窗口 | 能否本地重跑 |
|---|---|---|---|---|
| `p1_pipeline_bookdiff_{holdout,selection_dev}.json` | 成交级分解（spr/drft/pex/tslp/net，**含成交后 markout**） | 仅 BTC/ETH @ core_hl | holdout w21/w23；sel w13-w16 | ❌ 只有产出 JSON，源 replay 需 lakehouse |
| `multivenue_three_tracks_{sel_w19,holdout_w24,holdout_w25}.json` | 静态盘口统计（`half_spread_minus_fee_bps`、`median_spread_bps`、`realized_1s_vol_bps`、`median_l1_min_usd`） | BTC/ETH + DRAM/NVDA/QQQ/SNDK/SPY | w19/w24/w25 | ✅ 可直接读取分析（已完成） |
| `recordings/2026-11-07-rwa/*.ndjson` | 原始 L2 订单簿（20 档 + oracle 参考价） | NVDA/TSLA/BTC/ETH | ~5 分钟片段 | ✅ 可算静态毛边 + 代理 markout（诊断级，非 10h） |
| `~/aleph-tx-lakehouse/` | bookdiff/pipeline 真实 replay 的源数据 | — | — | ❌ **本地不存在**（空目录） |

**关键缺口（本文最重要的单点）**：
- bookdiff 的真实成交级 replay 引擎依赖 `~/aleph-tx-lakehouse`，本地为空。`scripts/score_strategy_arms.py` 只是 **scorer**（吃已有 JSON 做分解/打分），**不是 replay 引擎**。
- 因此 **RWA 标的的"成交后真实 drift"在本地无法从头跑出来**。这是 RWA 结论的命门：静态 +1.4bp 很诱人，但 drift 是否把它吃光，当前**无实测**。
- Gemini 若有 lakehouse 访问权，应优先补这块（见第 6 节执行方案）。

---

## 3. 多币种 × 多窗口静态毛边（费后半价差，maker-0 场所）

数据源：三个 multivenue 窗口，`maker_fee_bps==0` 的场所（Arcus/Lighter/Lighter-RH/Extended/Bulk）。单位 bp，跨窗口取最差值看稳定性。

| 标的 @ 场所 | 域 | w19 | w24 | w25 | 均值 | 最差窗 | L1深度(w25,$) |
|---|---|---|---|---|---|---|---|
| DRAM @ arcus | rwa | 1.380 | 1.490 | 1.417 | 1.429 | **+1.380** | 3702 |
| NVDA @ lighter_rh | rwa | 0.747 | 1.019 | 0.982 | 0.916 | **+0.747** | 653 |
| ETH @ lighter_rh | crypt | 0.844 | 0.793 | 0.680 | 0.772 | **+0.680** | 1704 |
| SNDK @ arcus | rwa | 0.593 | 0.699 | 0.663 | 0.652 | **+0.593** | 2250 |
| BTC @ lighter_rh | crypt | 0.536 | 0.708 | 0.584 | 0.609 | **+0.536** | 500 |
| QQQ @ lighter | rwa | 0.478 | 0.572 | 0.554 | 0.535 | **+0.478** | 589 |
| NVDA @ lighter | rwa | 0.484 | 0.442 | 0.433 | 0.453 | **+0.433** | 396 |
| NVDA @ arcus | rwa | 0.368 | 0.341 | 0.331 | 0.347 | **+0.331** | 2237 |
| ETH @ extended | crypt | 0.290 | 0.304 | 0.295 | 0.296 | +0.290 | 44558 |
| BTC @ lighter | crypt | 0.269 | 0.318 | 0.297 | 0.295 | +0.269 | 2042 |
| BTC @ extended | crypt | 0.081 | 0.085 | 0.083 | 0.083 | +0.081 | 105138 |
| **BTC @ arcus** | crypt | 0.008 | 0.009 | 0.008 | 0.008 | +0.008 | 1149 |

**读数**：
- 最后一行是症结所在：**BTC @ Arcus 费后毛边 ≈ 0**。我们之前整个 S1 优化盯死 BTC/ETH，踩在最没肉的标的上。
- RWA（DRAM/SNDK/NVDA）毛边高一个数量级，三窗几乎不漂移（DRAM 稳定 1.4bp）。
- 深度对我们的 clip 规模（$500 本金、单 clip 远小于 L1）足够。
- 波动率分层（maker-0 场所池化，69 行）：vol<1.0 → +0.243bp；1.0-2.0 → +0.418bp；vol≥2.0 → +0.697bp。`spr/vol` 比值随波动下降（0.711→0.509），即价差变宽跟不上波动放大。

---

## 4. RWA 独立复算与命门（从原始 L2 订单簿，交叉验证）

为确认 multivenue 的正毛边不是生成数据的伪影，我从 `recordings/2026-11-07-rwa/nvda-strict.ndjson` 的原始 L2 订单簿（lighter NVDA 腿，750 个有效快照）**独立复算**：

| 指标 | 本地复算值 | 交叉对照 |
|---|---|---|
| NVDA@lighter 中位全价差 | 1.119 bp | — |
| NVDA@lighter 中位**半价差**（maker 费=0，即 hs-fee） | **0.559 bp** | multivenue 报 0.433~0.484bp（同量级，略高，窗口不同） |
| 代理 1s mid 波动幅度（逆向选择尺度） | 中位 0.666bp / 均值 0.946bp / p90 2.160bp | — |
| **half_spread / 1s_move 比值** | **0.840** | <1 = 典型 1 秒波动即可吃掉整个半价差 |
| \|mid − oracle\| 中位偏离 | **12.838 bp** | lighter NVDA 定价 vs 外部 oracle 的基差/滞后 |

**命门（必须写入结论，不能回避）**：
1. **静态毛边交叉验证通过**：独立算出 0.559bp，与 multivenue 同量级。正毛边是真的。
2. **但 half_spread(0.559) < 1s_move(0.666)，比值 0.84 < 1**：NVDA 的半价差撑不住典型 1 秒价格波动。这与 bookdiff 里 BTC/ETH 的"drift 吃毛边"是同一现象，在 NVDA 上**同样存在**。静态 +0.5bp 被动成交后很可能被 markout 吃光甚至吃穿。
3. **\|mid − oracle\| = 12.8bp 的大偏离**：该场所 RWA 定价显著偏离外部 oracle，被动挂单面临定价/滞后风险，需要用 oracle 做 fair-value 锚定来规避。

**诚实边界**：RWA recordings 仅 ~5 分钟片段，只能做诊断级交叉验证，**不构成 10 小时验证**。代理 markout 用 mid 变化近似，非真实成交 markout。RWA 的真实成交后 drift 必须用 lakehouse replay 补（第 6 节）。

**综合判断**：RWA 是唯一有正毛边、值得主攻的方向，但"盈利刷量"能否成立，取决于 RWA 的真实 drift 是否小于其更高的静态毛边。当前证据**不足以承诺盈利**，只足以支持"优先用真 replay 验证 RWA"。

---

## 5. 业界实践锚点（公开市场微结构知识；未经外部链接二次核对）

> 以下为成熟公开知识，供 Gemini 核对方向。涉及具体公式实现时应另查原始文献确认符号与常数。

### 5.1 Avellaneda-Stoikov（AS）库存做市模型
- **保留价（indifference price）**：`r = s − q·γ·σ²·(T−t)`。`s`=中价，`q`=当前库存（带符号），`γ`=库存风险厌恶，`σ`=波动率，`(T−t)`=到期剩余。库存越偏，报价中心越往反方向偏移（inventory skew），主动引导成交把库存拉回中性。
- **最优价差**：`δ_bid + δ_ask = γ·σ²·(T−t) + (2/γ)·ln(1 + γ/κ)`。`κ`=订单到达强度衰减。波动 σ 越大，最优价差越宽——与第 1.5 节"高波动应报更宽"一致。
- **对我们的映射**：现有 `basis_maker_hedge.rs` 的 fair EMA 相当于 `s` 的估计；缺的是 `q·γ·σ²` 的库存偏移项和随 σ 自适应的价差。这是把"静态毛边真正捕获到"的核心抓手。

### 5.2 降低逆向选择（被毒流 pick off）的实践手法
1. **库存偏移（inventory skew）**：库存偏多时压低双边报价中心，鼓励被 taker 买走多头、少被砸。AS 的 `q` 项即此。
2. **微价格 / 订单簿不平衡（microprice / OBI）**：用 `microprice = (bid·ask_sz + ask·bid_sz)/(bid_sz+ask_sz)` 替代 mid 做 fair，提前反映短期方向，减少挂在错边。
3. **高波动报更宽 / 毒性窗口撤单**：σ 突增或检测到大单穿越时拉宽或暂停补单（对应第 1.5 节 drift 随波动放大）。
4. **markout 感知报价偏移**：用历史成交 markout（成交后 N 秒的不利漂移）反向修正报价，markout 持续为负的那一边加价差或减尺寸。现有 S1_markout 已是此思路的最简版，本文建议强化为"负 markout 侧主动 skew + 毒性窗口暂停"。
5. **spread/vol 比值做标的与时段筛选**：只在 `median_spread/realized_vol` 高（宽价差、相对低波动）的标的/时段做市。第 3 节数据已给出每标的该比值。
6. **oracle 锚定**：当场所中价显著偏离外部 oracle（NVDA 的 12.8bp）时，用 oracle 校正 fair，避免挂在陈旧价上被套利。

### 5.3 "盈利刷量"在业界的两条路径
- **路径 A：返佣档**（maker rebate 为正且 > 逆向选择磨损）。对几千刀本金**已关死**（够不到 T5/T6）。
- **路径 B：正 ex-fee 毛边**（静态价差捕获 > drift + 费）。这是我们唯一能自控的路径，全部火力应集中于此。别人能做到负成本，要么有返佣（路径 A），要么把路径 B 的毛边做正——本文第 6 节即为路径 B 的执行方案。

---

## 6. 执行方案（交给 Gemini；按优先级排序，含验收标准）

> 约束：三轨 HOLD，纯离线 replay + 只读数据。可改回测脚本，**不动核心策略代码**（`src/strategy/*`、`src/exchanges/*`）直到回测给出正毛边证据。每步先过 `make test-python`。

### P0（最高优先）— 补 RWA 真实成交后 drift（命门）
- **目标**：在 lakehouse 可用的环境里，对 RWA 标的（DRAM/SNDK/NVDA @ Arcus/Lighter）跑与 BTC/ETH 相同的 bookdiff pipeline replay，产出含真实 `spr/drft/pex/tslp/net` 的分解。
- **前提**：需要 `~/aleph-tx-lakehouse` 的 RWA 窗口数据。**本地当前为空**；Gemini 须先确认 lakehouse 是否含 RWA 币种与窗口（`research_lead_lag_hybrid.py --lakehouse` 的加载路径、`ll.load_pair(con, lakehouse, win, pair)`）。若无，这是**数据采集任务**，不是回测任务——应登记到数据源 registry 并明确标为缺口，不得用静态毛边伪造 drift。
- **验收**：产出 `p1_pipeline_bookdiff_rwa_{selection,holdout}.json`，字段与现有 BTC/ETH bookdiff 对齐；每个 RWA 标的给出实测 `gross_bp` 与 `drft_bp`。
- **判定**：若 RWA 实测 `gross_bp > 0`（毛边扛住了 drift）→ 路径 B 成立，进入 P2 策略改造。若 `gross_bp ≤ 0` → RWA 与 BTC/ETH 同病，需回到 P3 重新找标的/时段。

### P1 — 把无返佣 $/M 成本固化进回测脚本（可独立于 lakehouse 完成）
- **现状**：`scripts/score_strategy_arms.py` 是 scorer，**无 `cost_per_million` 实现**；成本计算目前散落在一次性脚本里。
- **任务**：新增独立模块 `scripts/eval_volume_farming_cost.py`（或并入 scorer），实现并单测：
  ```
  cost_per_million(gross_bp, taker_exit_share_s, taker_bp, rebate_bp=0.0):
      return (taker_bp * s - rebate_bp * (2 - s) - gross_bp) / 2 * 100
  # 默认 rebate_bp=0（几千刀本金够不到档）；保留参数以备未来返佣资格。
  ```
  对每个 arm 输出分场所成本（Arcus ANT 1.12 / Arcus RWA 2.25 / HL 4.5），**所有排名带 USD/M 单位**，杜绝无单位 bp 排名。
- **数据源**：场所 taker 费率来自 `account_fee_evidence_20261123.json`（已审计档位），不要硬编码。
- **验收**：`make test-python` 新增用例通过；对现有 bookdiff holdout 复现本文第 1.2 节数字（S1_markout Arcus ANT ≈ +30.6 USD/M，s→0 地板 ≈ +20 USD/M）。

### P2 — 新增 markout-skew 回测 arm（仅在 P0 给出 RWA 正毛边后）
- **思路**（对应第 5.2 节手法 1/4）：在 S1_markout 基础上，加（a）负 markout 侧主动报价偏移（inventory/markout skew），（b）毒性窗口暂停补单，（c）oracle 锚定 fair（针对 NVDA 的 12.8bp 偏离）。
- **形式**：先在**回测脚本层**实现为新 arm 参数组（不改 `src/strategy`），用 selection 窗口调参，holdout 只验一次。
- **验收**：新 arm 在 RWA holdout 的实测 `gross_bp` 优于 S1_markout，且 `net_per_ntl_bp` 向 0 收敛或转正。冻结参数后再谈是否值得改核心策略代码。

### P3 — 标的与时段筛选器（支撑 P2）
- 用 `spr/vol` 比值 + L1 深度做标的白名单（第 3 节数据），输出"可做市标的×时段"表。
- 避开 `corr(vol, net)<0` 暗示的高波动时段（第 1.5 节）。
- **验收**：产出筛选器脚本 + 白名单 JSON，可被 P2 回测消费。

### 不做（明确边界）
- 不改 `src/strategy/*`、`src/exchanges/*` 直到 P0/P2 给出正毛边证据。
- 不碰返佣档假设（rebate_bp 默认 0）。
- 不把 ~5 分钟 recordings 的诊断结果当 10 小时验证。
- 不触发任何实盘下单/撤单/重启。

---

## 7. 复算命令（所有本文数字可复现）

```bash
cd /Users/metaverse/funspace/aleph-tx

# 第 1.2 / 1.1 节：BTC/ETH 无返佣 $/M 前沿（从 bookdiff holdout）
python3 - <<'PY'
import json
d=json.load(open('.agent-state/research/aleph_10h_real_eval_20261126/p1_pipeline_bookdiff_holdout.json'))['pipeline']
from collections import defaultdict
agg=defaultdict(lambda:{k:0. for k in('net','spr','drft','pex','tslp','ntl')})
for r in d:
    for k in agg[r['arm']]: agg[r['arm']][k]+=r[k]
for arm,a in sorted(agg.items()):
    g=(a['spr']+a['drft']+a['pex']+a['tslp'])/a['ntl']*1e4
    s=-(a['net']-(a['spr']+a['drft']+a['pex']+a['tslp']))/(3.5e-4*a['ntl'])
    for v,tk in[('ArcusANT',1.12),('ArcusRWA',2.25),('HL',4.5)]:
        print(f"{arm:14}{v:9} gross={g:+.3f} s={s*100:5.1f}% cost={ (tk*s-g)/2*100:7.2f} USD/M")
PY

# 第 3 节：多币种静态费后毛边（三窗口稳定性）
python3 - <<'PY'
import json
for t in('sel_w19','holdout_w24','holdout_w25'):
    d=json.load(open(f'.agent-state/research/multivenue_three_tracks_{t}_20261126.json'))
    for r in d['track1_quant_profit']:
        if r.get('maker_fee_bps')==0.0 and (r.get('half_spread_minus_fee_bps') or 0)>0:
            print(t, r['symbol'], str(r['venue'])[:14], f"{r['half_spread_minus_fee_bps']:+.3f}bp")
PY

# 第 4 节：NVDA 原始 L2 独立复算（半价差 + 代理 markout）
# 见 recordings/2026-11-07-rwa/nvda-strict.ndjson，legs[0]=lighter NVDA，取 bids[0]/asks[0]

# 回测脚本门禁
make test-python
```

---

## 8. 一页速览（给 Gemini）

- **别在 BTC/ETH @ HL 上刷量**：gross_bp 负，无返佣地板 ≈ +20 USD/M，结构性亏。
- **主攻 RWA（DRAM/SNDK/NVDA）+ 零 maker 费场所**：静态毛边 +0.3~1.4bp，跨三窗稳定。
- **命门**：RWA 的成交后 drift 无实测；代理指标（half_spread/1s_move=0.84<1）提示 drift 可能吃光毛边。**P0 必须先用 lakehouse replay 补这块**，否则一切是空中楼阁。
- **执行顺序**：P0 补 RWA drift → P1 固化 $/M 成本 → （若 RWA 正毛边）P2 markout-skew arm → P3 筛选器。
- **红线**：不动核心策略代码、不碰返佣假设、不把短片段当 10h、三轨 HOLD。

---

*本文为只读调研产物。核心策略代码在本轮未改动。回测脚本的新增/修改由 Gemini 按第 6 节执行并自测。*
