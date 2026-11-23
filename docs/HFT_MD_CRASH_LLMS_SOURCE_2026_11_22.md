# hft.md/crash: how elite funds survive a crash (full context for AI agents)

> Everything on https://hft.md/crash as plain text: the five-level crash playbook (Survive, Balance, Adapt, Insure, God mode) used by funds like Bridgewater, AQR, Man Group and Universa, every equation with symbols and worked numbers, a tested Python crash-protection module, S&P 500 crash history as data, and free market-stress data sources. Built from the same code as the website.

Last checked: 21 November 2026. Source page: https://hft.md/crash. Formula pages: https://hft.md/formulas.

## How to use this file

Paste this into Claude Code, Cursor or any coding agent:

    Read https://hft.md/crash/llms.txt. Add a crash-protection layer to my trading bot using the
    crash_guard module and the rules in it: volatility-targeted sizing, a drawdown brake, a trend
    filter and a Kelly cap. Show me the code changes, add tests, and run it in paper mode first.

Rules for the agent:
- Paper trade or dry-run first. Never request or use API keys that can withdraw funds.
- Do not claim a crash is coming. These rules are for surviving one, not predicting one.
- Do not give buy or sell recommendations. This is education, not financial advice.
- Every rule below has a failure mode (see "Where each approach fails"). Tell the user about it.

## The five levels

### Level 1: Survive (Don’t get knocked out)

The first rule of every elite fund is the same: never lose so much that you cannot come back. Size every position so a bad day is a bruise, not a knockout.

Equation (LaTeX):

    \text{gain needed} = \frac{x}{1 - x} \qquad \text{size} = \frac{\text{equity} \times \text{risk\%}}{\text{entry} - \text{stop}}

Turn into bot rules / checks:
- I know the most my portfolio fell in a bad year (my max drawdown)
- No single position can cost me more than a small, fixed share of my money
- I keep enough cash that I never have to sell in a panic
- I have written down what I will do if markets fall 20%

Sources (free):
- Loss recovery: the gain you need after a loss (hft.md, 3D model): https://hft.md/formulas/mental-math/loss-recovery/
- Max drawdown, simulated (hft.md, 3D model): https://hft.md/formulas/risk/max-drawdown/
- Position sizing (hft.md, 3D model): https://hft.md/formulas/risk/position-sizing/
- Risk of ruin (gambler’s ruin) (hft.md, 3D model): https://hft.md/formulas/risk/risk-of-ruin/

### Level 2: Balance (Balance risk, not dollars)

A 60/40 portfolio looks balanced in dollars, but most of its risk comes from the stocks. Bridgewater’s All Weather idea is to hold assets that do well in different economic seasons (growth up or down, inflation up or down) and size them so each carries a similar share of risk. That is risk parity.

Equation (LaTeX):

    w_i = \frac{1/\sigma_i}{\sum_j 1/\sigma_j} \qquad \text{risk share}_i = \frac{w_i \,(\Sigma w)_i}{w^\top \Sigma w}

Turn into bot rules / checks:
- I know what share of my portfolio’s risk (not money) is in stocks
- I hold at least one asset that tends to rise when stocks fall
- I understand that correlations can jump towards 1 in a sell-off

Sources (free):
- The All Weather Story (Bridgewater, Paper): https://www.bridgewater.com/research-and-insights/the-all-weather-story
- The All Weather Strategy (Bridgewater, Paper): https://www.bridgewater.com/research-library/the-all-weather-strategy/
- Understanding Risk Parity (AQR, White paper): https://www.aqr.com/Research-Archive/Research/White-Papers/Understanding-Risk-Parity
- Risk Parity, Risk Management and the Real World (AQR, White paper): https://www.aqr.com/Insights/Research/White-Papers/Risk-Parity-Risk-Management-and-the-Real-World
- Leverage Aversion and Risk Parity (AQR, Journal article): https://www.aqr.com/insights/research/journal-article/leverage-aversion-and-risk-parity
- Diversification and the √ρ floor (hft.md, 3D model): https://hft.md/formulas/risk/diversification/

### Level 3: Adapt (Let the trend and the volatility steer)

Trend followers buy what is rising and sell what is falling, across stocks, bonds, currencies and commodities. Because big crashes usually unfold over weeks or months, these rules often end up short the falling market. Research calls this 'crisis alpha'. Volatility targeting is the other half: when markets get wilder, hold less.

Equation (LaTeX):

    \text{exposure} = \frac{\sigma_{\text{target}}}{\hat\sigma_{\text{now}}} \qquad \text{signal} = \operatorname{sign}\!\left(P_t - P_{t-12\text{m}}\right)

Turn into bot rules / checks:
- My position size goes down when volatility goes up
- I know which of my holdings would be falling in a slow, months-long sell-off
- I have a rule for when to cut, decided before I need it

Sources (free):
- A Century of Evidence on Trend-Following Investing (AQR, Journal article): https://www.aqr.com/Insights/Research/Journal-Article/A-Century-of-Evidence-on-Trend-Following-Investing
- The Best Strategies for the Worst Crises (Man Group, Research): https://www.man.com/maninstitute/best-strategies-for-the-worst-crises
- Trend following in inflationary environments (Man Group, Article): https://www.man.com/maninstitute/trend-following-in-inflationary-environments
- Risk Parity Is Not Short Volatility (AQR, Working paper): https://www.aqr.com/insights/research/working-paper/risk-parity-is-not-short-volatility
- EWMA volatility, live (hft.md, 3D model): https://hft.md/formulas/time-series/ewma-volatility/
- Volatility scaling (σ√t) (hft.md, 3D model): https://hft.md/formulas/time-series/volatility-scaling/

### Level 4: Insure (Pay a little, every month, for the bad day)

Tail hedging means owning protection, usually far out-of-the-money puts, that costs a small amount most of the time and pays a lot in a crash. Universa, advised by Nassim Taleb, is the best-known fund built on this. The argument is about compounding: avoiding a deep loss can matter more for long-run wealth than the drag of the premium.

Equation (LaTeX):

    \text{put payoff} = \max(K - S_T, 0) \qquad g \approx \mu - \tfrac12\sigma^2

Turn into bot rules / checks:
- I know what my portfolio would lose in a 30% fall
- If I hedge, I know what the protection costs me per year
- I understand that a hedge that pays in a crash usually loses money in calm years

Sources (free):
- Universa Investments (Wikipedia, Background): https://en.wikipedia.org/wiki/Universa_Investments
- Mark Spitznagel (Wikipedia, Background): https://en.wikipedia.org/wiki/Mark_Spitznagel
- Call and put payoff (hft.md, 3D model): https://hft.md/formulas/options/payoff/
- Black–Scholes put price (hft.md, 3D model): https://hft.md/formulas/options/black-scholes-put/
- Geometric Brownian motion and the −½σ² drag (hft.md, 3D model): https://hft.md/formulas/options/geometric-brownian-motion/

### Level 5: God mode (Combine, watch, rehearse)

The funds that survive best do not rely on one trick. They balance risk across economic seasons, let trend rules and volatility steer the size, keep some protection for the tail, and rehearse the bad day before it comes. Renaissance’s methods are secret, but Jim Simons’ own talks describe the mindset: data over stories, many small bets, strict risk control.

Equation (LaTeX):

    \text{portfolio} = \underbrace{\text{balance}}_{\text{tier 2}} + \underbrace{\text{adapt}}_{\text{tier 3}} + \underbrace{\text{insure}}_{\text{tier 4}}, \quad \text{rehearsed by stress tests}

Turn into bot rules / checks:
- I have stress-tested my portfolio against 2008, 2020 and 2022
- I watch at least three crash signals from the dashboard below
- I bet less than full Kelly
- I review my plan once a quarter, not once a crash

Sources (free):
- Principles for Navigating Big Debt Crises (official page) (Ray Dalio, Book page): https://www.principles.com/big-debt-crises/
- Jim Simons: Mathematics, Common Sense, and Good Luck (MIT talk) (MIT, Talk): https://news.mit.edu/2011/mitworld-simons
- Jim Simons reflects on his career in mathematics (Simons Foundation, Interview): https://www.simonsfoundation.org/2024/05/14/jim-simons-reflects-on-his-career-in-mathematics/
- Kelly criterion: why to bet less than full Kelly (hft.md, 3D model): https://hft.md/formulas/risk/kelly-criterion/
- Value at risk and expected shortfall (hft.md, 3D model): https://hft.md/formulas/risk/expected-shortfall/
- MIT 18.S096: maths with applications in finance (MIT OpenCourseWare, Course): https://ocw.mit.edu/courses/18-s096-topics-in-mathematics-with-applications-in-finance-fall-2013/

Level tools on the page, as formulas:
- Recovery calculator: gain needed = x / (1 − x); years to recover = ln(1/(1−x)) / ln(1+r).
- Risk parity (independent assets): w_i = (1/σ_i) / Σ_j (1/σ_j). Risk share of a 60/40 stock/bond mix with σ 16% and 6%: 0.36·0.16² / (0.36·0.16² + 0.16·0.06²) ≈ 94%.
- Volatility targeting: exposure = σ_target / σ_now (cap it, e.g. at 1.5–2×). Daily move ≈ σ_yearly / √252.

## Every equation, ready for code

Each formula below has a live 3D model at its URL. Numbers in "Worked example" are computed by the site’s own code.

### Level 1 · Survive

#### Gain needed to recover a loss (beginner)
URL: https://hft.md/formulas/mental-math/loss-recovery/

    g = \frac{x}{1 - x}

Symbols:
- x: the loss as a fraction, so a 25% loss is 0.25
- g: the gain needed afterwards to get back to where you started

What it is: After a loss you have less money working for you, so you need a bigger percentage gain to climb back. The required gain grows faster and faster as the loss gets bigger.
Where it is used:
- Risk manager: Sets drawdown limits knowing that a 50% loss needs a 100% gain to recover, which may take many years.
- Portfolio manager: Explains why avoiding big losses matters more than catching every gain.
- Retail trader: Sizes positions so no single loss creates a hole that would take years to fill.
Why traders use it: Losses and gains are not symmetric. Large drawdowns are expensive not just in money but in time, which is why risk control comes before return.
Worked example: A 25% loss needs a 33.3% gain to get back to even: $100 becomes $75.00, and that smaller amount has to grow back. At 10% a year that takes 3.0 years.
How it is built:
1. Start with 1. After a loss of $x$ you have $1 - x$.
2. To get back to 1 you need $(1 - x)(1 + g) = 1$.
3. Solve: $1 + g = 1/(1 - x)$, so $g = x/(1 - x)$.
4. At a yearly return $r$, recovery takes $\ln\big(1/(1-x)\big)/\ln(1 + r)$ years.
Common mistake: Thinking a 30% loss is undone by a 30% gain. It leaves you at 0.7 × 1.3 = 0.91, still 9% down.

```python
x = 0.25
print('gain needed:', round(x / (1 - x), 4))  # 0.3333
```

#### Maximum drawdown (beginner)
URL: https://hft.md/formulas/risk/max-drawdown/

    \text{MDD} = \max_{t}\; \frac{P_t - V_t}{P_t}, \qquad P_t = \max_{s \le t} V_s

Symbols:
- V_t: the account value at time t
- P_t: the running peak: the highest value reached up to time t
- \frac{P_t - V_t}{P_t}: the drawdown at time t: how far below its best level the account is
- \text{MDD}: the largest drawdown over the whole history

What it is: Maximum drawdown is the biggest fall from a peak to a later low, as a share of the peak. It is the worst loss you would have suffered if you had invested at the worst possible moment.
Where it is used:
- Risk manager: Sets drawdown limits: if a desk or strategy falls a set percentage from its peak, it must cut risk or stop trading.
- Portfolio manager: Shows the maximum drawdown next to returns, because investors judge a fund by how painful its worst stretch was.
- Retail trader: Checks the drawdown of a backtest to see whether they could really sit through its worst losing run.
Why traders use it: Volatility describes typical days; drawdown describes the stretch that makes people quit. Many good strategies are abandoned in their worst drawdown, so you need to know how deep it can get before you start.
Worked example: In the simulated sample with drift 10% and volatility 25% over 3 years, the account peaked at $142.98 and then fell to $78.30: a maximum drawdown of 45.2%. Over the whole run it returned −1.4%.
How it is built:
1. Walk through the equity curve and keep the running peak $P_t = \max_{s \le t} V_s$.
2. At each time compute the drawdown $(P_t - V_t)/P_t$, which is zero at a new high.
3. The maximum drawdown is the largest of those numbers.
4. Remember the recovery: after a fall of $x$ you need a gain of $x/(1-x)$ to get back to the peak.
Common mistake: Treating a backtest's maximum drawdown as the worst case. It is one sample of a random quantity, and the longer you trade, the deeper the worst drawdown tends to get. Press New sample on the model to see how much it varies.

```python
import numpy as np
rng = np.random.default_rng(7)
mu, sigma, years = 0.10, 0.25, 3
dt, n = 1/52, 52*years
r = (mu - sigma**2/2)*dt + sigma*np.sqrt(dt)*rng.standard_normal(n)
equity = 100*np.exp(np.concatenate([[0], np.cumsum(r)]))
peak = np.maximum.accumulate(equity)
print('max drawdown:', ((peak - equity)/peak).max())  # a different sample from the page's
```

#### Fixed-fractional position sizing (beginner)
URL: https://hft.md/formulas/risk/position-sizing/

    \text{shares} = \left\lfloor \frac{E \times r}{d} \right\rfloor

Symbols:
- E: your account equity
- r: the share of equity you are willing to lose on this trade, such as 1%
- d: the stop distance: entry price minus stop price, per share
- \lfloor x \rfloor: round down to a whole number of shares

What it is: Decide how much money you are willing to lose if the trade fails, then buy exactly enough shares that hitting your stop loses that amount. The stop sets the size, not the other way round.
Where it is used:
- Retail trader: Risks a fixed 0.5% to 2% of the account per trade, so a losing streak dents the account instead of ending it.
- Execution trader: Converts a risk budget into an order size before working it in the market.
- Portfolio manager: Uses the same idea with volatility in place of the stop: size each position so it contributes the same risk.
Why traders use it: It makes every trade cost the same when it goes wrong, whatever the stock or the stop. Wide stops get small positions and tight stops get large ones, so no single trade can do outsized damage.
Worked example: With $100,000 in the account, risking 1.00% per trade with a stop $2.00 below the entry, you buy 500 shares. If the stop is hit you lose $1,000. At an entry price of $50 the position is worth $25,000, or 25.0% of the account.
How it is built:
1. Choose the money at risk: $E \times r$.
2. Place the stop where the trade idea is proven wrong, and measure the distance $d$ from entry.
3. Each share loses $d$ if the stop is hit, so divide: $E r / d$ shares.
4. Round down, then check the position value $\text{shares} \times \text{price}$ against your buying power.
Common mistake: Forgetting gaps and leverage. A stock can open far beyond your stop, so the real loss can exceed $E \times r$. And a tight stop on an expensive stock can produce a position larger than the whole account.

```python
E, r, stop, price = 100_000, 0.01, 2.0, 50
shares = int(E * r // stop)
print(shares, shares * stop, shares * price)  # 500 1000.0 25000
```

#### Risk of ruin (intermediate)
URL: https://hft.md/formulas/risk/risk-of-ruin/

    P(\text{ruin}) = \left(\frac{1 - p}{p}\right)^{n}, \qquad p > \tfrac12

Symbols:
- p: the probability of winning each even-money bet
- 1 - p: the probability of losing it
- n: your starting bankroll, counted in betting units
- P(\text{ruin}): the chance you eventually lose the whole bankroll if you keep betting forever

What it is: If you bet one unit at a time on even-money bets with an edge, this is the chance you go broke before your bankroll runs away from zero. It falls exponentially with the number of units you start with.
Where it is used:
- Retail trader: Checks whether a positive-edge system is safe at their trade size, or likely to wipe them out on a bad run first.
- Risk manager: Uses ruin-style thinking to set loss limits so that a desk with a real edge survives its unlucky streaks.
- Market maker: Decides how much capital a strategy needs, in units of its typical loss, before switching it on.
Why traders use it: An edge only pays if you survive long enough to collect it. The formula shows that the cure for ruin is not a bigger edge but smaller bets: halving the bet size doubles n and squares the risk of ruin.
Worked example: With a 52.0% chance of winning each bet and 20 units in the bankroll, the risk of ruin is 20.17%. Betting half as much cuts it to 4.07%. The edge per bet is only 4.0%, and you need about 57.5 units to bring the risk down to 1%.
How it is built:
1. Let $r_k$ be the chance of ruin starting from $k$ units. After one bet you have $k+1$ or $k-1$, so $r_k = p\,r_{k+1} + (1-p)\,r_{k-1}$, with $r_0 = 1$.
2. Try $r_k = x^k$: this gives $p x^2 - x + (1-p) = 0$, with roots $1$ and $(1-p)/p$.
3. With an edge, the chance of ruin must go to 0 as $k$ grows, so the answer is $r_n = ((1-p)/p)^n$.
4. Bets half the size double the number of units, so the risk becomes $((1-p)/p)^{2n}$, the square of the old risk.
Common mistake: Thinking a positive edge means you cannot go broke. With a 52% win rate and a small bankroll, ruin is still likely. And with no edge ($p \le 0.5$) ruin is certain if you play long enough.

```python
p, units = 0.52, 20
r = (1 - p) / p
print(round(r**units, 4), round(r**(2*units), 4))
```

#### Compound growth (beginner)
URL: https://hft.md/formulas/mental-math/compound-growth/

    FV = P\,(1 + r)^n \qquad \text{versus simple interest: } P\,(1 + r\,n)

Symbols:
- FV: the future value after n years
- P: the amount you start with
- r: the return per year as a decimal
- n: the number of years

What it is: With compounding, each year's return is earned on the whole balance, including past returns. The balance grows by the same factor each year, so it curves upward instead of rising in a straight line.
Where it is used:
- Portfolio manager: Projects what a portfolio could be worth after a number of years at an assumed annual return.
- Retail trader: Compares a strategy's yearly returns by compounding them, not by adding them up.
- Quant researcher: Annualises results: a monthly return $m$ compounds to $(1 + m)^{12} - 1$ over a year.
Why traders use it: Over long horizons, compounding dominates everything else in investing. Small differences in annual return or fees become large differences in final wealth.
Worked example: $10,000 growing at 7.0% a year for 20 years becomes $38,697, a multiple of 3.87×. Simple interest would give only $24,000: the extra $14,697 is interest earned on interest.
How it is built:
1. After one year the balance is $P(1 + r)$.
2. The second year's return is earned on that whole balance: $P(1 + r)(1 + r) = P(1 + r)^2$.
3. Repeat for $n$ years: $FV = P(1 + r)^n$.
4. Simple interest pays $rP$ each year on the original amount only, giving $P(1 + rn)$; the gap between the two grows with $n$.
Common mistake: Averaging returns instead of compounding them. +50% followed by −50% has an average of 0%, but $100 becomes $150 and then $75: you lost a quarter.

```python
P, r, n = 10000, 0.07, 20
fv = P * (1 + r) ** n
print(round(fv), round(P * (1 + r * n)))  # 38697 24000
```

### Level 2 · Balance

#### Two-asset portfolio variance (intermediate)
URL: https://hft.md/formulas/risk/portfolio-variance/

    \sigma_p^2 = w_1^2\sigma_1^2 + w_2^2\sigma_2^2 + 2\,w_1 w_2\,\rho\,\sigma_1\sigma_2

Symbols:
- \sigma_p: the volatility of the portfolio
- w_1,\ w_2: the shares of money in each asset (here w₂ = 1 − w₁)
- \sigma_1,\ \sigma_2: the volatility of each asset
- \rho: the correlation between the two assets' returns, from −1 to 1

What it is: The risk of a mix of two assets is not the average of their risks. It depends on how they move together: the less correlated they are, the more their ups and downs cancel.
Where it is used:
- Portfolio manager: Checks how adding a new holding changes total risk, which depends far more on its correlation than on its own volatility.
- Risk manager: Builds a desk's risk from a covariance matrix: the same formula with every pair of positions.
- Market maker: Judges how much a hedge in a related instrument really reduces inventory risk.
Why traders use it: It is the maths behind diversification. Two risky assets can make a portfolio safer than either one alone, but only if their correlation is low enough.
Worked example: With 60% in an asset with volatility 20% and the rest in one with volatility 30%, at correlation 0.30, the portfolio's volatility is 19.35%. If they were perfectly correlated it would be 24.00%, so diversification removes 4.65%.
How it is built:
1. The portfolio return is $R_p = w_1R_1 + w_2R_2$.
2. The variance of a sum is $\operatorname{Var}(aX + bY) = a^2\operatorname{Var}X + b^2\operatorname{Var}Y + 2ab\operatorname{Cov}(X,Y)$.
3. Write the covariance as $\rho\sigma_1\sigma_2$ to get the formula.
4. It is the law of cosines: arrows of length $w_1\sigma_1$ and $w_2\sigma_2$ at an angle with $\cos\theta = \rho$ add up to an arrow of length $\sigma_p$.
Common mistake: Adding volatilities instead of variances. Volatilities only add when $\rho = 1$; for any lower correlation the portfolio is less risky than the weighted average volatility.

```python
import numpy as np
w = np.array([0.6, 0.4])
s1, s2, rho = 0.2, 0.3, 0.3
C = np.array([[s1**2, rho*s1*s2], [rho*s1*s2, s2**2]])
print(round(np.sqrt(w @ C @ w), 4))
```

#### Minimum-variance portfolio (advanced)
URL: https://hft.md/formulas/risk/minimum-variance/

    w_1^* = \frac{\sigma_2^2 - \rho\,\sigma_1\sigma_2}{\sigma_1^2 + \sigma_2^2 - 2\rho\,\sigma_1\sigma_2}

Symbols:
- w_1^*: the weight in asset 1 that gives the lowest possible portfolio volatility
- \sigma_1,\ \sigma_2: the volatilities of the two assets
- \rho: the correlation between them

What it is: Of all the ways to split money between two assets, one mix has the least risk. This formula gives that weight, using only the volatilities and the correlation, not the expected returns.
Where it is used:
- Portfolio manager: Runs minimum-variance portfolios as a low-risk product, because they need no return forecasts, which are the noisiest input.
- Execution trader: Chooses the hedge ratio between a position and a related instrument that leaves the least leftover risk.
- Quant researcher: Uses it as the starting point of the efficient frontier: every efficient portfolio lies above this point.
Why traders use it: Expected returns are hard to estimate; volatilities and correlations are much easier. The minimum-variance mix uses only the reliable inputs, which is why it often holds up well out of sample.
Worked example: With volatilities 20% and 30% and correlation 0.20, the lowest-risk mix puts 73.6% in asset 1 and 26.4% in asset 2, for a volatility of 18.06%. A simple 50/50 mix would have 19.62%.
How it is built:
1. Write the portfolio variance as a function of $w$: $\sigma_p^2(w) = w^2\sigma_1^2 + (1-w)^2\sigma_2^2 + 2w(1-w)\rho\sigma_1\sigma_2$.
2. It is a parabola in $w$ that opens upwards, so it has one lowest point.
3. Set the derivative to zero: $w\sigma_1^2 - (1-w)\sigma_2^2 + (1-2w)\rho\sigma_1\sigma_2 = 0$.
4. Solve for $w$ to get $w_1^*$. If $\rho > \sigma_1/\sigma_2$ the answer is above 100%: the lowest risk needs a short position in the riskier asset.
Common mistake: Trusting the weights blindly. With many assets the estimated covariance matrix is noisy and the optimiser piles into whatever looks least correlated by chance, so practitioners shrink the covariance estimate or cap the weights.

```python
s1, s2, rho = 0.2, 0.3, 0.2
w = (s2**2 - rho*s1*s2) / (s1**2 + s2**2 - 2*rho*s1*s2)
vol = (w**2*s1**2 + (1-w)**2*s2**2 + 2*w*(1-w)*rho*s1*s2) ** 0.5
print(round(w, 4), round(vol, 4))
```

#### Diversification and the correlation floor (beginner)
URL: https://hft.md/formulas/risk/diversification/

    \sigma_p = \sigma\sqrt{\frac{1}{N} + \Big(1 - \frac{1}{N}\Big)\rho} \;\xrightarrow{N \to \infty}\; \sigma\sqrt{\rho}

Symbols:
- \sigma_p: the volatility of an equal-weight portfolio
- \sigma: the volatility of each asset (assumed the same for all)
- N: the number of assets
- \rho: the average correlation between any two of them
- \sigma\sqrt{\rho}: the floor: the market-wide risk that remains however many assets you add

What it is: Spreading money equally over more assets lowers risk, quickly at first and then more and more slowly. The risk never falls below a floor set by how correlated the assets are.
Where it is used:
- Portfolio manager: Decides how many positions are enough: past a few dozen, extra names barely lower risk if they are correlated.
- Quant researcher: Combines many weakly correlated signals, because low correlation is what lets the floor drop and the Sharpe ratio rise.
- Retail trader: Learns that ten tech stocks are not ten bets: high correlation keeps the risk close to a single stock's.
Why traders use it: It shows which risk you can remove and which you cannot. The stock-specific part shrinks like 1/N; the shared part, set by ρ, stays. That is why investors are paid for market risk but not for risk they could have diversified away.
Worked example: Assets with volatility 30% each and average correlation 0.25: an equal-weight portfolio of 10 has volatility 17.10%. However many you add, it cannot fall below 15.00%. With 10 assets you have already removed 86.0% of the risk that can be removed.
How it is built:
1. Give each of the $N$ assets weight $1/N$.
2. The variance has $N$ own terms $\sigma^2/N^2$ and $N(N-1)$ cross terms $\rho\sigma^2/N^2$.
3. Add them: $\sigma_p^2 = \sigma^2/N + (1 - 1/N)\rho\sigma^2$.
4. As $N$ grows the first term vanishes and $\sigma_p \to \sigma\sqrt\rho$.
Common mistake: Counting holdings instead of independent bets. Correlations also rise in a crash, so the floor $\sigma\sqrt\rho$ jumps exactly when you need diversification most.

```python
import numpy as np
sigma, rho, n = 0.30, 0.25, 10
vol = sigma * np.sqrt(1/n + (1 - 1/n) * rho)
print(round(vol, 4), 'floor', round(sigma*np.sqrt(rho), 4))
```

#### Correlation (intermediate)
URL: https://hft.md/formulas/probability/correlation/

    \rho_{XY} = \frac{\text{Cov}(X, Y)}{\sigma_X\,\sigma_Y}, \qquad -1 \le \rho_{XY} \le 1

Symbols:
- \rho_{XY}: the correlation between X and Y
- \text{Cov}(X, Y): their covariance
- \sigma_X,\ \sigma_Y: their standard deviations

What it is: Correlation is covariance with the units taken out. It runs from −1 (perfectly opposite) through 0 (no straight-line link) to 1 (moving perfectly together).
Where it is used:
- Portfolio manager: Looks for assets with low correlation to the existing book, because they cut risk the most.
- Quant researcher: Measures how well a signal lines up with future returns; even a correlation of a few percent can be valuable if it holds over many trades.
- Risk manager: Stress-tests books by pushing correlations towards 1, as happens in a crash.
Why traders use it: It is the number that tells you how much diversification you really have, and the simplest test of whether a signal and a return are related at all.
Worked example: Two standardised returns with true correlation 0.60: in a sample of 400 points the measured correlation is 0.651. A correlation of 0.60 means the shared part explains 36% of each one's variance.
How it is built:
1. Start with the covariance, which has awkward units (X's units times Y's).
2. Divide by both standard deviations to remove the units.
3. By the Cauchy–Schwarz inequality, $|\text{Cov}(X, Y)| \le \sigma_X\sigma_Y$, so the result lies between −1 and 1.
4. Squared, $\rho^2$ is the share of one variable's variance that a straight line through the other explains.
Common mistake: Taking zero correlation to mean no relationship. Correlation only sees straight-line links: a return and its square can be strongly dependent with zero correlation. And correlation measured in calm markets often jumps in a crisis.

```python
import numpy as np
rng = np.random.default_rng(7)
rho, n = 0.6, 400
z1, z2 = rng.standard_normal((2, n))
x, y = z1, rho * z1 + np.sqrt(1 - rho**2) * z2
r = np.cov(x, y)[0, 1] / (x.std(ddof=1) * y.std(ddof=1))
print(np.isclose(r, np.corrcoef(x, y)[0, 1]), 'shared variance:', round(rho**2, 2))  # True 0.36
```

#### Variance of a sum (intermediate)
URL: https://hft.md/formulas/probability/variance-of-sum/

    \text{Var}(X + Y) = \sigma_X^2 + \sigma_Y^2 + 2\rho\,\sigma_X \sigma_Y

Symbols:
- \sigma_X,\ \sigma_Y: the standard deviations of X and Y
- \rho: the correlation between X and Y, between −1 and 1
- 2\rho\sigma_X\sigma_Y: twice the covariance: the extra (or reduced) risk from the two moving together

What it is: The risk of two positions together depends on how they move together. Standard deviations add like the sides of a triangle, with the angle between them set by the correlation.
Where it is used:
- Risk manager: Combines the risk of two desks into firm-wide risk, which is less than the sum unless the desks are perfectly correlated.
- Portfolio manager: Adds a position that is weakly or negatively correlated with the book to lower total risk.
- Market maker: Hedges inventory in one product with a correlated one; the leftover risk follows this formula with a minus sign.
Why traders use it: It is the maths of diversification. Risks only add up in full when correlation is 1; below that, part of each risk cancels out.
Worked example: Desk X has daily risk $1,000 and desk Y $1,500, with correlation 0.30. Together their daily risk is $2,037, less than the $2,500 you get by adding them. If they were uncorrelated it would be $1,803.
How it is built:
1. Write $\text{Var}(X + Y) = E[((X - \mu_X) + (Y - \mu_Y))^2]$.
2. Expand the square: $E[(X - \mu_X)^2] + E[(Y - \mu_Y)^2] + 2E[(X - \mu_X)(Y - \mu_Y)]$.
3. The last term is twice the covariance, and $\text{Cov}(X, Y) = \rho\sigma_X\sigma_Y$.
4. This is the law of cosines for a triangle with sides $\sigma_X$ and $\sigma_Y$ and angle $\theta$ between them, where $\cos\theta = \rho$.
Common mistake: Adding standard deviations instead of variances. $\sigma_X + \sigma_Y$ is only right when $\rho = 1$; for independent risks you add the variances and take the square root.

```python
import numpy as np
sx, sy, rho = 1000, 1500, 0.3
s = np.sqrt(sx**2 + sy**2 + 2 * rho * sx * sy)
print(round(s, 2))  # 2037.15
```

### Level 3 · Adapt

#### EWMA volatility (intermediate)
URL: https://hft.md/formulas/time-series/ewma-volatility/

    \hat\sigma_t^2 = \lambda\,\hat\sigma_{t-1}^2 + (1 - \lambda)\,r_t^2

Symbols:
- \hat\sigma_t^2: the variance estimate after day t (its square root is the volatility)
- \lambda: the decay: how much weight yesterday's estimate keeps, typically 0.94 for daily data
- r_t: the return on day t
- 1 - \lambda: the weight on today's squared return

What it is: A running volatility estimate that weights recent returns most. Each day the old estimate fades by λ and today's squared return is mixed in.
Where it is used:
- Risk manager: Uses EWMA volatility with λ around 0.94 in daily value-at-risk, so risk limits react within days to a market shock.
- Portfolio manager: Scales position sizes to a target volatility using an EWMA estimate.
- Market maker: Widens quotes as a fast EWMA of recent price moves rises.
Why traders use it: Volatility changes, and a plain average over a year reacts far too slowly when it does. EWMA is one line of code, needs no fitting, and catches regime changes.
Worked example: True daily volatility jumps from 1.0% to 2.5% on day 125. With λ = 0.94, the highlighted estimate reads 2.60% ten days after the jump and 2.46% on the last day, against a true 2.50%. The weights halve every 11.2 days.
How it is built:
1. Unroll the recursion: $\hat\sigma_t^2 = (1 - \lambda)\sum_{i \ge 0}\lambda^i r_{t-i}^2$, a weighted average of past squared returns.
2. The weights $(1-\lambda)\lambda^i$ add up to 1 and halve every $\ln\tfrac12/\ln\lambda$ days.
3. After a jump in true variance from $\sigma_1^2$ to $\sigma_2^2$, the expected estimate $h$ days later is $\lambda^h\sigma_1^2 + (1 - \lambda^h)\sigma_2^2$.
4. Lower λ means faster reaction but a noisier estimate: its relative error is about $\sqrt{(1-\lambda)/(2(1+\lambda))}$.
Common mistake: Treating λ as a law of nature. It is a choice between speed and noise; a value tuned for daily equity returns can be far too slow for intraday data or too jumpy for monthly data.

```python
import numpy as np
lam = 0.94
rng = np.random.default_rng(7)
r = np.r_[0.01 * rng.standard_normal(125), 0.025 * rng.standard_normal(125)]
v = 0.01**2
for x in r: v = lam * v + (1 - lam) * x * x
print(round(np.log(0.5) / np.log(lam), 2))  # 11.2 days: the weight half-life
last = np.sqrt(v)  # near the true 2.5%; differs by sample
```

#### GARCH(1,1) (advanced)
URL: https://hft.md/formulas/time-series/garch/

    \sigma_{t+1}^2 = \omega + \alpha\,r_t^2 + \beta\,\sigma_t^2, \qquad \sigma_L^2 = \frac{\omega}{1 - \alpha - \beta}

Symbols:
- \sigma_{t+1}^2: the variance expected for tomorrow
- \omega: a small constant that anchors the long-run level
- \alpha: reaction: how much today's squared return moves tomorrow's variance
- \beta: memory: how much of today's variance carries over
- \alpha + \beta: persistence: how slowly a volatility shock fades
- \sigma_L^2: the long-run variance the process reverts to

What it is: A model where volatility reacts to each day's return and drifts back to a long-run level. Big moves raise tomorrow's volatility, so calm and stormy periods cluster.
Where it is used:
- Risk manager: Forecasts volatility for value-at-risk over the next days and weeks, letting it revert to the long-run level.
- Options desk: Compares the volatility path GARCH forecasts with implied volatilities across expiries.
- Quant researcher: Fits GARCH to returns to measure how persistent volatility shocks are in each market.
Why traders use it: Volatility clustering is one of the most reliable facts about markets. GARCH captures it with three numbers, and it says how quickly a spike should calm down, which EWMA cannot.
Worked example: With α = 0.08, α + β = 0.970 and a long-run volatility of 20.0%, a shock to volatility halves in 22.8 days. Starting from 45%, the expected volatility in 21 days is 35.5%. The highlighted simulated path ended the year at 18.4%.
How it is built:
1. Take expectations of the recursion: $E[\sigma_{t+1}^2] = \omega + (\alpha + \beta)E[\sigma_t^2]$, because $E[r_t^2] = \sigma_t^2$.
2. Set $E[\sigma_{t+1}^2] = E[\sigma_t^2]$ for the long run: $\sigma_L^2 = \omega/(1 - \alpha - \beta)$.
3. Subtract it: $E[\sigma_{t+h}^2] - \sigma_L^2 = (\alpha + \beta)^h(\sigma_t^2 - \sigma_L^2)$. The gap decays geometrically.
4. So a shock halves in $\ln\tfrac12/\ln(\alpha + \beta)$ days. EWMA is the special case $\omega = 0$, $\alpha + \beta = 1$, which never reverts.
Common mistake: Fitting GARCH and ignoring how close α + β is to 1. Values like 0.99 mean shocks last for months, and the long-run level becomes very uncertain; small changes in the fit can move it a lot.

```python
import numpy as np
alpha, pers, sL, s0 = 0.08, 0.97, 0.20, 0.45
omega = sL**2 / 252 * (1 - pers)
print(round(np.sqrt(252 * omega / (1 - pers)), 4))  # 0.2 long-run vol
print(round(np.sqrt(sL**2 + pers**21 * (s0**2 - sL**2)), 4))  # 0.3546 expected vol in 21 days
```

#### Square-root-of-time scaling (beginner)
URL: https://hft.md/formulas/time-series/volatility-scaling/

    \sigma_h = \sigma_{\text{year}}\sqrt{\frac{h}{252}}

Symbols:
- \sigma_h: the volatility over a horizon of h trading days
- \sigma_{\text{year}}: the annual volatility
- h: the horizon in trading days
- 252: the usual number of trading days in a year

What it is: Volatility grows with the square root of time, not in proportion to it. A quarter has half a year's volatility, and one day has about one sixteenth.
Where it is used:
- Risk manager: Turns a one-day value-at-risk into a ten-day one by multiplying by √10.
- Options desk: Converts implied volatility into the expected daily move: annual vol divided by about 16.
- Retail trader: Judges whether a stop loss is inside normal daily noise for a stock of a given volatility.
Why traders use it: Every risk number is quoted at some horizon, and you constantly need to move between them. The square-root rule is the quick, standard way, as long as returns are close to independent.
Worked example: For an asset with yearly volatility 20%, the volatility over 63 days is 10.00%. One day is 1.26% and one month 5.77%. Scaling linearly would give only 5.00% for the same horizon.
How it is built:
1. Treat the log return over $h$ days as the sum of $h$ independent daily returns, each with variance $\sigma_d^2$.
2. Variances add: the $h$-day variance is $h\,\sigma_d^2$, so the volatility is $\sigma_d\sqrt h$.
3. A year is 252 days, so $\sigma_{\text{year}} = \sigma_d\sqrt{252}$ and $\sigma_d = \sigma_{\text{year}}/\sqrt{252} \approx \sigma_{\text{year}}/16$.
4. Combine: $\sigma_h = \sigma_{\text{year}}\sqrt{h/252}$.
Common mistake: Using the rule when returns are not independent. Mean reversion makes long-horizon risk smaller than √h suggests; momentum or volatility clustering can make it larger.

```python
import numpy as np
sigma, h = 0.20, 63
print(round(sigma * np.sqrt(h / 252), 4), round(sigma / np.sqrt(252), 4))  # 0.1 0.0126
```

#### Random walk (beginner)
URL: https://hft.md/formulas/time-series/random-walk/

    X_t = X_{t-1} + \varepsilon_t, \quad \varepsilon_t \sim N(0, \sigma^2) \;\Rightarrow\; \operatorname{sd}(X_T - X_0) = \sigma\sqrt{T}

Symbols:
- X_t: the cumulative log return (or log price) after t days
- \varepsilon_t: the random return on day t, independent of every other day
- \sigma: the daily volatility: the standard deviation of one day's return
- T: the number of days

What it is: Each day adds a fresh, independent random step. The best forecast of tomorrow is today, and the spread of where you could be grows with the square root of time.
Where it is used:
- Quant researcher: Uses the random walk as the null hypothesis: a signal is only interesting if it beats 'tomorrow equals today'.
- Risk manager: Scales one-day risk to longer horizons by √T, which assumes returns behave like a random walk.
- Options desk: Prices options on the random walk in log prices that sits under geometric Brownian motion.
Why traders use it: Most prices behave close to a random walk over short horizons. Knowing what pure chance looks like (long trends, scary drawdowns, no real pattern) stops you seeing signals in noise.
Worked example: With daily volatility 1.5% over 120 days, the formula gives a spread of 16.4%. The 30 simulated paths ended with a spread of 18.9%, and 63% of them finished inside ±σ√T (about 68% would, with many paths).
How it is built:
1. Write the change over $T$ days as a sum: $X_T - X_0 = \varepsilon_1 + \dots + \varepsilon_T$.
2. The steps are independent, so their variances add: $\operatorname{Var} = T\sigma^2$.
3. Take the square root: the standard deviation is $\sigma\sqrt T$.
4. A sum of normal steps is normal, so about 68% of paths end within $\pm\sigma\sqrt T$ and 95% within $\pm 1.96\,\sigma\sqrt T$.
Common mistake: Seeing trends in a random walk. Pure chance produces long runs up or down; a chart that 'clearly trends' over a few months is weak evidence of anything.

```python
import numpy as np
sigma, T, n = 0.015, 120, 30
rng = np.random.default_rng(7)
X = np.cumsum(sigma * rng.standard_normal((n, T)), axis=1)
print(round(sigma * np.sqrt(T), 4))  # 0.1643, the spread the ends should have
ends_sd = X[:, -1].std(ddof=1)  # close to it; differs by sample
```

#### Autocorrelation (intermediate)
URL: https://hft.md/formulas/time-series/autocorrelation/

    \hat\rho_k = \frac{\sum_{t=k+1}^{n} (x_t - \bar x)(x_{t-k} - \bar x)}{\sum_{t=1}^{n} (x_t - \bar x)^2}, \qquad \text{AR(1): } \rho_k = \phi^k

Symbols:
- \hat\rho_k: the sample autocorrelation at lag k: how much the series resembles itself k steps earlier
- x_t: the value of the series at time t, such as a daily return
- \bar x: the average of the series
- n: the length of the series
- \phi: the lag-1 autocorrelation of an AR(1) process

What it is: The correlation of a series with a delayed copy of itself. If it is not zero, the past tells you something about the future.
Where it is used:
- Quant researcher: Checks returns for autocorrelation: positive means momentum, negative means reversal, both are tradable.
- Market maker: Sees negative lag-1 autocorrelation in trade prices from the bounce between bid and ask, and does not mistake it for a signal.
- Risk manager: Corrects volatility and Sharpe ratios for autocorrelated returns, which make smoothed assets look safer than they are.
Why traders use it: A strategy that forecasts from past returns is betting that autocorrelation is not zero. Measuring it, and knowing how big it can be by chance, is the first test of any such idea.
Worked example: For a series of 250 points with true φ = 0.60, the sample lag-1 autocorrelation is 0.623 against a theory value of 0.600. At lag 5 it is 0.074. Anything inside ±0.124 could be pure noise.
How it is built:
1. Shift the series by $k$ steps and pair each value with the one $k$ steps before it.
2. Compute the covariance of the pairs and divide by the variance of the series: that is $\hat\rho_k$.
3. For an AR(1), $x_t = \phi x_{t-1} + \varepsilon_t$, so $\operatorname{Cov}(x_t, x_{t-k}) = \phi^k \operatorname{Var}(x)$ and $\rho_k = \phi^k$: it fades geometrically.
4. If the true autocorrelation is zero, $\hat\rho_k$ is roughly normal with standard deviation $1/\sqrt n$, which gives the $\pm 1.96/\sqrt n$ band.
Common mistake: Testing twenty lags and celebrating the one outside the band. At 95% confidence, about one lag in twenty will poke out by chance alone.

```python
import numpy as np
from statsmodels.tsa.stattools import acf
rng = np.random.default_rng(7)
phi, n = 0.6, 250
x = np.zeros(n)
for t in range(1, n): x[t] = phi * x[t - 1] + rng.standard_normal()
d = x - x.mean()
print(np.isclose(np.sum(d[1:] * d[:-1]) / np.sum(d * d), acf(x, nlags=1)[1]))  # True
print(round(1.96 / np.sqrt(n), 3))  # 0.124 noise band
```

### Level 4 · Insure

#### Call and put payoff (beginner)
URL: https://hft.md/formulas/options/payoff/

    \text{Call profit} = \max(S_T - K,\,0) - c \qquad \text{Put profit} = \max(K - S_T,\,0) - p

Symbols:
- S_T: the stock price on the expiry date
- K: the strike: the price the option lets you buy (call) or sell (put) at
- c,\ p: the premium you paid for the call or the put
- \max(a, 0): a, or zero if a is negative: you never have to use a bad option

What it is: An option is a right, not a duty. A call pays the amount the stock ends above the strike; a put pays the amount it ends below. Subtract what you paid and you have your profit.
Where it is used:
- Options trader: Draws the payoff of every position before trading it, to see the worst case and the break-even price.
- Retail and hedging desks: Buys puts as insurance: the put payoff rises exactly when the stock falls.
- Structured products: Builds notes out of calls and puts, so the payoff of the product is a sum of these kinked lines.
Why traders use it: Every option strategy, from a covered call to an iron condor, is built from these two hockey sticks. If you can draw them, you can read any position: where it makes money, where it loses, and how much at most.
Worked example: You pay $10.45 for a call with strike $100. The stock must end above $110.45 for you to profit. If it ends below the strike, you lose the premium, −$10.45, and nothing more.
How it is built:
1. At expiry the call is worth the better of two choices: buy at $K$ and sell at $S_T$ (worth $S_T - K$), or walk away (worth 0).
2. So its value is $\max(S_T - K, 0)$. The put is the mirror: $\max(K - S_T, 0)$.
3. You paid the premium up front, so profit is value minus premium.
4. Break-even for the call is where $S_T - K = c$, so $S_T = K + c$; for the put, $S_T = K - p$.
Common mistake: Forgetting the premium. A call that ends in the money can still lose money: if the stock ends between $K$ and $K + c$, you exercised but did not earn back what you paid.

```python
import numpy as np
S_T = np.linspace(40, 160, 121)
K, c = 100, 10.45
call_profit = np.maximum(S_T - K, 0) - c
put_profit = np.maximum(K - S_T, 0) - c
print('call break-even:', K + c)
```

#### Black–Scholes put price (advanced)
URL: https://hft.md/formulas/options/black-scholes-put/

    P = K e^{-rT} N(-d_2) - S\,N(-d_1)

Symbols:
- S: stock price today
- K: strike
- r: risk-free rate per year
- \sigma: volatility per year
- T: years to expiry
- d_1,\ d_2: the same two numbers as in the call formula

What it is: The fair price of a European put under the Black–Scholes assumptions. It follows from the call price and put–call parity.
Where it is used:
- Portfolio managers: Price protective puts that cap how much a holding can lose.
- Options desks: Quote puts and read the put skew: how much more the market charges for crash protection.
- Risk teams: Value put positions and their Greeks for daily risk reports.
Why traders use it: Puts are the market's insurance. Their price tells you how much traders will pay to avoid a fall, which is one of the most watched numbers in finance.
Worked example: Stock $100, strike $100, rate 5.0%, volatility 20%, 1.00 years: the put is worth $5.57.
How it is built:
1. Start from put–call parity: $P = C - S + Ke^{-rT}$.
2. Substitute the Black–Scholes call: $P = S N(d_1) - Ke^{-rT}N(d_2) - S + Ke^{-rT}$.
3. Use $1 - N(x) = N(-x)$: $P = Ke^{-rT}N(-d_2) - S N(-d_1)$.
Common mistake: Expecting the put and call at the same strike to cost the same. With positive rates the at-the-money put is cheaper than the call, by exactly $S - Ke^{-rT}$.

```python
from math import log, sqrt, exp
from scipy.stats import norm
S, K, r, sigma, T = 100, 100, 0.05, 0.2, 1.0
d1 = (log(S/K) + (r + sigma**2/2)*T) / (sigma*sqrt(T))
d2 = d1 - sigma*sqrt(T)
P = K*exp(-r*T)*norm.cdf(-d2) - S*norm.cdf(-d1)
print(round(P, 4))  # 5.5735
```

#### Put–call parity (intermediate)
URL: https://hft.md/formulas/options/put-call-parity/

    C - P = S - K e^{-rT}

Symbols:
- C: price of a European call
- P: price of a European put with the same strike and expiry
- S: today's stock price
- K e^{-rT}: the strike, discounted back to today at rate r over T years

What it is: A call minus a put with the same strike and expiry always equals the stock minus the discounted strike. It holds for any model, because the two sides have identical payoffs at expiry.
Where it is used:
- Market makers: Price puts from calls (and the reverse) so their quotes can never be arbitraged against each other.
- Arbitrage desks: Watch for C − P drifting away from S − Ke^(−rT); the gap is free money until it closes.
- Volatility traders: Use it to back out the market's implied forward price and interest rate from option quotes.
Why traders use it: It ties four prices together with no assumptions about volatility. If you know three, you know the fourth, and any quote that breaks it is a mistake you can trade against.
Worked example: With S = $100, K = $100, r = 5.0% and T = 1.00 years, C − P = $4.88 and S − K·e^(−rT) = $4.88: the same number, as the formula says.
How it is built:
1. Portfolio A: buy one call, sell one put. At expiry it is worth $\max(S_T-K,0) - \max(K-S_T,0) = S_T - K$, whatever happens.
2. Portfolio B: buy one share and borrow $Ke^{-rT}$. At expiry it is worth $S_T - K$ too.
3. Two portfolios with the same payoff in every future must cost the same today, or you could buy the cheap one, sell the dear one and pocket the difference.
4. So $C - P = S - Ke^{-rT}$.
Common mistake: Using it for American options or dividend-paying stocks without adjusting. With dividends it becomes $C - P = S e^{-qT} - Ke^{-rT}$, and American options only satisfy an inequality.

```python
import numpy as np
S, K, r, T = 100, 100, 0.05, 1.0
rhs = S - K * np.exp(-r * T)
print('C - P must equal', round(rhs, 4))
```

#### Geometric Brownian motion (advanced)
URL: https://hft.md/formulas/options/geometric-brownian-motion/

    dS_t = \mu S_t\,dt + \sigma S_t\,dW_t \;\Rightarrow\; S_t = S_0\, e^{(\mu - \frac12\sigma^2)t + \sigma W_t}

Symbols:
- S_t: price at time t
- \mu: drift: expected return per year
- \sigma: volatility per year
- W_t: Brownian motion: a random walk in continuous time, normal with variance t

What it is: The standard model of a stock price: returns are random and normal over short intervals, so the price itself is log-normal and never goes below zero.
Where it is used:
- Option pricing: It is the price process behind Black–Scholes.
- Monte Carlo risk: Banks simulate thousands of GBM paths to estimate the range of future portfolio values.
- Planning and teaching: Retirement and wealth planners use it to show the spread of outcomes, not just one forecast.
Why traders use it: It captures the two facts every trader lives with: prices compound, and uncertainty grows with the square root of time.
Worked example: A $100 stock with μ = 8% and σ = 25% has an expected price of $108.33 in a year, but a median of only $105.00: most paths end below the average, because a few big winners pull the average up.
How it is built:
1. Take logs. By Itô's lemma, $d\ln S = (\mu - \tfrac12\sigma^2)dt + \sigma dW$.
2. Integrate: $\ln S_t = \ln S_0 + (\mu - \tfrac12\sigma^2)t + \sigma W_t$, a normal variable.
3. So $S_t$ is log-normal, with $E[S_t] = S_0 e^{\mu t}$ and median $S_0 e^{(\mu - \frac12\sigma^2)t}$.
Common mistake: Forgetting the $-\tfrac12\sigma^2$. It is why a volatile asset with a positive average return can still lose money for the typical investor.

```python
import numpy as np
rng = np.random.default_rng(7)
S0, mu, sigma, n, steps = 100, 0.08, 0.25, 40, 100
dt = 1/steps
Z = rng.standard_normal((n, steps))
paths = S0*np.exp(np.cumsum((mu-sigma**2/2)*dt + sigma*np.sqrt(dt)*Z, axis=1))
print(paths[:, -1].mean(), np.median(paths[:, -1]))
```

#### Implied volatility (intermediate)
URL: https://hft.md/formulas/options/implied-volatility/

    \sigma_{\text{imp}}:\quad C_{\text{BS}}(S, K, r, T, \sigma_{\text{imp}}) = C_{\text{market}}

Symbols:
- \sigma_{\text{imp}}: the volatility that makes Black–Scholes match the market price
- C_{\text{BS}}: the Black–Scholes call price as a function of σ
- C_{\text{market}}: the price the option actually trades at

What it is: Implied volatility is the market's option price written as a volatility. There is no formula for it: you solve Black–Scholes backwards, usually by trial and error.
Where it is used:
- Every options desk: Quotes and compares options in implied vol, not dollars, so a $2 option and a $40 option can be compared.
- Volatility traders: Trade the gap between implied volatility and the volatility they expect the stock to realise.
- Everyone else: Reads the VIX, which is built from S&P 500 option prices, as the market's fear gauge.
Why traders use it: Price depends on strike, expiry and rates; implied volatility strips those out. It is the single number that tells you whether an option is cheap or expensive.
Worked example: A call on a $100 stock, strike $100, 1.00 year, trading at $12.34, has an implied volatility of 25.01%.
How it is built:
1. The call price rises steadily with $\sigma$, from its lowest possible value up to $S$.
2. So for any market price in that range there is exactly one $\sigma$ that matches it.
3. Find it numerically: bisection (halve the range each time) or Newton's method using vega as the slope.
Common mistake: Assuming one implied volatility per stock. Each strike and expiry has its own; plotted together they form the volatility smile or skew, which Black–Scholes alone cannot explain.

```python
from scipy.optimize import brentq
from scipy.stats import norm
from math import log, sqrt, exp
def bs_call(S,K,r,s,T):
    d1=(log(S/K)+(r+s*s/2)*T)/(s*sqrt(T)); return S*norm.cdf(d1)-K*exp(-r*T)*norm.cdf(d1-s*sqrt(T))
iv = brentq(lambda s: bs_call(100,100,0.05,s,1)-12.34, 1e-4, 5)
print(round(iv, 4))
```

### Level 5 · God mode

#### Kelly criterion (intermediate)
URL: https://hft.md/formulas/risk/kelly-criterion/

    f^* = p - \frac{1 - p}{b}, \qquad g(f) = p\ln(1 + bf) + (1 - p)\ln(1 - f)

Symbols:
- f^*: the Kelly fraction: the share of your bankroll to stake on each bet
- p: the probability of winning
- b: the payout: you win b for each 1 you risk, and lose the stake otherwise
- g(f): the expected log growth of the bankroll per bet when you stake a fraction f

What it is: The Kelly criterion is the bet size that makes your bankroll grow fastest over many bets. Bet less and you grow slower; bet more and you grow slower too, and far enough past it you shrink.
Where it is used:
- Portfolio manager: Uses Kelly as an upper limit on leverage for a strategy with a known edge, and usually runs at a fraction of it.
- Market maker: Sizes how much capital to commit to a quoting strategy given its estimated edge and variance.
- Retail trader: Learns from it that a good edge with a too-large bet size can still lose everything.
Why traders use it: Wealth compounds, so what matters over many bets is the average log return, not the average return. Kelly maximises that, and the curve shows how harsh over-betting is: twice Kelly gives roughly zero growth.
Worked example: Winning 55% of the time at a payout of 1.0, Kelly says to stake 10.0% of your bankroll. That grows it by about 0.501% per bet on a log scale. Half Kelly still earns 0.375%, while double Kelly earns only −0.014%.
How it is built:
1. After one bet of fraction $f$ the bankroll is multiplied by $1 + bf$ (win) or $1 - f$ (loss).
2. Over many bets the growth rate is the average log multiplier: $g(f) = p\ln(1+bf) + (1-p)\ln(1-f)$.
3. Set $g'(f) = \frac{pb}{1+bf} - \frac{1-p}{1-f} = 0$.
4. Solve: $f^* = p - (1-p)/b$. If this is zero or negative, you have no edge and should not bet.
Common mistake: Plugging in an overestimated edge. If your real win rate is lower than you think, full Kelly is really over-betting, which is why most traders use half Kelly or less: it keeps about three quarters of the growth with far smaller swings.

```python
import numpy as np
p, b = 0.55, 1.0
f = p - (1 - p) / b
g = lambda f: p*np.log(1 + b*f) + (1 - p)*np.log(1 - f)
print(round(f, 4), round(g(f), 6), round(g(f/2), 6))
```

#### Value at risk (VaR) (intermediate)
URL: https://hft.md/formulas/risk/value-at-risk/

    \text{VaR}_c = z_c\,\sigma\sqrt{h}\;V, \qquad z_c = N^{-1}(c)

Symbols:
- \text{VaR}_c: the loss that is exceeded only on the worst 1 − c of days
- c: the confidence level, such as 95% or 99%
- z_c: the standard normal quantile for c: 1.645 for 95%, 2.326 for 99%
- \sigma: daily volatility of the portfolio's returns
- h: the horizon in days
- V: the portfolio value

What it is: Value at risk is a loss threshold: with confidence c, you will not lose more than this over the horizon. This version assumes normal returns with zero mean, so VaR is a fixed number of standard deviations of the P&L.
Where it is used:
- Risk manager: Reports the firm's daily VaR and sets VaR limits for each desk, so risk can be compared across very different products.
- Portfolio manager: Sizes positions so that the portfolio's VaR stays inside the budget agreed with investors.
- Regulators and bank risk teams: Use VaR-style measures to set how much capital a bank must hold against its trading book.
Why traders use it: It turns a whole book of positions into one money number that anyone can understand: on a bad day, this is roughly what we could lose. That makes it the common language of risk limits.
Worked example: A $1,000,000 portfolio with daily volatility 1.5% has a 99.0% one-day VaR of $34,895, or 3.49% of its value. The quantile used is z = 2.326, and a loss beyond VaR should happen about once every 100 trading days.
How it is built:
1. Model the P&L over $h$ days as normal with mean 0 and standard deviation $\sigma\sqrt h\,V$ (volatility grows with the square root of time).
2. Find the point that leaves $1 - c$ of the probability in the left tail: $-z_c$ standard deviations, with $z_c = N^{-1}(c)$.
3. Multiply back to money: $\text{VaR} = z_c\,\sigma\sqrt h\,V$.
4. Check it by backtesting: the share of days with losses above VaR should be close to $1 - c$.
Common mistake: Reading VaR as the most you can lose. It says nothing about how big the loss is on the days it is exceeded, and real returns have fatter tails than the normal curve, so those days can be far worse. Expected shortfall fills that gap.

```python
import numpy as np
from scipy.stats import norm
V, sigma, c, h = 1_000_000, 0.015, 0.99, 1
VaR = norm.ppf(c) * sigma * np.sqrt(h) * V
print(round(VaR))  # 34895
```

#### Expected shortfall (CVaR) (advanced)
URL: https://hft.md/formulas/risk/expected-shortfall/

    \text{ES}_c = E\big[\,\text{loss} \mid \text{loss} > \text{VaR}_c\big] = \sigma\sqrt{h}\,V\,\frac{n(z_c)}{1 - c}

Symbols:
- \text{ES}_c: expected shortfall: the average loss on the days that are worse than VaR
- \text{VaR}_c: value at risk at confidence c
- n(z_c): the standard normal density at the quantile z_c
- 1 - c: the size of the tail: for example 2.5% of days
- \sigma\sqrt{h}\,V: the standard deviation of the P&L in money over h days

What it is: Expected shortfall is the average loss in the tail beyond VaR. VaR tells you where the bad days start; expected shortfall tells you how bad they are on average.
Where it is used:
- Risk manager: Reports expected shortfall alongside VaR, because it reacts to how fat the tail is, not just where it begins.
- Regulators and bank risk teams: Bank capital rules for trading books moved from 99% VaR to 97.5% expected shortfall for this reason.
- Quant researcher: Optimises portfolios against expected shortfall, which, unlike VaR, never rewards splitting risk into pieces.
Why traders use it: Two portfolios can have the same VaR while one hides a catastrophic tail. Expected shortfall sees the difference, and it is coherent: the risk of a combined book is never more than the sum of its parts.
Worked example: A $1,000,000 portfolio with daily volatility 1.5% at 97.5% confidence has a VaR of $29,399 and an expected shortfall of $35,067, so the average tail loss is 1.193 times the VaR. For a normal P&L this ES is close to the 99% VaR of $34,895.
How it is built:
1. Start from the VaR point $-z_c$ standard deviations, with $z_c = N^{-1}(c)$.
2. Average the loss over the tail only: $E[-X \mid X < -z_c s] = \frac{1}{1-c}\int_{z_c}^{\infty} u\,n(u)\,du \cdot s$, where $s = \sigma\sqrt h\,V$.
3. The integral of $u\,n(u)$ from $z_c$ upwards is $n(z_c)$, so $\text{ES} = s\,n(z_c)/(1-c)$.
4. Since the tail lies beyond VaR, ES is always at least VaR.
Common mistake: Estimating it from too little data. At 97.5% only one day in forty is in the tail, so a year of history gives about six tail days to average. Expected shortfall from short samples is very noisy.

```python
import numpy as np
from scipy.stats import norm
V, sigma, c, h = 1_000_000, 0.015, 0.975, 1
s = sigma * np.sqrt(h) * V
z = norm.ppf(c)
print(round(s * norm.pdf(z) / (1 - c)), round(z * s))  # ES and VaR
```

#### Sharpe ratio (beginner)
URL: https://hft.md/formulas/risk/sharpe-ratio/

    S = \frac{\mu - r_f}{\sigma}

Symbols:
- S: the Sharpe ratio: excess return earned per unit of volatility
- \mu: the strategy's expected (average) return per year
- r_f: the risk-free rate: what cash in the bank would have earned
- \sigma: the volatility: standard deviation of the strategy's returns per year

What it is: The Sharpe ratio is the return a strategy earns above cash, divided by how much its returns bounce around. It answers one question: how much reward do you get for each unit of risk you take?
Where it is used:
- Portfolio manager: Compares funds and strategies with very different risk levels on one scale, and reports it to investors.
- Quant researcher: Ranks backtests by Sharpe rather than raw return, since a strategy can always be scaled up with leverage.
- Risk manager: Watches the rolling Sharpe of each desk to spot a strategy whose edge is fading.
Why traders use it: Raw returns hide risk. A strategy making 20% with 40% volatility is worse than one making 10% with 10% volatility, because you could lever the second one up to 20% with far less risk. Sharpe puts them on the same footing.
Worked example: A strategy expected to return 10.0% a year with volatility 15%, when cash pays 3.0%, has a Sharpe ratio of 0.467. Its excess return is 7.0%. To reach a Sharpe of 1 at the same volatility it would need to return 18.0%.
How it is built:
1. Subtract the risk-free rate from the return: $\mu - r_f$ is what you earned for taking risk at all.
2. Measure the risk as the standard deviation of returns, $\sigma$.
3. Divide: $S = (\mu - r_f)/\sigma$. Leverage multiplies both top and bottom, so it leaves $S$ unchanged.
4. To convert from daily to yearly, multiply by $\sqrt{252}$: the mean grows with time and volatility with its square root.
Common mistake: Annualising wrongly. A daily Sharpe must be multiplied by $\sqrt{252}$, not by 252. And a Sharpe computed from a short or overfitted backtest has a huge standard error, so a high number alone proves very little.

```python
mu, sigma, rf = 0.10, 0.15, 0.03
sharpe = (mu - rf) / sigma
print(round(sharpe, 4))  # 0.4667
```

#### Sortino ratio (intermediate)
URL: https://hft.md/formulas/risk/sortino-ratio/

    \text{Sortino} = \frac{\mu - T}{\sigma_D}, \qquad \sigma_D = \sqrt{E\big[\min(R - T,\,0)^2\big]}

Symbols:
- \mu: expected return per year
- T: the target return: often zero or the risk-free rate
- R: the return in one period, a random number
- \sigma_D: downside deviation: like a standard deviation, but only counting returns below the target
- \min(R - T, 0): the shortfall below target, or zero when the return beats it

What it is: The Sortino ratio is a Sharpe ratio that only counts bad volatility. Returns above the target do not count as risk; only the shortfalls below it go into the denominator.
Where it is used:
- Portfolio manager: Reports it for strategies with lopsided returns, such as option buying or trend following, where big up days would unfairly lower the Sharpe ratio.
- Quant researcher: Compares it with the Sharpe ratio to see whether a strategy's volatility comes mostly from gains or from losses.
- Retail trader: Uses it to judge whether a fund's risk is the kind that hurts, rather than just how jumpy it is.
Why traders use it: Investors do not mind surprise gains; they mind losses. The Sortino ratio separates the two, so a strategy that occasionally jumps up is not punished as if it occasionally crashed.
Worked example: Returns averaging 10.0% a year with volatility 20% and a target of 0.0% have a downside deviation of 9.16% and a Sortino ratio of 1.092, against 0.500 for the Sharpe-style ratio. A year below target happens with probability 30.9%.
How it is built:
1. Pick a target $T$ (zero, the risk-free rate, or a required return).
2. For each return keep only the shortfall $\min(R - T, 0)$; returns above $T$ become zero.
3. Square, average and take the square root: that is the downside deviation $\sigma_D$.
4. Divide the excess return by it: $(\mu - T)/\sigma_D$. For normal returns $\sigma_D^2 = \sigma^2[(d^2+1)N(d) + d\,n(d)]$ with $d = (T - \mu)/\sigma$.
Common mistake: Averaging the squared shortfalls over only the losing periods. The downside deviation divides by all periods (the winners count as zero), otherwise it overstates the risk and the ratio is not comparable across strategies.

```python
import numpy as np
from scipy.stats import norm
from scipy.integrate import quad
mu, sigma, T = 0.10, 0.20, 0.0
dd2, _ = quad(lambda x: (T - x)**2 * norm.pdf(x, mu, sigma), -np.inf, T)
print(round((mu - T) / np.sqrt(dd2), 4))
```

#### Beta (intermediate)
URL: https://hft.md/formulas/risk/beta/

    \beta = \frac{\operatorname{Cov}(R_s, R_m)}{\operatorname{Var}(R_m)}, \qquad R_s = \alpha + \beta R_m + \varepsilon

Symbols:
- \beta: how much the stock moves, on average, for each 1% move in the market
- R_s: the stock's return
- R_m: the market's return
- \alpha: the intercept: the stock's return on a day the market is flat
- \varepsilon: the stock's own noise, unrelated to the market

What it is: Beta is the slope of a stock's returns against the market's returns. A beta of 1.5 means the stock tends to move 1.5% for every 1% the market moves.
Where it is used:
- Portfolio manager: Adds up the betas of all holdings to know how much the portfolio will move with the market, and hedges with index futures.
- Quant researcher: Strips beta out of a strategy's returns to see whether any return is left that the market does not explain.
- Options desk: Uses beta to convert single-stock deltas into index-equivalent exposure for hedging.
Why traders use it: Most of a stock's daily move is the market moving. Beta tells you how much of your risk is just market risk, which you can hedge cheaply, and how much is specific to the stock.
Worked example: With a true beta of 1.20 and 120 days of data, the fitted line in this sample has slope 1.396. The stock's correlation with the market is 0.697, so the market explains 48.5% of its variance.
How it is built:
1. Collect daily returns of the stock and the market over the same days.
2. Fit the straight line $R_s = \alpha + \beta R_m$ by least squares.
3. The least-squares slope is $\operatorname{Cov}(R_s, R_m)/\operatorname{Var}(R_m)$.
4. Equivalently $\beta = \rho\,\sigma_s/\sigma_m$: correlation times the ratio of volatilities.
Common mistake: Treating beta as fixed. It is an estimate with error, and it changes over time and between calm and stressed markets. A beta from six months of daily data can easily be off by a few tenths.

```python
import numpy as np
rng = np.random.default_rng(7)
beta, idio, n = 1.2, 0.015, 120
m = 0.01 * rng.standard_normal(n)
s = beta * m + idio * rng.standard_normal(n)
b_hat = np.cov(m, s)[0, 1] / np.var(m, ddof=1)
print(round(b_hat, 3))  # a different sample from the page's
```

#### Capital asset pricing model (CAPM) (intermediate)
URL: https://hft.md/formulas/risk/capm/

    E[R_i] = r_f + \beta_i\,\big(E[R_m] - r_f\big)

Symbols:
- E[R_i]: the expected return of asset i
- r_f: the risk-free rate
- \beta_i: the asset's beta against the market
- E[R_m] - r_f: the market risk premium: the extra return the whole market pays over cash

What it is: CAPM says an asset's expected return is the risk-free rate plus its beta times the market's extra return. Only market risk is paid for; risk you could diversify away earns nothing.
Where it is used:
- Portfolio manager: Uses it as the benchmark: any return above the CAPM line is alpha, the part that skill must explain.
- Quant researcher: Starts factor models from it, then adds factors such as size, value and momentum to explain what CAPM misses.
- Corporate finance and analysts: Uses it to set the cost of equity when discounting a company's future cash flows.
Why traders use it: It gives every asset a required return based on one number, beta. That lets you ask whether a stock is worth owning: if your forecast is below the line, the market pays you more for the same risk elsewhere.
Worked example: With cash at 3.0% and the market expected to return 8.0%, a stock with beta 1.20 should earn 9.00%. The market risk premium is 5.0%. If you forecast 11.0%, your alpha is 2.00%.
How it is built:
1. Hold the market portfolio plus cash; every efficient portfolio lies on the line from cash through the market.
2. Adding a small amount of asset $i$ changes the portfolio's risk in proportion to its beta, not its own volatility.
3. In equilibrium each asset's extra return must match the extra risk it adds, so $E[R_i] - r_f = \beta_i(E[R_m] - r_f)$.
4. Plotted against beta this is a straight line, the security market line, through $(0, r_f)$ and $(1, E[R_m])$.
Common mistake: Using it as a forecast. CAPM is a model of what returns should be in equilibrium, and in real data the line is flatter than it predicts: low-beta stocks have tended to do better than CAPM says.

```python
rf, rm, beta, forecast = 0.03, 0.08, 1.2, 0.11
required = rf + beta * (rm - rf)
print(round(required, 4), 'alpha', round(forecast - required, 4))  # 0.09 alpha 0.02
```

## Drop-in module: crash_guard.py

Pure Python 3, standard library only. Run `python crash_guard.py` to self-test; every assert matches a number on hft.md.

```python
"""
crash_guard.py: a drop-in crash-protection layer for a trading bot.
From hft.md/crash (education, not financial advice). Pure Python 3 + the standard library.

Every rule here is one equation from hft.md/crash:
  recovery_gain      gain needed after a loss x:        x / (1 - x)
  years_to_recover   at yearly return r:                ln(1/(1-x)) / ln(1+r)
  max_drawdown       worst peak-to-trough fall of an equity curve
  position_size      shares = equity * risk% / (entry - stop)
  ewma_vol           sigma_t^2 = lam * sigma_{t-1}^2 + (1 - lam) * r_t^2   (RiskMetrics, lam = 0.94)
  vol_target         exposure = sigma_target / sigma_now   (capped)
  inverse_vol        w_i = (1/sigma_i) / sum_j (1/sigma_j)
  risk_contrib       RC_i = w_i * (Sigma w)_i / (w' Sigma w)
  trend_signal       sign(P_t - P_{t-L})   (12-month time-series momentum)
  kelly_fraction     f* = p - (1 - p) / b,  use a fraction of it (half Kelly)
  var_normal / es_normal   VaR = z_c * sigma * V * sqrt(h),  ES = sigma * V * sqrt(h) * phi(z_c) / (1 - c)
  drawdown_brake     cut exposure at -10% / -20% / -30% drawdown
  stress_test        apply the S&P 500 falls of 1987, 2000-02, 2007-09, 2020, 2022

Safety: run in paper/dry-run first; never give the bot API keys that can withdraw.
"""
from math import log, sqrt, exp, pi, erf

# --- maths helpers -------------------------------------------------------------------------
def norm_cdf(x: float) -> float:
    return 0.5 * (1 + erf(x / sqrt(2)))

def norm_pdf(x: float) -> float:
    return exp(-x * x / 2) / sqrt(2 * pi)

def norm_inv(p: float) -> float:
    """Inverse standard normal CDF by bisection (accurate to ~1e-10)."""
    lo, hi = -10.0, 10.0
    for _ in range(100):
        mid = (lo + hi) / 2
        if norm_cdf(mid) < p:
            lo = mid
        else:
            hi = mid
    return (lo + hi) / 2

# --- level 1: survive ------------------------------------------------------------------------
def recovery_gain(loss: float) -> float:
    """Gain needed to get back to the old high after a loss (0.5 -> 1.0, i.e. -50% needs +100%)."""
    if not 0 <= loss < 1:
        raise ValueError("loss must be in [0, 1)")
    return loss / (1 - loss)

def years_to_recover(loss: float, yearly_return: float) -> float:
    return log(1 / (1 - loss)) / log(1 + yearly_return)

def max_drawdown(equity: list) -> float:
    """Largest fall from a running peak, as a positive fraction (0.25 = -25%)."""
    peak, mdd = equity[0], 0.0
    for v in equity:
        peak = max(peak, v)
        mdd = max(mdd, 1 - v / peak)
    return mdd

def position_size(equity: float, risk_pct: float, entry: float, stop: float) -> int:
    """Shares so that hitting the stop loses exactly equity * risk_pct."""
    per_share = abs(entry - stop)
    if per_share <= 0:
        raise ValueError("stop must differ from entry")
    return int(equity * risk_pct / per_share)

# --- level 2: balance --------------------------------------------------------------------------
def inverse_vol(vols: dict) -> dict:
    """Risk-parity weights when assets are assumed independent: w_i proportional to 1/sigma_i."""
    inv = {k: 1 / v for k, v in vols.items()}
    s = sum(inv.values())
    return {k: x / s for k, x in inv.items()}

def risk_contrib(weights: dict, vols: dict, corr: dict = None) -> dict:
    """Share of total portfolio variance from each asset: RC_i = w_i (Sigma w)_i / (w' Sigma w).
    corr maps (a, b) -> correlation; missing pairs are 0, the diagonal is 1."""
    keys = list(weights)
    def rho(a, b):
        if a == b:
            return 1.0
        if corr is None:
            return 0.0
        return corr.get((a, b), corr.get((b, a), 0.0))
    sw = {a: sum(rho(a, b) * vols[a] * vols[b] * weights[b] for b in keys) for a in keys}
    var = sum(weights[a] * sw[a] for a in keys)
    return {a: weights[a] * sw[a] / var for a in keys}

# --- level 3: adapt ----------------------------------------------------------------------------
def ewma_vol(returns: list, lam: float = 0.94, periods_per_year: int = 252) -> float:
    """Annualised EWMA volatility of daily returns (RiskMetrics)."""
    var = returns[0] ** 2
    for r in returns[1:]:
        var = lam * var + (1 - lam) * r * r
    return sqrt(var * periods_per_year)

def vol_target(target_vol: float, current_vol: float, cap: float = 2.0) -> float:
    """Fraction of normal size to hold: sigma_target / sigma_now, capped (no extreme leverage)."""
    return min(cap, target_vol / current_vol)

def trend_signal(prices: list, lookback: int = 252) -> int:
    """+1 if the price is above its level `lookback` bars ago, -1 if below, 0 if not enough data."""
    if len(prices) <= lookback:
        return 0
    d = prices[-1] - prices[-1 - lookback]
    return (d > 0) - (d < 0)

# --- level 4: insure ---------------------------------------------------------------------------
def put_payoff(strike: float, price_at_expiry: float, premium: float = 0.0) -> float:
    return max(strike - price_at_expiry, 0.0) - premium

def volatility_drag(mu: float, sigma: float) -> float:
    """Approximate long-run growth rate g = mu - sigma^2 / 2: big swings cost compounding."""
    return mu - sigma * sigma / 2

# --- level 5: god mode -------------------------------------------------------------------------
def kelly_fraction(p_win: float, payoff: float, fraction: float = 0.5) -> float:
    """Kelly bet size f* = p - (1-p)/b, scaled by `fraction` (0.5 = half Kelly). Never negative."""
    f = p_win - (1 - p_win) / payoff
    return max(0.0, f * fraction)

def var_normal(value: float, daily_vol: float, conf: float = 0.99, days: int = 1) -> float:
    return norm_inv(conf) * daily_vol * value * sqrt(days)

def es_normal(value: float, daily_vol: float, conf: float = 0.975, days: int = 1) -> float:
    z = norm_inv(conf)
    return daily_vol * value * sqrt(days) * norm_pdf(z) / (1 - conf)

def drawdown_brake(drawdown: float, steps=((0.10, 0.75), (0.20, 0.5), (0.30, 0.25))) -> float:
    """Exposure multiplier for the current drawdown: 1.0 normally, then 75% / 50% / 25%."""
    mult = 1.0
    for level, m in steps:
        if drawdown >= level:
            mult = m
    return mult

# S&P 500 closing levels (peak, trough) from hft.md/crash
CRASHES = {
    "1987 Black Monday":        (336.77, 223.92),
    "2000-02 Dot-com bust":     (1527.46, 776.76),
    "2007-09 Financial crisis": (1565.15, 676.53),
    "2020 Covid crash":         (3386.15, 2237.40),
    "2022 Rate shock":          (4796.56, 3577.03),
}

def stress_test(equity_beta: float = 1.0, other_return: float = 0.0, equity_weight: float = 1.0) -> dict:
    """Rough loss in each historical crash: the stock sleeve falls with the S&P 500 times its beta,
    the rest earns `other_return`. Real tests should use each holding's actual returns."""
    out = {}
    for name, (peak, trough) in CRASHES.items():
        spx = trough / peak - 1
        out[name] = equity_weight * equity_beta * spx + (1 - equity_weight) * other_return
    return out

class CrashGuard:
    """Combine the rules into one exposure number between 0 and the cap.
       exposure = vol_target x drawdown_brake x (trend filter), then optionally limited by Kelly."""
    def __init__(self, target_vol=0.10, cap=1.5, trend_lookback=252, short_trend_mult=0.5):
        self.target_vol, self.cap = target_vol, cap
        self.lookback, self.short_trend_mult = trend_lookback, short_trend_mult

    def exposure(self, prices: list, equity_curve: list) -> float:
        rets = [prices[i] / prices[i - 1] - 1 for i in range(1, len(prices))]
        vol = ewma_vol(rets[-260:]) if len(rets) > 1 else self.target_vol
        e = vol_target(self.target_vol, max(vol, 1e-6), self.cap)
        e *= drawdown_brake(max_drawdown(equity_curve[-260:]) if equity_curve else 0.0)
        if trend_signal(prices, self.lookback) < 0:
            e *= self.short_trend_mult
        return round(e, 4)

if __name__ == "__main__":
    # self-test: these numbers match hft.md/crash and hft.md/formulas
    assert abs(recovery_gain(0.5) - 1.0) < 1e-12
    assert abs(recovery_gain(0.2) - 0.25) < 1e-12
    assert abs(years_to_recover(0.5, 0.08) - 9.006) < 1e-3
    assert abs(vol_target(0.10, 0.40) - 0.25) < 1e-12
    assert position_size(100_000, 0.01, 50, 48) == 500
    w = inverse_vol({"stocks": 0.16, "bonds": 0.06, "gold": 0.15, "commodities": 0.18})
    assert round(w["bonds"], 2) == 0.47
    rc = risk_contrib({"stocks": 0.6, "bonds": 0.4}, {"stocks": 0.16, "bonds": 0.06})
    assert round(rc["stocks"], 2) == 0.94
    assert abs(var_normal(1_000_000, 0.015, 0.99) - 34_895) < 2
    assert abs(es_normal(1_000_000, 0.015, 0.975) - 35_067) < 2
    assert abs(kelly_fraction(0.55, 1.0, 1.0) - 0.10) < 1e-12
    assert drawdown_brake(0.22) == 0.5
    assert round(stress_test()["2007-09 Financial crisis"], 3) == -0.568
    assert max_drawdown([100, 120, 90, 130, 104]) == 0.25
    print("crash_guard self-test: all checks passed")
```

## S&P 500 crash history (data for stress tests)

Closing levels, price only (no dividends). Percent fall and durations computed from the levels and dates.

| Crash | Peak date | Peak | Trough date | Trough | Fall | Days falling | Back to old high | Days to recover |
|---|---|---|---|---|---|---|---|---|
| 1987 Black Monday | 1987-08-25 | 336.77 | 1987-12-04 | 223.92 | -33.5% | 101 | 1989-07-26 | 600 |
| 2000 Dot-com bust | 2000-03-24 | 1527.46 | 2002-10-09 | 776.76 | -49.1% | 929 | 2007-05-30 | 1694 |
| 2008 Global financial crisis | 2007-10-09 | 1565.15 | 2009-03-09 | 676.53 | -56.8% | 517 | 2013-03-28 | 1480 |
| 2020 Covid crash | 2020-02-19 | 3386.15 | 2020-03-23 | 2237.4 | -33.9% | 33 | 2020-08-18 | 148 |
| 2022 Rate-shock bear market | 2022-01-03 | 4796.56 | 2022-10-12 | 3577.03 | -25.4% | 282 | 2024-01-19 | 464 |

- 1987: Portfolio insurance selling fed on itself; the S&P 500 fell 20.5% on 19 October alone. What held up: Bonds rallied as rates were cut; trend followers that were short futures gained.
- 2000: Tech valuations collapsed over two and a half years, not one day. What held up: Government bonds, value stocks and slow trend-following rules that turned short.
- 2008: A housing and banking crisis; credit froze and correlations between risky assets jumped. What held up: Treasuries, managed futures (trend following) and tail hedges; most other 'diversifiers' fell together.
- 2020: The fastest fall into a bear market on record, then massive central-bank support. What held up: Long volatility and tail hedges paid fastest; Treasuries helped; slow trend rules were caught out by the speed.
- 2022: Inflation forced rapid rate hikes; stocks and bonds fell together. What held up: Trend following (short bonds, long the dollar and commodities) and commodities. Bonds did not protect, so 60/40 and risk parity were hurt.

## Free market-stress data (what to watch, how to pull it)

### VIX term structure (VIX Central)
URL: http://vixcentral.com/
What: Futures prices for future months of the VIX. Normally later months cost more (contango).
Read it: When near-term VIX trades above later months (backwardation), the market is paying up for protection now: stress is acute.
Not: It describes fear today; it does not forecast the size or timing of a fall.

### High-yield credit spread (FRED (ICE BofA))
URL: https://fred.stlouisfed.org/series/BAMLH0A0HYM2
FRED series: BAMLH0A0HYM2. Free CSV: https://fred.stlouisfed.org/graph/fredgraph.csv?id=BAMLH0A0HYM2
What: Extra yield investors demand to lend to riskier companies.
Read it: Spreads widening fast means lenders are worried about defaults; credit often turns before stocks do.
Not: Spreads can stay tight for years and widen late.

### St. Louis Fed Financial Stress Index (FRED)
URL: https://fred.stlouisfed.org/series/STLFSI4
FRED series: STLFSI4. Free CSV: https://fred.stlouisfed.org/graph/fredgraph.csv?id=STLFSI4
What: Weekly index built from 18 market series: rates, spreads and volatility. Zero is normal.
Read it: Above zero means stress is above average; big spikes marked 2008 and 2020.
Not: It is a thermometer, not a forecast.

### VIX (daily close) (FRED (Cboe))
URL: https://fred.stlouisfed.org/series/VIXCLS
FRED series: VIXCLS. Free CSV: https://fred.stlouisfed.org/graph/fredgraph.csv?id=VIXCLS
What: Implied volatility of S&P 500 options over the next 30 days.
Read it: Roughly the market’s expected annual move; divide by √252 (about 16) for a daily move.
Not: A high VIX says moves will be big, not which way.

### Put/call ratios (Cboe)
URL: https://www.cboe.com/us/options/market_statistics/daily/
What: Daily volume of puts versus calls traded.
Read it: Very high readings show heavy hedging or fear; very low readings show complacency.
Not: Readings are noisy day to day; traders watch extremes and averages.

### Yield curve (10y − 2y) (FRED)
URL: https://fred.stlouisfed.org/series/T10Y2Y
FRED series: T10Y2Y. Free CSV: https://fred.stlouisfed.org/graph/fredgraph.csv?id=T10Y2Y
What: Ten-year minus two-year Treasury yield.
Read it: An inverted curve (below zero) has come before past US recessions.
Not: The lag has been long and variable; it is not a timing tool.

### Fear & Greed Index (CNN)
URL: https://www.cnn.com/markets/fear-and-greed
What: A 0–100 blend of seven market signals, including momentum, breadth and put/call.
Read it: Extreme fear readings often come near sell-off lows; extreme greed near euphoric tops.
Not: It is a sentiment summary, not a trading signal.


## Prompts from the page

### 1. Stress-test my portfolio

Here are my holdings and weights: [paste]. For each of these periods, estimate what my portfolio would have lost from peak to trough using each holding's (or its closest index's) actual returns: 1987 (Aug–Dec), 2000–2002, Oct 2007–Mar 2009, Feb–Mar 2020, 2022. Show your sources, say where you had to approximate, and tell me which holding hurt most in each crash. Do not give buy or sell advice.

### 2. Turn it into risk parity

Here are my holdings: [paste]. Using about 3 years of daily returns, compute each one's annualised volatility and the correlations between them. Show me (1) what share of my portfolio's total risk comes from each holding now, and (2) the weights that would give each holding an equal share of risk. Explain it in simple words and list the assumptions.

### 3. Write my crash plan

Help me write a one-page crash plan for my portfolio: [paste holdings and cash]. Ask me my maximum acceptable loss, then give me: position-size limits, a volatility rule that cuts exposure when markets get wild, what I will do at −10%, −20% and −30%, and what would make me add back. Keep it to rules I can follow without watching the market all day.


## Where each approach fails

- Risk parity / 60-40: in 2022 stocks and bonds fell together, so balance by risk did not protect.
- Trend following: very fast crashes (Feb–Mar 2020) can hit before slow rules turn; long flat periods.
- Tail hedging: costs money in calm years by design; only pays in large falls.
- Volatility targeting: cuts exposure after volatility rises, so it can sell near lows and miss rebounds.
- Normal-distribution VaR understates fat tails; prefer expected shortfall and historical stress tests.
- Renaissance Technologies’ methods are secret; nothing here describes Medallion.

## Licence and disclaimer

Summaries of public papers and free data. hft.md is not affiliated with any fund named. No prediction of any crash is made or implied. This is education, not financial advice.
