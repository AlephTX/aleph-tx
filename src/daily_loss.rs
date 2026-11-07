//! Daily loss circuit breaker whose baseline and breach survive restarts.
//!
//! Without persistence, every restart re-anchors the baseline to the current
//! (already reduced) equity and silently grants a fresh loss budget. The guard
//! stores the UTC-day starting equity and a sticky breach flag; a breached day
//! refuses to start again until the next UTC day.

use anyhow::{Context, Result};
use chrono::{Datelike, Utc};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct DailyLossState {
    /// Days since CE in UTC (`chrono::NaiveDate::num_days_from_ce`).
    utc_day: i32,
    start_equity: f64,
    breached: bool,
}

#[derive(Debug)]
pub struct DailyLossGuard {
    path: Option<PathBuf>,
    limit_pct: f64,
    state: DailyLossState,
}

/// Current UTC day as days since CE. Allocation-free.
pub fn utc_day_now() -> i32 {
    Utc::now().date_naive().num_days_from_ce()
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
        let state = match stored {
            Some(state) if state.utc_day == today => {
                anyhow::ensure!(
                    !state.breached,
                    "daily loss limit already breached today (baseline ${:.2}); refusing to start",
                    state.start_equity
                );
                state
            }
            _ => DailyLossState {
                utc_day: today,
                start_equity: equity,
                breached: false,
            },
        };
        let guard = Self {
            path,
            limit_pct,
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

    /// Check `equity` against today's baseline, rolling it at UTC midnight.
    /// Returns the fractional PnL; errors once the loss limit is breached.
    pub fn check(&mut self, equity: f64, today: i32) -> Result<f64> {
        if !equity.is_finite() || equity <= 0.0 {
            return Ok(0.0); // Unusable snapshots are rejected upstream.
        }
        if today != self.state.utc_day {
            self.state = DailyLossState {
                utc_day: today,
                start_equity: equity,
                breached: false,
            };
            self.persist()?;
            return Ok(0.0);
        }
        let pnl_pct = (equity - self.state.start_equity) / self.state.start_equity;
        if pnl_pct < -self.limit_pct {
            self.state.breached = true;
            // Persist before failing so a restart cannot re-arm the budget.
            self.persist()?;
            anyhow::bail!(
                "daily loss limit breached: {:.2}% (${:.2} -> ${:.2})",
                pnl_pct * 100.0,
                self.state.start_equity,
                equity
            );
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
            let mut file = File::create(&tmp)?;
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
            let state: DailyLossState = serde_json::from_slice(&bytes)
                .with_context(|| format!("corrupt daily loss state {}", path.display()))?;
            anyhow::ensure!(
                state.start_equity.is_finite() && state.start_equity > 0.0,
                "invalid daily loss baseline in {}",
                path.display()
            );
            Ok(Some(state))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests;
