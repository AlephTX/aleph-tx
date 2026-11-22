//! Daily loss circuit breaker whose baseline and breach survive restarts.
//!
//! Without persistence, every restart re-anchors the baseline to the current
//! (already reduced) equity and silently grants a fresh loss budget. The guard
//! stores the UTC-day starting equity and a sticky breach flag; a breached day
//! refuses to start again until the next UTC day.

use anyhow::{Context, Result};
use chrono::{Datelike, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct DailyLossState {
    /// Days since CE in UTC (`chrono::NaiveDate::num_days_from_ce`).
    utc_day: i32,
    start_equity: f64,
    breached: bool,
    /// Intraday high-water mark equity on `utc_day`.
    #[serde(default)]
    peak_equity: f64,
    /// Progressive drawdown brake tier: `0=1.00x, 1=0.75x, 2=0.50x, 3=0.25x, 4=breached (0.00x)`.
    #[serde(default)]
    brake_tier: u8,
}

#[derive(Debug)]
pub struct DailyLossGuard {
    path: Option<PathBuf>,
    limit_pct: f64,
    state: DailyLossState,
    persisted_peak_equity: f64,
}

/// The baseline and limit used by the strategy's daily loss check.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DailyLossSnapshot {
    pub daily_loss_day_start_ms: i64,
    pub daily_loss_start_equity: f64,
    pub daily_loss_limit_pct: f64,
}

/// Current UTC day as days since CE. Allocation-free.
pub fn utc_day_now() -> i32 {
    Utc::now().date_naive().num_days_from_ce()
}

/// Compute next drawdown brake tier (`0..=3`) with `0.05` recovery hysteresis.
fn next_brake_tier(current_tier: u8, r_dd: f64) -> u8 {
    let r = if r_dd.is_finite() { r_dd.max(0.0) } else { 0.0 };
    let escalated = if r >= 0.75 {
        3
    } else if r >= 0.50 {
        2
    } else if r >= 0.25 {
        1
    } else {
        0
    };
    let mut tier = current_tier.min(3);
    if escalated >= tier {
        return escalated;
    }
    if tier == 3 && r < 0.70 {
        tier = 2;
    }
    if tier == 2 && r < 0.45 {
        tier = 1;
    }
    if tier == 1 && r < 0.20 {
        tier = 0;
    }
    tier
}

impl DailyLossGuard {
    /// Load today's baseline from `path`, or start one at `equity`.
    ///
    /// Fails if today's breaker already tripped, or if the stored state is
    /// unreadable (a corrupt file must not reset the loss budget).
    pub fn open(path: Option<PathBuf>, limit_pct: f64, equity: f64, today: i32) -> Result<Self> {
        anyhow::ensure!(
            limit_pct.is_finite() && limit_pct > 0.0 && limit_pct < 1.0,
            "daily loss limit must be in (0, 1)"
        );
        anyhow::ensure!(
            equity.is_finite() && equity > 0.0,
            "daily loss baseline requires positive equity"
        );
        let stored = match &path {
            Some(path) => load(path)?,
            None => None,
        };
        anyhow::ensure!(
            stored.is_none_or(|state| state.utc_day <= today),
            "daily loss state is from a later UTC day; refusing to reset the budget"
        );
        let state = match stored {
            Some(mut state) if state.utc_day == today => {
                anyhow::ensure!(
                    !state.breached,
                    "daily loss limit already breached today (baseline ${:.2}); refusing to start",
                    state.start_equity
                );
                state.peak_equity = state.peak_equity.max(equity);
                let budget_usd = state.start_equity * limit_pct;
                let r_dd = if budget_usd > 0.0 {
                    (state.peak_equity - equity).max(0.0) / budget_usd
                } else {
                    0.0
                };
                state.brake_tier = next_brake_tier(state.brake_tier, r_dd);
                state
            }
            _ => DailyLossState {
                utc_day: today,
                start_equity: equity,
                breached: false,
                peak_equity: equity,
                brake_tier: 0,
            },
        };
        let guard = Self {
            path,
            limit_pct,
            persisted_peak_equity: state.peak_equity,
            state,
        };
        if stored != Some(state) {
            guard.persist()?;
        }
        Ok(guard)
    }

    pub fn start_equity(&self) -> f64 {
        self.state.start_equity
    }

    pub fn peak_equity(&self) -> f64 {
        self.state.peak_equity
    }

    pub fn brake_tier(&self) -> u8 {
        if self.state.breached {
            4
        } else {
            self.state.brake_tier
        }
    }

    /// Real-time progressive sizing scale in `[0.0, 1.0]` (never `> 1.0`).
    pub fn risk_scale(&self) -> f64 {
        if self.state.breached {
            return 0.0;
        }
        match self.state.brake_tier {
            0 => 1.0,
            1 => 0.75,
            2 => 0.50,
            3 => 0.25,
            _ => 0.0,
        }
    }

    /// Intraday drawdown from `peak_equity` as a multiple of today's daily loss budget.
    pub fn drawdown_budget_ratio(&self, equity: f64) -> f64 {
        if !equity.is_finite() {
            return 0.0;
        }
        let budget_usd = self.state.start_equity * self.limit_pct;
        if budget_usd <= 0.0 || !budget_usd.is_finite() {
            return 0.0;
        }
        (self.state.peak_equity - equity).max(0.0) / budget_usd
    }

    pub fn snapshot(&self) -> DailyLossSnapshot {
        let epoch_day = chrono::DateTime::UNIX_EPOCH.date_naive().num_days_from_ce();
        DailyLossSnapshot {
            daily_loss_day_start_ms: (i64::from(self.state.utc_day) - i64::from(epoch_day))
                * 86_400_000,
            daily_loss_start_equity: self.start_equity(),
            daily_loss_limit_pct: self.limit_pct,
        }
    }

    /// Check `equity` against today's baseline, rolling it at UTC midnight.
    /// Returns the fractional PnL; errors once the loss limit is breached.
    pub fn check(&mut self, equity: f64, today: i32) -> Result<f64> {
        anyhow::ensure!(
            today >= self.state.utc_day,
            "UTC day moved backward; refusing to reset the daily loss budget"
        );
        anyhow::ensure!(
            today > self.state.utc_day || !self.state.breached,
            "daily loss limit already breached today"
        );
        if !equity.is_finite() {
            return Ok(0.0); // Non-finite snapshots are rejected upstream.
        }
        if equity <= 0.0 {
            if today > self.state.utc_day {
                self.state.utc_day = today;
            }
            self.state.breached = true;
            self.state.brake_tier = 4;
            // Persist before failing so a wipeout cannot re-arm on restart.
            self.persist()?;
            anyhow::bail!(
                "daily loss limit breached: non-positive equity (${:.2} -> ${:.2})",
                self.state.start_equity,
                equity
            );
        }
        if today > self.state.utc_day {
            self.state = DailyLossState {
                utc_day: today,
                start_equity: equity,
                breached: false,
                peak_equity: equity,
                brake_tier: 0,
            };
            self.persisted_peak_equity = equity;
            self.persist()?;
            return Ok(0.0);
        }
        let pnl_pct = (equity - self.state.start_equity) / self.state.start_equity;
        if pnl_pct < -self.limit_pct {
            self.state.breached = true;
            self.state.brake_tier = 4;
            // Persist before failing so a restart cannot re-arm the budget.
            self.persist()?;
            anyhow::bail!(
                "daily loss limit breached: {:.2}% (${:.2} -> ${:.2})",
                pnl_pct * 100.0,
                self.state.start_equity,
                equity
            );
        }
        self.state.peak_equity = self.state.peak_equity.max(equity);
        let r_dd = self.drawdown_budget_ratio(equity);
        let new_tier = next_brake_tier(self.state.brake_tier, r_dd);
        let tier_changed = new_tier != self.state.brake_tier;
        self.state.brake_tier = new_tier;
        let peak_step = 0.10 * self.state.start_equity * self.limit_pct;
        if tier_changed || (self.state.peak_equity - self.persisted_peak_equity) >= peak_step {
            self.persisted_peak_equity = self.state.peak_equity;
            self.persist()?;
        }
        Ok(pnl_pct)
    }

    fn persist(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        if let Some(dir) = parent {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut opts = OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            opts.mode(0o600);
            let mut file = opts.open(&tmp)?;
            file.write_all(&serde_json::to_vec(&self.state)?)?;
            file.sync_all()?;
        }
        fs::rename(&tmp, path)?;
        File::open(parent.unwrap_or(Path::new(".")))?.sync_all()?;
        Ok(())
    }
}

fn load(path: &Path) -> Result<Option<DailyLossState>> {
    match fs::read(path) {
        Ok(bytes) => {
            let mut state: DailyLossState = serde_json::from_slice(&bytes)
                .with_context(|| format!("corrupt daily loss state {}", path.display()))?;
            anyhow::ensure!(
                state.start_equity.is_finite() && state.start_equity > 0.0,
                "invalid daily loss baseline in {}",
                path.display()
            );
            anyhow::ensure!(
                state.peak_equity.is_finite() && state.peak_equity >= 0.0,
                "invalid peak equity in {}",
                path.display()
            );
            if state.peak_equity == 0.0 {
                // Upgrade legacy 3-field state files seamlessly.
                state.peak_equity = state.start_equity;
            } else {
                anyhow::ensure!(
                    state.peak_equity >= state.start_equity * (1.0 - 1e-12),
                    "invalid peak equity below start equity in {}",
                    path.display()
                );
                state.peak_equity = state.peak_equity.max(state.start_equity);
            }
            anyhow::ensure!(
                state.brake_tier <= 4,
                "invalid brake tier {} in {}",
                state.brake_tier,
                path.display()
            );
            if state.breached {
                state.brake_tier = 4;
            } else {
                anyhow::ensure!(
                    state.brake_tier <= 3,
                    "inconsistent unbreached brake tier 4 in {}",
                    path.display()
                );
            }
            Ok(Some(state))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests;
