//! Discrete Markov microprice model (Stoikov 2018) with offline transition fitting
//! and zero-allocation $O(1)$ hot-path lookup (`BARBELL / S1-3`).
//!
//! Discretizes top-of-book imbalance $I = Q_B / (Q_B + Q_A)$ and spread in ticks $S$,
//! symmetrizes state transitions $(I, S) \leftrightarrow (1 - I, S)$ with negated mid
//! moves to preserve exact bid/ask parity, solves $G_1 = (I - Q)^{-1} r$ for the
//! first-order adjustment, and iterates $g^{(m+1)} = g^{(m)} + (Q + T)^m G_1$.

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

fn default_require_convergence() -> bool {
    true
}

/// Configuration for [`MarkovMicropriceTable::fit`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarkovMicropriceConfig {
    /// Even number of equal-width imbalance buckets in `(0, 1)` (`>= 2`, `<= 32`).
    pub imbalance_buckets: usize,
    /// Number of discrete spread buckets starting at 1 tick (`>= 1`, `<= 16`).
    pub spread_buckets: usize,
    /// Maximum iterations of the convergent adjustment series (`>= 1`).
    pub max_iters: usize,
    /// Infinite-norm stopping tolerance on iterative step `||(Q+T)^m G1||_inf`.
    pub tol: f64,
    /// Minimum post-symmetrization outgoing transitions required per discrete state.
    pub min_transitions_per_state: u64,
    /// Reject unconverged tables (`residual_inf_norm > tol`) by default (`true`).
    #[serde(default = "default_require_convergence")]
    pub require_convergence: bool,
}

impl Default for MarkovMicropriceConfig {
    fn default() -> Self {
        Self {
            imbalance_buckets: 10,
            spread_buckets: 4,
            max_iters: 64,
            tol: 1e-9,
            min_transitions_per_state: 1,
            require_convergence: true,
        }
    }
}

impl MarkovMicropriceConfig {
    /// Fail closed on invalid discretization, iteration, or tolerance bounds.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.imbalance_buckets >= 2
                && self.imbalance_buckets <= 32
                && self.imbalance_buckets.is_multiple_of(2),
            "imbalance_buckets must be an even integer in [2, 32]"
        );
        ensure!(
            (1..=16).contains(&self.spread_buckets),
            "spread_buckets must be in [1, 16]"
        );
        ensure!(self.max_iters >= 1, "max_iters must be >= 1");
        ensure!(
            self.tol.is_finite() && self.tol > 0.0 && self.tol < 1.0,
            "tol must be finite and in (0, 1)"
        );
        Ok(())
    }
}

/// One causal top-of-book snapshot used to calibrate a [`MarkovMicropriceTable`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct MarkovBookObservation {
    pub timestamp_ms: u64,
    pub bid_px: f64,
    pub ask_px: f64,
    pub bid_sz: f64,
    pub ask_sz: f64,
    pub tick_size: f64,
}

impl MarkovBookObservation {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.timestamp_ms > 0,
            "observation timestamp_ms must be > 0"
        );
        ensure!(
            self.bid_px.is_finite()
                && self.ask_px.is_finite()
                && self.bid_sz.is_finite()
                && self.ask_sz.is_finite()
                && self.tick_size.is_finite(),
            "observation fields must be finite"
        );
        ensure!(
            self.bid_px > 0.0
                && self.ask_px > self.bid_px
                && self.bid_sz > 0.0
                && self.ask_sz > 0.0
                && self.tick_size > 0.0,
            "observation prices, sizes, and tick_size must be positive and non-crossed"
        );
        Ok(())
    }
}

/// Tight residual ceiling (`1e-6` ticks) required before a fitted or serialized
/// [`MarkovMicropriceTable`] can drive [`QuotePipeline`](crate::strategy::quote_pipeline::QuotePipeline).
pub const MAX_PRODUCTION_MICROPRICE_RESIDUAL_TICKS: f64 = 1e-6;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkovMicropriceTableRaw {
    imbalance_buckets: usize,
    spread_buckets: usize,
    adjustments_ticks: Vec<f64>,
    iterations_used: usize,
    residual_inf_norm: f64,
    #[serde(default)]
    convergence_tol: f64,
    #[serde(default)]
    converged: bool,
    symmetrized_transitions: u64,
}

/// Fitted Stoikov (2018) discrete Markov microprice lookup table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "MarkovMicropriceTableRaw")]
pub struct MarkovMicropriceTable {
    imbalance_buckets: usize,
    spread_buckets: usize,
    adjustments_ticks: Vec<f64>,
    iterations_used: usize,
    residual_inf_norm: f64,
    convergence_tol: f64,
    converged: bool,
    symmetrized_transitions: u64,
}

impl TryFrom<MarkovMicropriceTableRaw> for MarkovMicropriceTable {
    type Error = anyhow::Error;

    fn try_from(raw: MarkovMicropriceTableRaw) -> Result<Self> {
        let table = Self {
            imbalance_buckets: raw.imbalance_buckets,
            spread_buckets: raw.spread_buckets,
            adjustments_ticks: raw.adjustments_ticks,
            iterations_used: raw.iterations_used,
            residual_inf_norm: raw.residual_inf_norm,
            convergence_tol: raw.convergence_tol,
            converged: raw.converged,
            symmetrized_transitions: raw.symmetrized_transitions,
        };
        table.validate()?;
        Ok(table)
    }
}

impl MarkovMicropriceTable {
    /// Fit a symmetrized discrete Markov microprice table from chronological observations.
    ///
    /// Fails closed on empty/non-monotonic/invalid books, unobserved states, singular
    /// transient matrices `(I - Q)`, or non-finite intermediate values.
    pub fn fit(
        cfg: &MarkovMicropriceConfig,
        observations: &[MarkovBookObservation],
    ) -> Result<Self> {
        cfg.validate()?;
        ensure!(
            observations.len() >= 2,
            "at least 2 chronological observations are required"
        );
        for (idx, obs) in observations.iter().enumerate() {
            obs.validate()?;
            if idx > 0 {
                ensure!(
                    obs.timestamp_ms >= observations[idx - 1].timestamp_ms,
                    "non-monotonic observation timestamp at index {idx}"
                );
            }
        }

        let n_imb = cfg.imbalance_buckets;
        let n_spr = cfg.spread_buckets;
        let n_states = n_imb * n_spr;

        let mut q_counts = vec![0.0_f64; n_states * n_states];
        let mut t_counts = vec![0.0_f64; n_states * n_states];
        let mut r_sum_ticks = vec![0.0_f64; n_states];
        let mut row_counts = vec![0_u64; n_states];

        let state_index = |obs: &MarkovBookObservation| -> Option<(usize, usize)> {
            let spread_ticks = ((obs.ask_px - obs.bid_px) / obs.tick_size).round();
            if !spread_ticks.is_finite() || spread_ticks < 1.0 {
                return None;
            }
            let s = (spread_ticks as usize).saturating_sub(1).min(n_spr - 1);
            let total_sz = obs.bid_sz + obs.ask_sz;
            if !total_sz.is_finite() || total_sz <= 0.0 {
                return None;
            }
            let imb = obs.bid_sz / total_sz;
            let i = symmetric_imbalance_bucket(imb, n_imb);
            let i_mir = (n_imb - 1) - i;
            Some((s * n_imb + i, s * n_imb + i_mir))
        };

        let mut total_sym_transitions = 0_u64;
        for pair in observations.windows(2) {
            let prev = &pair[0];
            let next = &pair[1];
            let Some((u, u_mir)) = state_index(prev) else {
                bail!("invalid source book discretization state");
            };
            let Some((v, v_mir)) = state_index(next) else {
                bail!("invalid next book discretization state");
            };
            let mid_prev = 0.5 * (prev.bid_px + prev.ask_px);
            let mid_next = 0.5 * (next.bid_px + next.ask_px);
            let d_mid_ticks = (mid_next - mid_prev) / prev.tick_size;
            ensure!(d_mid_ticks.is_finite(), "non-finite mid-price move");

            if d_mid_ticks.abs() <= 1e-12 {
                q_counts[u * n_states + v] += 1.0;
                q_counts[u_mir * n_states + v_mir] += 1.0;
            } else {
                t_counts[u * n_states + v] += 1.0;
                t_counts[u_mir * n_states + v_mir] += 1.0;
                r_sum_ticks[u] += d_mid_ticks;
                r_sum_ticks[u_mir] -= d_mid_ticks;
            }
            row_counts[u] = row_counts[u].saturating_add(1);
            row_counts[u_mir] = row_counts[u_mir].saturating_add(1);
            total_sym_transitions = total_sym_transitions.saturating_add(2);
        }

        for (u, &cnt) in row_counts.iter().enumerate() {
            ensure!(
                cnt >= cfg.min_transitions_per_state && cnt > 0,
                "insufficient transitions for state {u}: {cnt} < {}",
                cfg.min_transitions_per_state.max(1)
            );
        }

        let mut t_mat = vec![0.0_f64; n_states * n_states];
        let mut iq_mat = vec![0.0_f64; n_states * n_states];
        let mut r_vec = vec![0.0_f64; n_states];

        for u in 0..n_states {
            let inv = 1.0 / (row_counts[u] as f64);
            r_vec[u] = r_sum_ticks[u] * inv;
            for v in 0..n_states {
                let q = q_counts[u * n_states + v] * inv;
                let t = t_counts[u * n_states + v] * inv;
                t_mat[u * n_states + v] = t;
                iq_mat[u * n_states + v] = if u == v { 1.0 - q } else { -q };
            }
        }

        // Exact Stoikov (2018) Theorem 1 discrete renewal series:
        //   G_1 = (I - Q)^{-1} r,  B^* = (I - Q)^{-1} T,  g^{(m+1)} = G_1 + \sum_{k=1}^m (B^*)^k G_1.
        let inv_iq = invert_matrix(n_states, &mut iq_mat)?;
        let mut g1 = vec![0.0_f64; n_states];
        mat_vec_mul(n_states, &inv_iq, &r_vec, &mut g1);
        for &val in &g1 {
            ensure!(val.is_finite(), "non-finite Markov microprice G1 entry");
        }
        let mut b_star = vec![0.0_f64; n_states * n_states];
        mat_mat_mul(n_states, &inv_iq, &t_mat, &mut b_star);

        let mut g = g1.clone();
        let mut prev_term = g1;
        let mut term = vec![0.0_f64; n_states];
        mat_vec_mul(n_states, &b_star, &prev_term, &mut term);

        let mut iters_used = 1usize;
        let mut residual = term.iter().fold(0.0_f64, |acc, v| acc.max(v.abs()));
        while iters_used < cfg.max_iters && residual > cfg.tol {
            for idx in 0..n_states {
                g[idx] += term[idx];
                ensure!(g[idx].is_finite(), "non-finite microprice iterate");
            }
            std::mem::swap(&mut prev_term, &mut term);
            mat_vec_mul(n_states, &b_star, &prev_term, &mut term);
            residual = term.iter().fold(0.0_f64, |acc, v| acc.max(v.abs()));
            ensure!(residual.is_finite(), "non-finite microprice residual");
            iters_used += 1;
        }

        let converged = residual <= cfg.tol;
        if cfg.require_convergence && !converged {
            bail!(
                "Markov microprice failed to converge within {} iterations (residual_inf_norm={:.3e} > tol={:.3e})",
                iters_used,
                residual,
                cfg.tol
            );
        }

        // Enforce exact antisymmetry and bound adjustments to half the bucket's minimum spread.
        for s in 0..n_spr {
            let max_adj_ticks = 0.5 * ((s + 1) as f64);
            for i in 0..(n_imb / 2) {
                let idx_l = s * n_imb + i;
                let idx_r = s * n_imb + (n_imb - 1 - i);
                let anti = 0.5 * (g[idx_l] - g[idx_r]);
                let clamped = anti.clamp(-max_adj_ticks, max_adj_ticks);
                g[idx_l] = clamped;
                g[idx_r] = -clamped;
            }
        }

        Ok(Self {
            imbalance_buckets: n_imb,
            spread_buckets: n_spr,
            adjustments_ticks: g,
            iterations_used: iters_used,
            residual_inf_norm: residual,
            convergence_tol: cfg.tol,
            converged,
            symmetrized_transitions: total_sym_transitions,
        })
    }

    pub fn imbalance_buckets(&self) -> usize {
        self.imbalance_buckets
    }

    pub fn spread_buckets(&self) -> usize {
        self.spread_buckets
    }

    pub fn iterations_used(&self) -> usize {
        self.iterations_used
    }

    pub fn residual_inf_norm(&self) -> f64 {
        self.residual_inf_norm
    }

    pub fn convergence_tol(&self) -> f64 {
        self.convergence_tol
    }

    pub fn is_converged(&self) -> bool {
        self.converged
            && self.residual_inf_norm.is_finite()
            && self.residual_inf_norm >= 0.0
            && self.convergence_tol.is_finite()
            && self.convergence_tol > 0.0
            && self.convergence_tol < 1.0
            && self.residual_inf_norm <= self.convergence_tol
    }

    /// Validate structural table invariants and serialized convergence witness consistency.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.imbalance_buckets >= 2
                && self.imbalance_buckets <= 32
                && self.imbalance_buckets.is_multiple_of(2),
            "imbalance_buckets must be an even integer in [2, 32]"
        );
        ensure!(
            (1..=16).contains(&self.spread_buckets),
            "spread_buckets must be in [1, 16]"
        );
        let expected_len = self
            .imbalance_buckets
            .checked_mul(self.spread_buckets)
            .ok_or_else(|| anyhow::anyhow!("MarkovMicropriceTable dimensions overflow"))?;
        ensure!(
            self.adjustments_ticks.len() == expected_len,
            "adjustments_ticks length ({}) does not match expected grid ({expected_len})",
            self.adjustments_ticks.len()
        );
        for s in 0..self.spread_buckets {
            let max_adj_ticks = 0.5 * ((s + 1) as f64);
            for i in 0..self.imbalance_buckets {
                let val = self.adjustments_ticks[s * self.imbalance_buckets + i];
                ensure!(
                    val.is_finite() && val.abs() <= max_adj_ticks + 1e-12,
                    "microprice adjustment at spread bucket {s}, imbalance {i} out of bounds: {val}"
                );
            }
            for i in 0..(self.imbalance_buckets / 2) {
                let idx_l = s * self.imbalance_buckets + i;
                let idx_r = s * self.imbalance_buckets + (self.imbalance_buckets - 1 - i);
                let sym_err = (self.adjustments_ticks[idx_l] + self.adjustments_ticks[idx_r]).abs();
                ensure!(
                    sym_err <= 1e-9,
                    "microprice adjustment antisymmetry violated at spread bucket {s}, pair ({i}, {}): err={sym_err:.3e}",
                    self.imbalance_buckets - 1 - i
                );
            }
        }
        ensure!(self.iterations_used >= 1, "iterations_used must be >= 1");
        ensure!(
            self.residual_inf_norm.is_finite() && self.residual_inf_norm >= 0.0,
            "residual_inf_norm must be finite and >= 0"
        );
        ensure!(
            self.convergence_tol.is_finite()
                && self.convergence_tol >= 0.0
                && self.convergence_tol < 1.0,
            "convergence_tol must be finite and in [0, 1)"
        );
        if self.converged {
            ensure!(
                self.convergence_tol > 0.0 && self.convergence_tol < 1.0,
                "converged table requires convergence_tol in (0, 1)"
            );
            ensure!(
                self.residual_inf_norm <= self.convergence_tol,
                "inconsistent convergence witness: residual_inf_norm ({:.3e}) > convergence_tol ({:.3e})",
                self.residual_inf_norm,
                self.convergence_tol
            );
        }
        Ok(())
    }

    /// Require a structurally valid, converged table whose fitted tolerance and residual
    /// satisfy the production residual bound `max_residual_ticks`.
    pub fn validate_for_production(&self, max_residual_ticks: f64) -> Result<()> {
        self.validate()?;
        ensure!(
            max_residual_ticks.is_finite() && max_residual_ticks > 0.0 && max_residual_ticks < 1.0,
            "max_residual_ticks must be finite and in (0, 1)"
        );
        ensure!(
            self.is_converged(),
            "MarkovMicropriceTable is not validly converged (residual={:.3e}, tol={:.3e})",
            self.residual_inf_norm,
            self.convergence_tol
        );
        ensure!(
            self.convergence_tol <= max_residual_ticks
                && self.residual_inf_norm <= max_residual_ticks,
            "MarkovMicropriceTable tolerance/residual ({:.3e}, {:.3e}) exceeds production cap ({:.3e})",
            self.convergence_tol,
            self.residual_inf_norm,
            max_residual_ticks
        );
        Ok(())
    }

    pub fn symmetrized_transitions(&self) -> u64 {
        self.symmetrized_transitions
    }

    /// Raw fitted microprice adjustment $g(I, S)$ in ticks for a discrete bucket pair.
    pub fn adjustment_ticks(&self, imbalance_bucket: usize, spread_bucket: usize) -> Option<f64> {
        if imbalance_bucket >= self.imbalance_buckets || spread_bucket >= self.spread_buckets {
            return None;
        }
        Some(self.adjustments_ticks[spread_bucket * self.imbalance_buckets + imbalance_bucket])
    }

    /// Zero-allocation $O(1)$ hot-path evaluation returning $P_{\text{micro}} = M + g(I, S)$
    /// strictly bounded within `[bid_px, ask_px]`.
    ///
    /// The adjustment is a nearest-bucket table lookup (a step function over the discretized
    /// imbalance/spread grid); it performs NO linear interpolation between adjacent buckets.
    #[inline]
    pub fn microprice(
        &self,
        bid_px: f64,
        ask_px: f64,
        bid_sz: f64,
        ask_sz: f64,
        tick_size: f64,
    ) -> Option<f64> {
        if !bid_px.is_finite()
            || !ask_px.is_finite()
            || !bid_sz.is_finite()
            || !ask_sz.is_finite()
            || !tick_size.is_finite()
            || bid_px <= 0.0
            || ask_px <= bid_px
            || bid_sz <= 0.0
            || ask_sz <= 0.0
            || tick_size <= 0.0
        {
            return None;
        }
        let spread = ask_px - bid_px;
        let spread_ticks = (spread / tick_size).round();
        if !spread_ticks.is_finite() || spread_ticks < 1.0 {
            return None;
        }
        let s = (spread_ticks as usize)
            .saturating_sub(1)
            .min(self.spread_buckets - 1);
        let imb = bid_sz / (bid_sz + ask_sz);
        let adj_ticks = if (imb - 0.5).abs() <= 1e-12 {
            0.0
        } else {
            let i = symmetric_imbalance_bucket(imb, self.imbalance_buckets);
            self.adjustments_ticks[s * self.imbalance_buckets + i]
        };
        let mid = 0.5 * (bid_px + ask_px);
        let g_px = (adj_ticks * tick_size).clamp(-0.5 * spread, 0.5 * spread);
        let out = (mid + g_px).clamp(bid_px, ask_px);
        out.is_finite().then_some(out)
    }
}

#[inline]
fn symmetric_imbalance_bucket(imb: f64, n_imb: usize) -> usize {
    let half = n_imb / 2;
    if imb < 0.5 {
        ((imb * (n_imb as f64)).floor().max(0.0) as usize).min(half - 1)
    } else {
        let mir = (((1.0 - imb) * (n_imb as f64)).floor().max(0.0) as usize).min(half - 1);
        (n_imb - 1) - mir
    }
}

fn mat_vec_mul(n: usize, mat: &[f64], vec: &[f64], out: &mut [f64]) {
    for (r, slot) in out.iter_mut().enumerate().take(n) {
        let mut sum = 0.0;
        let base = r * n;
        for c in 0..n {
            sum += mat[base + c] * vec[c];
        }
        *slot = sum;
    }
}

fn mat_mat_mul(n: usize, a: &[f64], b: &[f64], out: &mut [f64]) {
    for r in 0..n {
        let a_row = r * n;
        let out_row = r * n;
        for c in 0..n {
            let mut sum = 0.0;
            for k in 0..n {
                sum += a[a_row + k] * b[k * n + c];
            }
            out[out_row + c] = sum;
        }
    }
}

/// Invert a square matrix `a` (`n x n`) via Gauss-Jordan elimination with partial pivoting.
fn invert_matrix(n: usize, a: &mut [f64]) -> Result<Vec<f64>> {
    let mut inv = vec![0.0_f64; n * n];
    for i in 0..n {
        inv[i * n + i] = 1.0;
    }

    for col in 0..n {
        let mut pivot_row = col;
        let mut best_abs = a[col * n + col].abs();
        for row in (col + 1)..n {
            let val = a[row * n + col].abs();
            if val > best_abs {
                best_abs = val;
                pivot_row = row;
            }
        }
        ensure!(
            best_abs.is_finite() && best_abs > 1e-12,
            "singular or degenerate Markov transient matrix (I - Q) at col {col}"
        );
        if pivot_row != col {
            for k in 0..n {
                a.swap(col * n + k, pivot_row * n + k);
                inv.swap(col * n + k, pivot_row * n + k);
            }
        }
        let inv_pivot = 1.0 / a[col * n + col];
        ensure!(inv_pivot.is_finite(), "non-finite pivot inverse");
        for k in 0..n {
            a[col * n + k] *= inv_pivot;
            inv[col * n + k] *= inv_pivot;
        }
        for row in 0..n {
            if row == col {
                continue;
            }
            let factor = a[row * n + col];
            if factor == 0.0 {
                continue;
            }
            for k in 0..n {
                a[row * n + k] -= factor * a[col * n + k];
                inv[row * n + k] -= factor * inv[col * n + k];
            }
        }
    }

    for &val in &inv {
        ensure!(val.is_finite(), "non-finite inverse matrix entry");
    }
    Ok(inv)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_predictive_book_stream() -> Vec<MarkovBookObservation> {
        // Generate rich observations across 2 imbalance buckets x 1 spread bucket where
        // bid-heavy books predict upward mid-price moves after transient dwelling.
        let mut obs = Vec::new();
        let mut ts = 1_700_000_000_000_u64;
        let mut mid = 100.05_f64;
        let tick = 0.1_f64;

        for step in 0..80_u64 {
            let bid_px = ((mid - 0.5 * tick) / tick).round() * tick;
            let ask_px = bid_px + tick;
            let (bid_sz, ask_sz) = match step % 4 {
                0 => (9.0, 1.0), // bid heavy transient -> stays bid heavy
                1 => (9.0, 1.0), // bid heavy -> jumps +1 tick on next step
                2 => (5.0, 5.0), // jump destination at mid+tick (balanced transiently)
                _ => (1.0, 9.0), // ask heavy transient -> jumps -1 tick
            };
            obs.push(MarkovBookObservation {
                timestamp_ms: ts,
                bid_px,
                ask_px,
                bid_sz,
                ask_sz,
                tick_size: tick,
            });
            ts += 50;
            if step % 4 == 1 {
                mid += tick;
            } else if step % 4 == 3 {
                mid -= tick;
            }
        }
        obs
    }

    #[test]
    fn markov_microprice_enforces_exact_symmetry_monotonicity_and_touch_bounds() {
        let cfg = MarkovMicropriceConfig {
            imbalance_buckets: 2,
            spread_buckets: 1,
            max_iters: 64,
            tol: 1e-9,
            min_transitions_per_state: 1,
            require_convergence: true,
        };
        let obs = synthetic_predictive_book_stream();
        let table = MarkovMicropriceTable::fit(&cfg, &obs).unwrap();

        let g_ask_heavy = table.adjustment_ticks(0, 0).unwrap();
        let g_bid_heavy = table.adjustment_ticks(1, 0).unwrap();
        assert!((g_ask_heavy + g_bid_heavy).abs() < 1e-12);
        assert!(
            g_bid_heavy > 0.0,
            "expected positive bid-heavy adjustment, got {g_bid_heavy}"
        );

        let bid = 100.0;
        let ask = 100.1;
        let tick = 0.1;
        let mid = 100.05;

        let p_equal = table.microprice(bid, ask, 5.0, 5.0, tick).unwrap();
        assert!((p_equal - mid).abs() < 1e-12);

        let p_bid = table.microprice(bid, ask, 9.0, 1.0, tick).unwrap();
        let p_ask = table.microprice(bid, ask, 1.0, 9.0, tick).unwrap();
        assert!(p_bid > mid && p_bid <= ask);
        assert!(p_ask < mid && p_ask >= bid);
        assert!(((p_bid - mid) + (p_ask - mid)).abs() < 1e-12);

        // Even at exact bucket boundary, swapping bid_sz <-> ask_sz stays strictly antisymmetric.
        let p_b2 = table.microprice(bid, ask, 20.0, 80.0, tick).unwrap();
        let p_a2 = table.microprice(bid, ask, 80.0, 20.0, tick).unwrap();
        assert!(((p_b2 - mid) + (p_a2 - mid)).abs() < 1e-12);
    }

    #[test]
    fn markov_microprice_fails_closed_on_singular_insufficient_and_non_monotonic_inputs() {
        let cfg = MarkovMicropriceConfig {
            imbalance_buckets: 2,
            spread_buckets: 1,
            ..MarkovMicropriceConfig::default()
        };

        // Never moves -> T == 0, Q is row-stochastic -> (I - Q) is singular!
        let static_obs: Vec<MarkovBookObservation> = (0..6)
            .map(|k| MarkovBookObservation {
                timestamp_ms: 1_000 + k * 10,
                bid_px: 100.0,
                ask_px: 100.1,
                bid_sz: if k % 2 == 0 { 8.0 } else { 2.0 },
                ask_sz: if k % 2 == 0 { 2.0 } else { 8.0 },
                tick_size: 0.1,
            })
            .collect();
        assert!(MarkovMicropriceTable::fit(&cfg, &static_obs).is_err());

        // Non-monotonic timestamps fail closed.
        let mut bad_ts = synthetic_predictive_book_stream();
        bad_ts[5].timestamp_ms = bad_ts[4].timestamp_ms - 1;
        assert!(MarkovMicropriceTable::fit(&cfg, &bad_ts).is_err());

        // Sparse bucket grid where some buckets remain unobserved fails closed.
        let fine_cfg = MarkovMicropriceConfig {
            imbalance_buckets: 10,
            spread_buckets: 4,
            min_transitions_per_state: 1,
            ..MarkovMicropriceConfig::default()
        };
        assert!(
            MarkovMicropriceTable::fit(&fine_cfg, &synthetic_predictive_book_stream()).is_err()
        );

        // Invalid hot-path lookups return None.
        let table = MarkovMicropriceTable::fit(&cfg, &synthetic_predictive_book_stream()).unwrap();
        assert!(table.microprice(100.1, 100.0, 5.0, 5.0, 0.1).is_none());
        assert!(table.microprice(100.0, 100.1, 0.0, 5.0, 0.1).is_none());
        assert!(table.microprice(100.0, 100.1, f64::NAN, 5.0, 0.1).is_none());
    }

    #[test]
    fn markov_microprice_matches_exact_markov_renewal_series_without_dwell_overcounting() {
        // Construct a 2-state x 1-spread transition system directly via stream with heavy dwell
        // (`Q` nonzero) so the buggy `Q + T` operator would severely overcount dwell steps,
        // while `B^* = (I - Q)^{-1} T` matches the exact truncated sum `\sum_{k=0}^{200} P^k r`.
        let obs = synthetic_predictive_book_stream();
        let cfg = MarkovMicropriceConfig {
            imbalance_buckets: 2,
            spread_buckets: 1,
            max_iters: 128,
            tol: 1e-11,
            min_transitions_per_state: 1,
            require_convergence: true,
        };
        let table = MarkovMicropriceTable::fit(&cfg, &obs).unwrap();

        // Independently recompute symmetrized P = Q + T and r from the exact same pairs.
        let n = 2usize;
        let mut p = vec![0.0_f64; n * n];
        let mut r = vec![0.0_f64; n];
        let mut counts = vec![0.0_f64; n];
        let bucket = |o: &MarkovBookObservation| -> (usize, usize) {
            let imb = o.bid_sz / (o.bid_sz + o.ask_sz);
            let i = symmetric_imbalance_bucket(imb, n);
            (i, n - 1 - i)
        };
        for pair in obs.windows(2) {
            let (u, u_m) = bucket(&pair[0]);
            let (v, v_m) = bucket(&pair[1]);
            let dm = (0.5 * (pair[1].bid_px + pair[1].ask_px)
                - 0.5 * (pair[0].bid_px + pair[0].ask_px))
                / pair[0].tick_size;
            p[u * n + v] += 1.0;
            p[u_m * n + v_m] += 1.0;
            if dm.abs() > 1e-12 {
                r[u] += dm;
                r[u_m] -= dm;
            }
            counts[u] += 1.0;
            counts[u_m] += 1.0;
        }
        for u in 0..n {
            r[u] /= counts[u];
            for v in 0..n {
                p[u * n + v] /= counts[u];
            }
        }
        // Exact horizon expectation \sum_{k=0}^{250} P^k r.
        let mut exact = r.clone();
        let mut cur = r;
        let mut next = vec![0.0_f64; n];
        for _ in 0..250 {
            mat_vec_mul(n, &p, &cur, &mut next);
            for i in 0..n {
                exact[i] += next[i];
            }
            std::mem::swap(&mut cur, &mut next);
        }

        assert!(
            (table.adjustment_ticks(0, 0).unwrap() - exact[0]).abs() < 1e-6,
            "state 0 fitted={} vs exact={}",
            table.adjustment_ticks(0, 0).unwrap(),
            exact[0]
        );
        assert!(
            (table.adjustment_ticks(1, 0).unwrap() - exact[1]).abs() < 1e-6,
            "state 1 fitted={} vs exact={}",
            table.adjustment_ticks(1, 0).unwrap(),
            exact[1]
        );
    }

    #[test]
    fn markov_microprice_rejects_unconverged_alternating_chain_by_default_and_allows_explicit_finite_horizon()
     {
        let mut alt_obs = Vec::new();
        for i in 0..7_u64 {
            let right = i % 2 == 1;
            let bid = if right { 101.0 } else { 100.0 };
            alt_obs.push(MarkovBookObservation {
                timestamp_ms: 1_000 + i * 100,
                bid_px: bid,
                ask_px: bid + 1.0,
                bid_sz: if right { 3.0 } else { 1.0 },
                ask_sz: if right { 1.0 } else { 3.0 },
                tick_size: 1.0,
            });
        }

        // Exact #675 counterexample: both max_iters=63 and max_iters=64 leave residual=1.0 > tol
        // and MUST fail closed under default `require_convergence: true`.
        for cap in [63_usize, 64_usize] {
            let default_cfg = MarkovMicropriceConfig {
                imbalance_buckets: 2,
                spread_buckets: 1,
                max_iters: cap,
                tol: 1e-9,
                min_transitions_per_state: 2,
                require_convergence: true,
            };
            assert!(MarkovMicropriceTable::fit(&default_cfg, &alt_obs).is_err());

            let opt_out = MarkovMicropriceTable::fit(
                &MarkovMicropriceConfig {
                    require_convergence: false,
                    ..default_cfg
                },
                &alt_obs,
            )
            .unwrap();
            assert!(!opt_out.is_converged());
            assert!((opt_out.residual_inf_norm() - 1.0).abs() < 1e-12);
        }
    }

    #[test]
    fn markov_microprice_dto_rejects_inconsistent_convergence_and_corrupt_geometry_while_failing_closed_on_legacy()
     {
        let obs = synthetic_predictive_book_stream();
        let cfg = MarkovMicropriceConfig {
            imbalance_buckets: 2,
            spread_buckets: 1,
            ..MarkovMicropriceConfig::default()
        };
        let valid = MarkovMicropriceTable::fit(&cfg, &obs).unwrap();
        assert!(
            valid
                .validate_for_production(MAX_PRODUCTION_MICROPRICE_RESIDUAL_TICKS)
                .is_ok()
        );
        assert_eq!(valid.convergence_tol(), cfg.tol);

        let valid_json = serde_json::to_value(&valid).unwrap();
        let roundtrip: MarkovMicropriceTable = serde_json::from_value(valid_json.clone()).unwrap();
        assert_eq!(roundtrip, valid);
        assert!(roundtrip.is_converged());

        // Legacy serialized tables missing `converged` / `convergence_tol` decode as unconverged
        // and fail closed at the production consumer boundary.
        let mut legacy_json = valid_json.clone();
        legacy_json.as_object_mut().unwrap().remove("converged");
        let legacy: MarkovMicropriceTable = serde_json::from_value(legacy_json).unwrap();
        assert!(!legacy.is_converged());
        assert!(
            legacy
                .validate_for_production(MAX_PRODUCTION_MICROPRICE_RESIDUAL_TICKS)
                .is_err()
        );

        // #681 counterexample: fabricated `converged: true` paired with 1-tick residual > tol
        // must fail deserialization immediately.
        let mut forged_residual = valid_json.clone();
        forged_residual["residual_inf_norm"] = serde_json::json!(1.0);
        assert!(serde_json::from_value::<MarkovMicropriceTable>(forged_residual).is_err());

        // Fabricated `converged: true` missing `convergence_tol` (defaults to 0.0) fails deserialization.
        let mut missing_tol = valid_json.clone();
        missing_tol
            .as_object_mut()
            .unwrap()
            .remove("convergence_tol");
        assert!(serde_json::from_value::<MarkovMicropriceTable>(missing_tol).is_err());

        // Loose-tolerance witness (`convergence_tol: 0.5, residual: 0.25`) may satisfy `is_converged()`,
        // but must be rejected by `validate_for_production(MAX_PRODUCTION_MICROPRICE_RESIDUAL_TICKS)`.
        let mut loose_tol = valid_json.clone();
        loose_tol["convergence_tol"] = serde_json::json!(0.5);
        loose_tol["residual_inf_norm"] = serde_json::json!(0.25);
        let loose: MarkovMicropriceTable = serde_json::from_value(loose_tol).unwrap();
        assert!(loose.is_converged());
        assert!(
            loose
                .validate_for_production(MAX_PRODUCTION_MICROPRICE_RESIDUAL_TICKS)
                .is_err()
        );

        // Corrupt geometry or truncated vectors fail deserialization.
        let mut zero_spread = valid_json.clone();
        zero_spread["spread_buckets"] = serde_json::json!(0);
        assert!(serde_json::from_value::<MarkovMicropriceTable>(zero_spread).is_err());

        let mut truncated = valid_json;
        truncated["adjustments_ticks"] = serde_json::json!([0.0]);
        assert!(serde_json::from_value::<MarkovMicropriceTable>(truncated).is_err());
    }
}
