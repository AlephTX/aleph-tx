use aleph_tx::config::{AppConfig, EXCH_LIGHTER, InventoryNeutralMMConfig, symbol_name};
use aleph_tx::exchanges::hyperliquid::{HyperliquidSigner, Network, validate_hex};
use aleph_tx::lighter_trading::{LighterCredentials, LighterTrading};
use aleph_tx::live_hedge::{HedgeLimits, LiveInventoryHedge};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::{Child, Command};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
struct FeederMarket {
    #[serde(default)]
    feeder_enabled: bool,
    #[serde(default)]
    feeder_ws_url: String,
    #[serde(default)]
    feeder_symbols: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Feeds {
    lighter: FeederMarket,
    hyperliquid: Option<FeederMarket>,
    inventory_hedge: Option<HedgeLimits>,
}

pub(super) struct Profile {
    pub strategy: InventoryNeutralMMConfig,
    pub network: Network,
    feeds: Feeds,
    feeder_config: String,
}

impl Profile {
    pub fn load(path: &Path) -> Result<Self> {
        let contents =
            fs::read_to_string(path).with_context(|| format!("Read {}", path.display()))?;
        let feeds: Feeds = toml::from_str(&contents)?;
        let (network, hedge_ws) = match feeds.lighter.feeder_ws_url.as_str() {
            "wss://mainnet.zklighter.elliot.ai/stream" => {
                (Network::Mainnet, "wss://api.hyperliquid.xyz/ws")
            }
            "wss://testnet.zklighter.elliot.ai/stream" => {
                (Network::Testnet, "wss://api.hyperliquid-testnet.xyz/ws")
            }
            _ => anyhow::bail!("Unsupported Lighter websocket endpoint"),
        };
        let app: AppConfig = toml::from_str(&contents)?;
        app.validate()?;
        let strategy = app
            .inventory_neutral_mm
            .context("Missing inventory_neutral_mm section")?;
        let symbol = symbol_name(strategy.symbol_id);
        ensure!(
            strategy.exchange_id == EXCH_LIGHTER && symbol != "UNKNOWN",
            "Inventory strategy requires a supported Lighter symbol (BTC or ETH)"
        );
        ensure!(
            feeds.lighter.feeder_enabled
                && feeds.lighter.feeder_symbols.len() == 1
                && feeds
                    .lighter
                    .feeder_symbols
                    .get(symbol)
                    .is_some_and(|market| market == &strategy.market_id.to_string()),
            "Lighter feeder must publish exactly the strategy's symbol and market ID"
        );
        let has_hl = feeds
            .hyperliquid
            .as_ref()
            .is_some_and(|hl| hl.feeder_enabled);
        if let Some(hl) = feeds.hyperliquid.as_ref().filter(|hl| hl.feeder_enabled) {
            ensure!(
                hl.feeder_symbols
                    .get(symbol)
                    .is_some_and(|market| market == symbol)
                    && hl
                        .feeder_symbols
                        .iter()
                        .all(|(ticker, market)| matches!(ticker.as_str(), "ETH" | "BTC")
                            && market == ticker),
                "Hyperliquid feeder must map the strategy symbol to its native perpetual"
            );
            ensure!(
                hl.feeder_ws_url == hedge_ws,
                "Hyperliquid feeder network differs from Lighter"
            );
        }
        if let Some(hedge) = &feeds.inventory_hedge {
            hedge.validate()?;
            ensure!(
                has_hl && strategy.use_post_only,
                "Inventory hedge requires the Hyperliquid feeder and post-only primary quotes"
            );
            ensure!(
                strategy.max_spread_bps
                    >= 2.0 * strategy.maker_fee_bps
                        + strategy.min_profit_bps
                        + hedge.round_trip_cost_bps(),
                "Maximum strategy spread is below the configured round-trip cost"
            );
        }
        // The supervised quote path accepts only the matched Lighter and
        // Hyperliquid books as pricing inputs. Serialize that exact validated
        // profile to the private session directory before the feeder starts.
        let mut feeder: toml::Table = toml::from_str(&contents)?;
        feeder.retain(|key, _| matches!(key, "lighter" | "hyperliquid"));
        let feeder_config = toml::to_string(&feeder)?;
        Ok(Self {
            strategy,
            network,
            feeds,
            feeder_config,
        })
    }

    pub fn has_hedge(&self) -> bool {
        self.feeds.inventory_hedge.is_some()
    }

    pub fn require_network(&self, expected: Network) -> Result<()> {
        ensure!(
            self.network == expected,
            "Launch mode requires a {expected:?} profile, found {:?}",
            self.network
        );
        Ok(())
    }
}

const LIGHTER_ENV: &[&str] = &[
    "LIGHTER_PRIVATE_KEY",
    "API_KEY_PRIVATE_KEY",
    "LIGHTER_ACCOUNT_INDEX",
    "LIGHTER_API_KEY_INDEX",
    "LIGHTER_BASE_URL",
    "LIGHTER_CHAIN_ID",
];
const HEDGE_ACCOUNT: &str = "HYPERLIQUID_ACCOUNT_ADDRESS";
const HEDGE_KEY: &str = "HYPERLIQUID_AGENT_PRIVATE_KEY";

struct HedgeSetup {
    limits: HedgeLimits,
    account: String,
    signer: HyperliquidSigner,
}

// Use the non-mutating API so neither dotenv nor a failed profile can change
// environment variables in an already multithreaded runtime.
#[allow(deprecated)]
fn read_env(path: &Path) -> Result<HashMap<String, String>> {
    let mut values = HashMap::new();
    if path != Path::new("-") {
        let entries =
            dotenv::from_path_iter(path).with_context(|| format!("Open {}", path.display()))?;
        for entry in entries {
            // Dotenv parse errors can contain the original secret-bearing line.
            let (key, value) = entry
                .map_err(|_| anyhow::anyhow!("Invalid dotenv syntax in {}", path.display()))?;
            if LIGHTER_ENV.contains(&key.as_str()) || key == HEDGE_ACCOUNT || key == HEDGE_KEY {
                ensure!(
                    values.insert(key, value).is_none(),
                    "Duplicate trading credential in {}",
                    path.display()
                );
            }
        }
    }
    for key in LIGHTER_ENV
        .iter()
        .copied()
        .chain([HEDGE_ACCOUNT, HEDGE_KEY])
    {
        if !values.contains_key(key)
            && let Ok(value) = std::env::var(key)
        {
            values.insert(key.to_owned(), value);
        }
    }
    Ok(values)
}

pub(super) fn require_platform() -> Result<()> {
    ensure!(
        cfg!(all(target_os = "linux", target_arch = "x86_64")),
        "Live trading requires Linux x86-64 with the bundled Lighter signer"
    );
    Ok(())
}

pub(super) fn lock_account(credentials: &LighterCredentials, network: Network) -> Result<File> {
    account_lock(&format!(
        "/dev/shm/aleph-tx-lighter-{network:?}-{}.lock",
        credentials.account_index()
    ))
}

fn account_lock(path: &str) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    file.try_lock()
        .context("Another AlephTX process already owns this trading account")?;
    Ok(file)
}

struct Session {
    ipc: PathBuf,
    active: PathBuf,
    journal: File,
    id: Uuid,
}

impl Session {
    fn new(state_dir: &Path, ipc_root: &Path) -> Result<Self> {
        fs::create_dir_all(state_dir)?;
        let id = Uuid::new_v4();
        let ipc = ipc_root.join(format!("aleph-tx-{id}"));
        let active = state_dir.join("aleph-trade.active.jsonl");
        let journal = OpenOptions::new()
            .create_new(true)
            .append(true)
            .mode(0o600)
            .open(&active)
            .with_context(|| {
                format!(
                    "Cannot reserve {}; reconcile the previous session if this file exists",
                    active.display()
                )
            })?;
        let initialized = File::open(state_dir)
            .and_then(|dir| dir.sync_all())
            .and_then(|()| DirBuilder::new().mode(0o700).create(&ipc));
        if let Err(error) = initialized {
            fs::remove_file(&active)?;
            return Err(error.into());
        }
        Ok(Self {
            ipc,
            active,
            journal,
            id,
        })
    }

    fn write_start(
        &mut self,
        network: Network,
        account: i64,
        hedge_account: Option<&str>,
        symbol: &str,
    ) -> Result<()> {
        serde_json::to_writer(
            &mut self.journal,
            &serde_json::json!({
                "event": "start", "session_id": self.id.to_string(), "network": format!("{network:?}"),
                "lighter_account_index": account, "hyperliquid_account": hedge_account, "symbol": symbol
            }),
        )?;
        self.journal.write_all(b"\n")?;
        self.journal.sync_all()?;
        Ok(())
    }

    fn archive(&self) -> Result<()> {
        let archive = self
            .active
            .with_file_name(format!("aleph-trade-{}.jsonl", self.id));
        fs::rename(&self.active, archive)?;
        if let Some(dir) = self.active.parent() {
            File::open(dir)?.sync_all()?;
        }
        Ok(())
    }

    fn discard_unstarted(&self) -> Result<()> {
        fs::remove_file(&self.active)?;
        File::open(
            self.active
                .parent()
                .context("Missing session state directory")?,
        )?
        .sync_all()?;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.ipc)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(path=%self.ipc.display(), %error, "Could not remove owned IPC directory");
        }
    }
}

pub(super) async fn launch(
    profile: Profile,
    expected: Network,
    dotenv: &Path,
    feeder_bin: &Path,
) -> Result<()> {
    profile.require_network(expected)?;
    require_platform()?;
    let env = read_env(dotenv)?;
    let credentials = LighterCredentials::from_env(|key| env.get(key).cloned())?;
    let network = credentials.network()?;
    ensure!(
        profile.network == network,
        "Lighter feeder and signer must use the same network"
    );
    let hedge = profile
        .feeds
        .inventory_hedge
        .map(|limits| -> Result<HedgeSetup> {
            let account = env
                .get(HEDGE_ACCOUNT)
                .context("Missing HYPERLIQUID_ACCOUNT_ADDRESS")?
                .clone();
            validate_hex(&account, 20)?;
            let signer = HyperliquidSigner::from_private_key(
                env.get(HEDGE_KEY)
                    .context("Missing HYPERLIQUID_AGENT_PRIVATE_KEY")?,
            )
            .map_err(|_| anyhow::anyhow!("Invalid HYPERLIQUID_AGENT_PRIVATE_KEY"))?;
            Ok(HedgeSetup {
                limits,
                account,
                signer,
            })
        })
        .transpose()?;
    let _account_lock = lock_account(&credentials, network)?;
    let _hedge_lock = hedge
        .as_ref()
        .map(|setup| {
            account_lock(&format!(
                "/dev/shm/aleph-tx-hyperliquid-{network:?}-{}.lock",
                setup.account.to_ascii_lowercase()
            ))
        })
        .transpose()?;
    let account = credentials.account_index();
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;

    let mut session = Session::new(Path::new("pids"), Path::new("/dev/shm"))?;
    let feeder_config = session.ipc.join("config.toml");
    if let Err(error) = fs::write(&feeder_config, profile.feeder_config) {
        session.discard_unstarted()?;
        return Err(error.into());
    }
    let mut feeder = Command::new(feeder_bin);
    for key in LIGHTER_ENV {
        if let Some(value) = env.get(*key) {
            feeder.env(key, value);
        }
    }
    let feeder = feeder
        .env("ALEPH_FEEDER_CONFIG", &feeder_config)
        .env("ALEPH_SHM_DIR", &session.ipc)
        .env_remove("ALEPH_SHM")
        .env_remove("ALEPH_FEEDER_PUBLIC_ONLY")
        .env_remove(HEDGE_KEY)
        .env_remove(HEDGE_ACCOUNT)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Start feeder {}", feeder_bin.display()));
    let mut feeder = match feeder {
        Ok(child) => child,
        Err(error) => {
            session.discard_unstarted()?;
            return Err(error);
        }
    };

    let (stop, shutdown) = watch::channel(false);
    let mut started = false;
    let mut verified_flat = false;
    let result = supervise(
        &mut feeder,
        &stop,
        async {
            let symbol = symbol_name(profile.strategy.symbol_id);
            let mut trading =
                LighterTrading::with_credentials(profile.strategy.market_id, credentials).await?;
            trading.validate_symbol(symbol)?;
            trading.validate_precision(profile.strategy.tick_size, profile.strategy.step_size)?;
            trading.set_supervised_quote_expiry();
            let (orders, position) =
                tokio::try_join!(trading.get_active_orders(), trading.signed_position())?;
            ensure!(
                orders.is_empty() && position.is_zero(),
                "Lighter market must be flat with no active orders before supervised startup"
            );
            session.write_start(
                network,
                account,
                hedge.as_ref().map(|setup| setup.account.as_str()),
                symbol,
            )?;
            let (mut strategy, trading) =
                super::connect_strategy(profile.strategy, trading, network, &session.ipc).await?;
            if let Some(setup) = hedge {
                strategy = strategy.with_inventory_hedge(
                    LiveInventoryHedge::connect(
                        setup.limits,
                        Arc::clone(&trading),
                        network,
                        symbol,
                        setup.account,
                        setup.signer,
                        session.journal.try_clone()?,
                    )
                    .await?,
                );
            }
            if *shutdown.borrow() {
                return Ok(());
            }
            started = true;
            strategy.run(Some(shutdown)).await?;
            let (orders, position) =
                tokio::try_join!(trading.get_active_orders(), trading.signed_position())?;
            ensure!(
                orders.is_empty() && position.is_zero(),
                "Lighter shutdown verification found residual exposure"
            );
            verified_flat = true;
            Ok(())
        },
        async {
            tokio::select! { _ = sigint.recv() => {}, _ = sigterm.recv() => {} }
        },
    )
    .await;

    if !started {
        session.discard_unstarted()?;
    } else if verified_flat {
        session
            .journal
            .write_all(b"{\"event\":\"verified_flat\"}\n")?;
        session.journal.sync_all()?;
        session.archive()?;
    } else {
        tracing::error!(journal=%session.active.display(), "Trading stopped with a session that requires reconciliation");
    }
    result
}

async fn supervise(
    feeder: &mut Child,
    stop: &watch::Sender<bool>,
    engine: impl Future<Output = Result<()>>,
    shutdown_signal: impl Future<Output = ()>,
) -> Result<()> {
    let mut engine = std::pin::pin!(engine);
    let result = tokio::select! {
        // Never drop the engine future while a submission is in flight.
        status = feeder.wait() => {
            let _ = stop.send(true);
            let cleanup = engine.await;
            match cleanup {
                Ok(()) => Err(anyhow::anyhow!("Feeder exited before the strategy: {status:?}")),
                Err(err) => Err(err.context(format!("Feeder exited before the strategy: {status:?}"))),
            }
        }
        () = shutdown_signal => {
            tracing::info!("Shutdown requested; keeping feeder alive until strategy cleanup returns");
            let _ = stop.send(true);
            engine.await
        }
        result = &mut engine => result,
    };
    if feeder.try_wait()?.is_none() {
        feeder.kill().await?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::pending;

    #[test]
    fn offline_profile_accepts_example_and_rejects_cross_network_books() {
        let example = include_str!("../../../config.example.toml");
        let path = std::env::temp_dir().join(format!("aleph-profile-{}.toml", Uuid::new_v4()));
        fs::write(&path, example).unwrap();
        let profile = Profile::load(&path).unwrap();
        assert_eq!(profile.network, Network::Mainnet);
        assert!(!profile.has_hedge());
        fs::write(
            &path,
            example.replace("api.hyperliquid.xyz/ws", "api.hyperliquid-testnet.xyz/ws"),
        )
        .unwrap();
        assert!(Profile::load(&path).is_err());
        fs::write(&path, example.replace("ETH = \"0\"", "ETH = \"1\"")).unwrap();
        assert!(Profile::load(&path).is_err());
        assert!(profile.require_network(Network::Testnet).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn testnet_profile_preserves_both_wide_market_ids_and_blocks_mainnet_mode() {
        let example = include_str!("../../../config.example.toml")
            .replace(
                "mainnet.zklighter.elliot.ai/stream",
                "testnet.zklighter.elliot.ai/stream",
            )
            .replace("api.hyperliquid.xyz/ws", "api.hyperliquid-testnet.xyz/ws");
        let path =
            std::env::temp_dir().join(format!("aleph-testnet-profile-{}.toml", Uuid::new_v4()));
        for (symbol, symbol_id, market_id) in [("ETH", 1002, 4095), ("BTC", 1001, 4096)] {
            fs::write(
                &path,
                example
                    .replace("ETH = \"0\"", &format!("{symbol} = \"{market_id}\""))
                    .replace("market_id = 0", &format!("market_id = {market_id}"))
                    .replace(
                        "symbol_id = 1002         # ETH-PERP",
                        &format!("symbol_id = {symbol_id}"),
                    ),
            )
            .unwrap();
            let profile = Profile::load(&path).unwrap();
            assert_eq!(profile.strategy.market_id, market_id);
            assert_eq!(profile.strategy.symbol_id, symbol_id);
            assert!(profile.require_network(Network::Testnet).is_ok());
            assert!(profile.require_network(Network::Mainnet).is_err());
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn interrupted_session_blocks_restart_and_only_removes_its_ipc() {
        let root = std::env::temp_dir().join(format!("aleph-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let state = root.join("pids");
        let mut session = Session::new(&state, &root).unwrap();
        session
            .write_start(Network::Testnet, 1, None, "ETH")
            .unwrap();
        let ipc = session.ipc.clone();
        let sentinel = root.join("unrelated-feeder");
        fs::write(&sentinel, "alive").unwrap();
        assert!(Session::new(&state, &root).is_err());
        drop(session);
        assert!(!ipc.exists());
        assert!(sentinel.is_file());
        assert!(Session::new(&state, &root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn supervisor_signals_strategy_and_waits_for_cleanup_before_reaping_feeder() {
        let mut feeder = Command::new("sleep")
            .arg("20")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (stop, mut shutdown) = watch::channel(false);
        supervise(
            &mut feeder,
            &stop,
            async {
                shutdown.changed().await?;
                assert!(*shutdown.borrow());
                Ok(())
            },
            async {},
        )
        .await
        .unwrap();
        assert!(feeder.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn supervisor_fails_if_feeder_exits_and_still_allows_cleanup() {
        let mut feeder = Command::new("false").spawn().unwrap();
        let (stop, mut shutdown) = watch::channel(false);
        let error = supervise(
            &mut feeder,
            &stop,
            async {
                shutdown.changed().await?;
                assert!(*shutdown.borrow());
                Ok(())
            },
            pending(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("Feeder exited"));
    }
}
