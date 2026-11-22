use super::*;
use aleph_tx::strategy::arcus_xyz_basis::lock_state_file;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub(super) fn account_scopes(network: &str, arc: &str, index: u8, hl: &str) -> [String; 2] {
    [
        format!("{network}/arcus/{}/{index}", arc.to_ascii_lowercase()),
        format!("{network}/hl/{}", hl.to_ascii_lowercase()),
    ]
    .map(|s| hex::encode(Sha256::digest(s.as_bytes())))
}

fn account_state_root() -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    let root = PathBuf::from("/var/lib/aleph-tx-arcus-xyz");
    #[cfg(not(target_os = "linux"))]
    let root = PathBuf::from(env::var_os("HOME").context("missing home directory")?)
        .join(".local/state/aleph-tx-arcus-xyz");
    Ok(root)
}

pub(super) fn account_locks(scopes: &[String; 2]) -> Result<Vec<File>> {
    let root = account_state_root()?;
    scopes
        .iter()
        .map(|scope| lock_state_file(&root.join("locks").join(scope)))
        .collect()
}

/// Retain the location of the private risk evidence across sessions. The
/// process locks serialize first use, including a CLI racing the service.
pub(super) struct StateBinding {
    marker: PathBuf,
    locations: String,
}
impl StateBinding {
    pub fn check(cfg: &ArcusXyzBasisConfig, scopes: &[String; 2]) -> Result<Option<Self>> {
        Self::check_at(&account_state_root()?, cfg, scopes)
    }

    fn check_at(
        root: &Path,
        cfg: &ArcusXyzBasisConfig,
        scopes: &[String; 2],
    ) -> Result<Option<Self>> {
        let identity = serde_json::to_vec(&(scopes, &cfg.arcus_symbol, &cfg.hl_dex, &cfg.hl_coin))?;
        let marker = root
            .join("bindings")
            .join(hex::encode(Sha256::digest(identity)));
        let canonical = |p: &Path| -> Result<PathBuf> {
            let parent = p
                .parent()
                .filter(|v| !v.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            Ok(parent
                .canonicalize()?
                .join(p.file_name().context("invalid state file path")?))
        };
        let paths = [
            canonical(&cfg.journal_path)?,
            canonical(&cfg.daily_loss_state_path)?,
        ];
        let locations = hex::encode(Sha256::digest(serde_json::to_vec(&paths)?));
        match fs::read_to_string(&marker) {
            Ok(stored) => {
                ensure!(
                    stored == locations,
                    "this account and market already have a different journal or daily loss path; use the original state"
                );
                ensure!(
                    fs::metadata(&cfg.journal_path)?.len() > 0
                        && cfg.daily_loss_state_path.is_file()
                        && EquityBasis::path(cfg).is_file(),
                    "previously bound risk evidence is missing; reconcile before restart"
                );
                Ok(None)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Ok(Some(Self { marker, locations }))
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn record(self) -> Result<()> {
        let parent = self.marker.parent().context("invalid binding path")?;
        fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&self.marker)?;
        file.write_all(self.locations.as_bytes())?;
        file.sync_all()?;
        File::open(parent)?.sync_all()?;
        File::open(parent.parent().context("invalid account state directory")?)?.sync_all()?;
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
pub(super) struct EquityBasis {
    scope: String,
    allocated: f64,
    limit_pct: f64,
    #[serde(with = "rust_decimal::serde::str")]
    pub initial_hl_equity: Decimal,
    pub arc: ArcusLegPnlTracker,
}

impl EquityBasis {
    fn path(cfg: &ArcusXyzBasisConfig) -> PathBuf {
        let mut path = cfg.daily_loss_state_path.as_os_str().to_owned();
        path.push(".basis");
        path.into()
    }

    pub fn restore(cfg: &ArcusXyzBasisConfig, scopes: &[String; 2], hl: Decimal) -> Result<Self> {
        let scope = format!(
            "{}/{}/{}/{}/{}",
            scopes[0], scopes[1], cfg.arcus_symbol, cfg.hl_dex, cfg.hl_coin
        );
        match fs::read(Self::path(cfg)) {
            Ok(bytes) => {
                let state: Self = serde_json::from_slice(&bytes)?;
                ensure!(
                    state.scope == scope
                        && state.allocated == cfg.allocated_capital_usd
                        && state.limit_pct == cfg.daily_loss_limit_pct
                        && state.arc.position().is_zero(),
                    "equity checkpoint does not match flat startup scope and allocation"
                );
                ensure!(
                    cfg.daily_loss_state_path.try_exists()?,
                    "equity checkpoint missing its daily loss state"
                );
                Ok(state)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    !cfg.daily_loss_state_path.try_exists()?,
                    "existing guard lacks an attributable equity checkpoint; reconcile before restart"
                );
                let state = Self {
                    scope,
                    allocated: cfg.allocated_capital_usd,
                    limit_pct: cfg.daily_loss_limit_pct,
                    initial_hl_equity: hl,
                    arc: ArcusLegPnlTracker::new(Decimal::ZERO, None),
                };
                state.persist(cfg)?;
                Ok(state)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn equity(&self, hl: Decimal, mid: Decimal) -> Result<(f64, f64, f64)> {
        let delta = (hl - self.initial_hl_equity)
            .to_f64()
            .context("invalid HL equity delta")?;
        let arc = self
            .arc
            .estimated_total_pnl_usd(mid.to_f64().context("invalid mark")?)
            .context("Arcus cost basis unavailable")?;
        let total = self.allocated + delta + arc;
        ensure!(total.is_finite(), "combined valuation unavailable");
        Ok((total, delta, arc))
    }

    pub fn persist(&self, cfg: &ArcusXyzBasisConfig) -> Result<()> {
        let path = Self::path(cfg);
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(parent)?;
        let tmp = parent.join(format!(".basis-{}", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&tmp)?;
        file.write_all(&serde_json::to_vec(self)?)?;
        file.sync_all()?;
        fs::rename(tmp, &path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_restart_cannot_substitute_a_new_journal_or_daily_guard() {
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        fs::create_dir_all(&root).unwrap();
        let mut cfg: ArcusXyzBasisConfig =
            toml::from_str(include_str!("../../../config.arcus_xyz_basis.example.toml")).unwrap();
        cfg.journal_path = root.join("journal");
        cfg.daily_loss_state_path = root.join("guard");
        let scope = account_scopes("testnet", "arc", 0, "hl");
        let binding = StateBinding::check_at(&root, &cfg, &scope)
            .unwrap()
            .unwrap();
        fs::write(&cfg.journal_path, b"synthetic").unwrap();
        fs::write(&cfg.daily_loss_state_path, b"synthetic").unwrap();
        fs::write(EquityBasis::path(&cfg), b"synthetic").unwrap();
        binding.record().unwrap();
        assert!(
            StateBinding::check_at(&root, &cfg, &scope)
                .unwrap()
                .is_none()
        );
        let mut other = cfg.clone();
        other.daily_loss_state_path = root.join("alternate");
        assert!(StateBinding::check_at(&root, &other, &scope).is_err());
        other.daily_loss_state_path = cfg.daily_loss_state_path.clone();
        other.journal_path = root.join("alternate");
        assert!(StateBinding::check_at(&root, &other, &scope).is_err());
        fs::remove_file(EquityBasis::path(&cfg)).unwrap();
        assert!(StateBinding::check_at(&root, &cfg, &scope).is_err());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn shared_venue_account_excludes_an_independent_runner_until_drop() {
        let a = account_scopes("mainnet", "SYN-ARC", 0, "SYN-HL");
        let b = account_scopes("mainnet", "OTHER-ARC", 0, "syn-hl");
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        let locks: Vec<_> = a
            .iter()
            .map(|key| lock_state_file(&root.join(key)).unwrap())
            .collect();
        assert!(lock_state_file(&root.join(&b[1])).is_err());
        drop(locks);
        assert!(lock_state_file(&root.join(&b[1])).is_ok());
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn two_sessions_share_one_daily_loss_budget() {
        let mut cfg: ArcusXyzBasisConfig =
            toml::from_str(include_str!("../../../config.arcus_xyz_basis.example.toml")).unwrap();
        let root = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        cfg.daily_loss_state_path = root.join("guard");
        let scope = account_scopes("testnet", "synthetic", 0, "synthetic");
        let mut basis = EquityBasis::restore(&cfg, &scope, Decimal::from(100)).unwrap();
        let path = Some(cfg.daily_loss_state_path.clone());
        let mut guard = DailyLossGuard::open(path.clone(), 0.05, 500., 1).unwrap();
        basis
            .arc
            .record_fill(true, Decimal::ONE, Decimal::from(100), 0.);
        // A hedged price move washes out while a single-leg move changes guard equity.
        assert_eq!(
            basis
                .equity(Decimal::from(90), Decimal::from(110))
                .unwrap()
                .0,
            500.
        );
        assert_eq!(
            basis
                .equity(Decimal::from(90), Decimal::from(100))
                .unwrap()
                .0,
            490.
        );
        basis
            .arc
            .record_fill(false, Decimal::ONE, Decimal::from(90), 0.);
        let eq = basis.equity(Decimal::from(90), Decimal::ZERO).unwrap().0;
        assert_eq!(eq, 480.);
        guard.check(eq, 1).unwrap();
        basis.persist(&cfg).unwrap();
        let resumed = EquityBasis::restore(&cfg, &scope, Decimal::from(90)).unwrap();
        let mut guard = DailyLossGuard::open(
            path.clone(),
            0.05,
            resumed.equity(Decimal::from(90), Decimal::ZERO).unwrap().0,
            1,
        )
        .unwrap();
        assert!(
            guard
                .check(
                    resumed.equity(Decimal::from(80), Decimal::ZERO).unwrap().0,
                    1
                )
                .is_err()
        );
        assert!(DailyLossGuard::open(path.clone(), 0.05, 500., 1).is_err());
        assert!(
            EquityBasis::restore(
                &cfg,
                &account_scopes("testnet", "other", 0, "synthetic"),
                Decimal::ZERO
            )
            .is_err()
        );
        fs::remove_file(EquityBasis::path(&cfg)).unwrap();
        assert!(EquityBasis::restore(&cfg, &scope, Decimal::from(100)).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
