use crate::backpack_api::client::BackpackClient;
use crate::backpack_api::model::*;
use crate::config::ExchangeConfig;
use crate::shm_reader::ShmBboMessage;
use crate::strategy::Strategy;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tracing::{error, info, warn};

pub struct BackpackMMStrategy {
    exchange_id: u8,
    symbol_id: u16,
    cfg: ExchangeConfig,
    api_client: Option<Arc<BackpackClient>>,

    // Price tracking
    last_mid: f64,
    last_quoted_mid: f64,
    last_update: Option<Instant>,

    // Volatility ring buffer
    mid_history: VecDeque<f64>,

    // Dynamic balance-based limits (refreshed periodically)
    max_position: f64,
    base_size: f64,
    stop_loss_usd: f64,
    last_balance_refresh: Option<Instant>,
    account_equity_usdc: f64,
}

impl BackpackMMStrategy {
    pub fn new(
        exchange_id: u8,
        symbol_id: u16,
        _half_spread_bps: f64,
        cfg: ExchangeConfig,
    ) -> Self {
        let env_path = std::env::var("BACKPACK_ENV_PATH").unwrap_or_else(|_| {
            "/home/metaverse/.openclaw/workspace/aleph-tx/.env.backpack".to_string()
        });
        let env_str = std::fs::read_to_string(&env_path).unwrap_or_default();
        let mut api_key = String::new();
        let mut api_secret = String::new();

        for line in env_str.lines() {
            if let Some(rest) = line.strip_prefix("BACKPACK_PUBLIC_KEY=") {
                api_key = rest.trim().to_string();
            }
            if let Some(rest) = line.strip_prefix("BACKPACK_SECRET_KEY=") {
                api_secret = rest.trim().to_string();
            }
        }

        let api_client = if !api_key.is_empty() && !api_secret.is_empty() {
            match BackpackClient::new(&api_key, &api_secret, "https://api.backpack.exchange") {
                Ok(client) => {
                    info!("🎒 Loaded Backpack API Client (v3 — dynamic allocation)");
                    Some(Arc::new(client))
                }
                Err(e) => {
                    warn!("Failed to init Backpack Client: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let vol_window = cfg.vol_window;
        Self {
            exchange_id,
            symbol_id,
            cfg,
            api_client,
            last_mid: 0.0,
            last_quoted_mid: 0.0,
            last_update: None,
            mid_history: VecDeque::with_capacity(vol_window + 1),
            max_position: 0.3,  // will be overwritten by balance fetch
            base_size: 0.05,    // will be overwritten
            stop_loss_usd: 5.0, // will be overwritten
            last_balance_refresh: None,
            account_equity_usdc: 0.0,
        }
    }

    fn symbol_name(&self) -> &str {
        if self.symbol_id == 1001 {
            "BTC_USDC_PERP"
        } else {
            "ETH_USDC_PERP"
        }
    }

    fn realized_vol_bps(&self) -> f64 {
        if self.mid_history.len() < 10 {
            return 20.0;
        }
        let returns: Vec<f64> = self
            .mid_history
            .iter()
            .zip(self.mid_history.iter().skip(1))
            .map(|(prev, cur)| ((cur - prev) / prev) * 10_000.0)
            .collect();
        let mean = returns.iter().sum::<f64>() / returns.len() as f64;
        let variance =
            returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / returns.len() as f64;
        variance.sqrt()
    }

    fn momentum_bps(&self) -> f64 {
        if self.mid_history.len() < 5 {
            return 0.0;
        }
        let recent = self.mid_history.back().unwrap();
        let lookback = self.mid_history.iter().rev().nth(4).unwrap();
        (recent - lookback) / lookback * 10_000.0
    }

    /// Refresh account balance and recompute dynamic limits
    fn maybe_refresh_balance(&mut self) {
        let should_refresh = match self.last_balance_refresh {
            None => true,
            Some(last) => last.elapsed() > Duration::from_secs(self.cfg.balance_refresh_secs),
        };
        if !should_refresh {
            return;
        }
        if self.last_mid <= 0.0 {
            return;
        }

        if let Some(client) = &self.api_client {
            let client_arc = client.clone();
            let mid = self.last_mid;
            let risk_fraction = self.cfg.risk_fraction;
            let stop_pct = self.cfg.stop_loss_pct;

            // Synchronous block_on for balance fetch (cold path, every 60s)
            // Use block_in_place to safely do sync work inside async runtime
            if let Ok(handle) = Handle::try_current() {
                let result = tokio::task::block_in_place(|| {
                    handle.block_on(async { client_arc.get_balances().await })
                });
                if let Ok(balances) = result {
                    let usdc = balances.get("USDC").or_else(|| balances.get("usdc"));
                    if let Some(b) = usdc {
                        let available: f64 = b.available.parse().unwrap_or(0.0);
                        let locked: f64 = b.locked.parse().unwrap_or(0.0);
                        let equity = available + locked;

                        self.account_equity_usdc = equity;
                        let risk_usd = equity * risk_fraction;
                        self.max_position = risk_usd / mid;
                        self.base_size = (self.max_position / 3.0).max(0.01);
                        self.stop_loss_usd = equity * stop_pct * 10.0; // stop at ~3% of risk capital
                        self.last_balance_refresh = Some(Instant::now());

                        info!(
                            "💰 [BP] Balance: ${:.2} | MaxPos: {:.4} ETH | BaseSize: {:.4} | StopLoss: ${:.2}",
                            equity, self.max_position, self.base_size, self.stop_loss_usd
                        );
                    }
                }
            }
        }
    }
}

impl Strategy for BackpackMMStrategy {
    fn name(&self) -> &str {
        "BackpackMM-v3"
    }

    fn on_bbo_update(&mut self, symbol_id: u16, exchange_id: u8, bbo: &ShmBboMessage) {
        if exchange_id != self.exchange_id || symbol_id != self.symbol_id {
            return;
        }
        if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
            self.last_mid = (bbo.bid_price + bbo.ask_price) / 2.0;
            self.mid_history.push_back(self.last_mid);
            if self.mid_history.len() > self.cfg.vol_window {
                self.mid_history.pop_front();
            }
        }
    }

    fn on_idle(&mut self) {
        if self.last_mid == 0.0 {
            return;
        }

        // Periodically refresh balance
        self.maybe_refresh_balance();

        let now = Instant::now();
        let should_update = match self.last_update {
            None => true,
            Some(last) => {
                let elapsed = now.duration_since(last);
                if elapsed < Duration::from_millis(self.cfg.requote_interval_ms) {
                    false
                } else {
                    let time_trigger = elapsed > Duration::from_secs(5);
                    let price_trigger = if self.last_quoted_mid > 0.0 {
                        let dev = (self.last_mid - self.last_quoted_mid).abs()
                            / self.last_quoted_mid
                            * 10_000.0;
                        dev > 8.0
                    } else {
                        false
                    };
                    time_trigger || price_trigger
                }
            }
        };

        if should_update {
            self.last_update = Some(now);
            self.last_quoted_mid = self.last_mid;

            if let Some(client) = &self.api_client {
                let mid_price = self.last_mid;
                let client_arc = client.clone();
                let symbol_name = self.symbol_name().to_string();
                let cfg = self.cfg.clone();

                let vol_bps = self.realized_vol_bps();
                let momentum = self.momentum_bps();
                let max_position = self.max_position;
                let base_size = self.base_size;
                let stop_loss_usd = self.stop_loss_usd;

                if let Ok(handle) = Handle::try_current() {
                    handle.spawn(async move {
                        // 1. Fetch live positions (with entry price)
                        let mut live_pos: f64 = 0.0;
                        let mut entry_price: f64 = 0.0;
                        match client_arc.get_open_positions().await {
                            Ok(positions) => {
                                for pos in positions {
                                    if pos.symbol == symbol_name {
                                        live_pos = pos.quantity.parse().unwrap_or(0.0);
                                        entry_price = pos.average_entry_price
                                            .as_deref()
                                            .and_then(|s| s.parse().ok())
                                            .unwrap_or(0.0);
                                    }
                                }
                            }
                            Err(e) => warn!("⚠️ [BP-v3] Position fetch err: {:?}", e),
                        }

                        // === STOP-LOSS CHECK ===
                        if live_pos.abs() > 0.001 && entry_price > 0.0 {
                            let unrealized = (mid_price - entry_price) * live_pos;
                            if unrealized < -stop_loss_usd {
                                warn!("🛑 [BP-v3] STOP LOSS! Pos={:.4}@{:.2} Mid={:.2} UPnL=${:.2} (limit=${:.2})",
                                    live_pos, entry_price, mid_price, unrealized, stop_loss_usd);
                                let close_side = if live_pos > 0.0 { "Ask" } else { "Bid" };
                                let close_price = if live_pos > 0.0 { mid_price * 0.998 } else { mid_price * 1.002 };
                                let req = BackpackOrderRequest {
                                    symbol: symbol_name.clone(),
                                    side: close_side.to_string(),
                                    order_type: "Limit".to_string(),
                                    price: format!("{:.2}", close_price),
                                    quantity: format!("{:.2}", live_pos.abs()),
                                    client_id: None,
                                    post_only: Some(false),
                                    time_in_force: Some("IOC".to_string()),
                                };
                                match client_arc.create_order(&req).await {
                                    Ok(resp) => warn!("🛑 [BP-v3] Stop-loss filled: {}", resp.id),
                                    Err(e) => error!("🛑 [BP-v3] Stop-loss FAILED: {:?}", e),
                                }
                                return;
                            }
                        }

                        // 2. Cancel existing quotes
                        if let Err(e) = client_arc.cancel_all_orders(&symbol_name).await {
                            warn!("⚠️ [BP-v3] Cancel error: {:?}", e);
                        }

                        // === DYNAMIC SPREAD ===
                        let base_spread = f64::max(cfg.min_spread_bps, vol_bps * cfg.vol_multiplier);
                        let mut bid_spread = base_spread;
                        let mut ask_spread = base_spread;

                        if momentum > cfg.momentum_threshold_bps {
                            bid_spread *= cfg.momentum_spread_mult;
                        } else if momentum < -cfg.momentum_threshold_bps {
                            ask_spread *= cfg.momentum_spread_mult;
                        }

                        // Inventory skew
                        let skew_factor = live_pos / max_position;
                        let skew_shift = skew_factor * base_spread * 0.5;
                        let skewed_mid = mid_price * (1.0 - skew_shift / 10_000.0);

                        let bid_price = skewed_mid * (1.0 - bid_spread / 10_000.0);
                        let ask_price = skewed_mid * (1.0 + ask_spread / 10_000.0);

                        // === DYNAMIC SIZING ===
                        let pos_ratio = live_pos.abs() / max_position;
                        let scaled = base_size * (1.0 - pos_ratio * 0.8).max(0.01);
                        let mut bid_size = scaled;
                        let mut ask_size = scaled;
                        if live_pos >= max_position { bid_size = 0.0; }
                        if live_pos <= -max_position { ask_size = 0.0; }

                        info!("🎒v3 Vol={:.1} Mom={:.1} | Bid:{:.3}@{:.2}(sp={:.0}) Ask:{:.3}@{:.2}(sp={:.0}) Pos={:.3} MaxPos={:.3}",
                            vol_bps, momentum, bid_size, bid_price, bid_spread, ask_size, ask_price, ask_spread, live_pos, max_position);

                        let mut futures = Vec::new();
                        for &(is_buy, price, size) in &[(true, bid_price, bid_size), (false, ask_price, ask_size)] {
                            if size < 0.01 { continue; }
                            let client_arc = client_arc.clone();
                            let symbol_name = symbol_name.clone();
                            let req_future = async move {
                                let req = BackpackOrderRequest {
                                    symbol: symbol_name,
                                    side: if is_buy { "Bid".to_string() } else { "Ask".to_string() },
                                    order_type: "Limit".to_string(),
                                    price: format!("{:.2}", price),
                                    quantity: format!("{:.2}", size),
                                    client_id: None,
                                    post_only: Some(true),
                                    time_in_force: None,
                                };
                                match client_arc.create_order(&req).await {
                                    Ok(resp) => info!("✅ [BP-v3] {:?}: {}", if is_buy {"Bid"} else {"Ask"}, resp.id),
                                    Err(e) => error!("❌ [BP-v3] {:?}: {:?}", if is_buy {"Bid"} else {"Ask"}, e),
                                }
                            };
                            futures.push(req_future);
                        }
                        futures::future::join_all(futures).await;
                    });
                }
            }
        }
    }
}
