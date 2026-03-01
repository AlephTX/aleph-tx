use crate::config::ExchangeConfig;
use crate::shm_reader::ShmBboMessage;
use crate::strategy::Strategy;

use crate::edgex_api::client::EdgeXClient;
use crate::edgex_api::model::{CreateOrderRequest, OrderSide, OrderType, TimeInForce};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

pub struct MarketMakerStrategy {
    target_exchange_id: u8,
    symbol_id: u16,
    cfg: ExchangeConfig,
    edgex_client: Option<Arc<EdgeXClient>>,
    account_id: u64,

    // Price tracking
    last_mid: f64,
    last_quoted_mid: f64,
    last_update: Option<Instant>,

    // Volatility
    mid_history: VecDeque<f64>,

    // Dynamic limits
    max_position: f64,
    base_size: f64,
    stop_loss_usd: f64,
    last_balance_refresh: Option<Instant>,
    account_equity_usd: f64,
}

impl MarketMakerStrategy {
    pub fn new(
        target_exchange_id: u8,
        symbol_id: u16,
        _half_spread_bps: f64,
        cfg: ExchangeConfig,
    ) -> Self {
        let mut edgex_client = None;
        let mut account_id = 0;

        let env_path = std::env::var("EDGEX_ENV_PATH").unwrap_or_else(|_| {
            "/home/metaverse/.openclaw/workspace/aleph-tx/.env.edgex".to_string()
        });

        if let Ok(env_str) = std::fs::read_to_string(&env_path) {
            let mut key = String::new();
            for line in env_str.lines() {
                if let Some(rest) = line.strip_prefix("EDGEX_ACCOUNT_ID=") {
                    account_id = rest.trim().parse().unwrap_or(0);
                }
                if let Some(rest) = line.strip_prefix("EDGEX_STARK_PRIVATE_KEY=") {
                    key = rest.trim().to_string();
                }
            }
            if account_id > 0
                && !key.is_empty()
                && let Ok(client) = EdgeXClient::new(&key, None)
            {
                edgex_client = Some(Arc::new(client));
                tracing::info!("✅ Loaded EdgeX API Client (v3 — dynamic allocation)");
            }
        }

        let vol_window = cfg.vol_window;
        let min_order = cfg.min_order_size;
        Self {
            target_exchange_id,
            symbol_id,
            cfg,
            edgex_client,
            account_id,
            last_update: None,
            last_mid: 0.0,
            last_quoted_mid: 0.0,
            mid_history: VecDeque::with_capacity(vol_window + 1),
            max_position: 0.2,
            base_size: min_order.max(0.1),
            stop_loss_usd: 5.0,
            last_balance_refresh: None,
            account_equity_usd: 0.0,
        }
    }

    fn realized_vol_bps(&self) -> f64 {
        if self.mid_history.len() < 10 {
            return 25.0;
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

    /// Refresh EdgeX balance and recompute limits
    fn maybe_refresh_balance(&mut self) {
        let should_refresh = match self.last_balance_refresh {
            None => true,
            Some(last) => last.elapsed() > Duration::from_secs(self.cfg.balance_refresh_secs),
        };
        if !should_refresh || self.last_mid <= 0.0 {
            return;
        }

        if let Some(client) = &self.edgex_client {
            let client_arc = client.clone();
            let account_id = self.account_id;
            let mid = self.last_mid;
            let risk_fraction = self.cfg.risk_fraction;
            let stop_pct = self.cfg.stop_loss_pct;
            let min_order_size = self.cfg.min_order_size;

            if let Ok(handle) = Handle::try_current() {
                let result = tokio::task::block_in_place(|| {
                    handle.block_on(async { client_arc.get_balances(account_id).await })
                });
                if let Ok(balances) = result {
                    // Sum total from EdgeX balance entries
                    let mut equity = 0.0;
                    for b in &balances {
                        let bal: f64 = b.balance.parse().unwrap_or(0.0);
                        if bal > equity {
                            equity = bal;
                        }
                    }

                    if equity > 0.0 {
                        self.account_equity_usd = equity;
                        let risk_usd = equity * risk_fraction;
                        self.max_position = risk_usd / mid;
                        self.base_size = (self.max_position / 2.0).max(min_order_size);
                        // Round to 0.01 for EdgeX stepSize
                        self.base_size = (self.base_size * 100.0).floor() / 100.0;
                        if self.base_size < min_order_size {
                            self.base_size = min_order_size;
                        }
                        self.stop_loss_usd = equity * stop_pct * 10.0;
                        self.last_balance_refresh = Some(Instant::now());

                        tracing::info!(
                            "💰 [EX] Balance: ${:.2} | MaxPos: {:.4} ETH | BaseSize: {:.2} | StopLoss: ${:.2}",
                            equity,
                            self.max_position,
                            self.base_size,
                            self.stop_loss_usd
                        );
                    }
                }
            }
        }
    }
}

impl Strategy for MarketMakerStrategy {
    fn name(&self) -> &str {
        "EdgeX-MM-v3"
    }

    fn on_bbo_update(&mut self, symbol_id: u16, exchange_id: u8, bbo: &ShmBboMessage) {
        if symbol_id != self.symbol_id || exchange_id != self.target_exchange_id {
            return;
        }
        if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
            let mid = (bbo.bid_price + bbo.ask_price) / 2.0;
            self.last_mid = mid;
            self.mid_history.push_back(mid);
            if self.mid_history.len() > self.cfg.vol_window {
                self.mid_history.pop_front();
            }
        }
    }

    fn on_idle(&mut self) {
        if self.last_mid == 0.0 {
            return;
        }

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
                        dev > 10.0
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

            if let Some(client) = &self.edgex_client {
                let mid_price = self.last_mid;
                let client_arc: Arc<EdgeXClient> = client.clone();
                let account_id = self.account_id;
                let cfg = self.cfg.clone();

                let vol_bps = self.realized_vol_bps();
                let momentum = self.momentum_bps();
                let max_position = self.max_position;
                let base_size = self.base_size;
                let stop_loss_usd = self.stop_loss_usd;

                if let Ok(handle) = Handle::try_current() {
                    handle.spawn(async move {
                        // 1. Fetch live positions
                        let mut live_pos = 0.0;
                        match client_arc.get_positions(account_id).await {
                            Ok(positions) => {
                                for p in positions {
                                    if p.contract_id == "10000002" {
                                        live_pos += p.open_size.parse::<f64>().unwrap_or(0.0);
                                    }
                                }
                            }
                            Err(e) => tracing::warn!("⚠️ [EX-v3] Position err: {:?}", e),
                        }

                        // === STOP-LOSS (over-exposure guard) ===
                        // Trigger only if position is WAY beyond max_position (3x)
                        // EdgeX doesn't return entry price, so we guard on exposure, not PnL
                        if live_pos.abs() > max_position * 3.0 && max_position > 0.0 {
                            tracing::warn!("🛑 [EX-v3] OVER-EXPOSED! Pos={:.4} MaxPos={:.4} — cancelling all orders",
                                live_pos, max_position);
                            use crate::edgex_api::model::CancelAllOrderRequest;
                            let cancel_req = CancelAllOrderRequest {
                                account_id, filter_contract_id_list: vec![10000002],
                            };
                            let _ = client_arc.cancel_all_orders(&cancel_req).await;
                            return;
                        }

                        // 2. Cancel existing quotes
                        use crate::edgex_api::model::CancelAllOrderRequest;
                        let cancel_req = CancelAllOrderRequest {
                            account_id, filter_contract_id_list: vec![10000002],
                        };
                        if let Err(e) = client_arc.cancel_all_orders(&cancel_req).await {
                            tracing::warn!("⚠️ [EX-v3] Cancel err: {:?}", e);
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

                        let skew_factor = live_pos / max_position;
                        let skew_shift = skew_factor * base_spread * 0.5;
                        let skewed_mid = mid_price * (1.0 - skew_shift / 10_000.0);
                        let bid_price = skewed_mid * (1.0 - bid_spread / 10_000.0);
                        let ask_price = skewed_mid * (1.0 + ask_spread / 10_000.0);

                        // === SIZING ===
                        let mut bid_size = base_size;
                        let mut ask_size = base_size;
                        if live_pos >= max_position { bid_size = 0.0; }
                        if live_pos <= -max_position { ask_size = 0.0; }

                        tracing::info!("🔌v3 Vol={:.1} Mom={:.1} | Bid:{:.2}@{:.2}(sp={:.0}) Ask:{:.2}@{:.2}(sp={:.0}) Pos={:.3} MaxPos={:.3}",
                            vol_bps, momentum, bid_size, bid_price, bid_spread, ask_size, ask_price, ask_spread, live_pos, max_position);

                        // Submit orders
                        let synthetic_id = "0x4554482d3900000000000000000000";
                        let collateral_id = "0x2ce625e94458d39dd0bf3b45a843544dd4a14b8169045a3a3d15aa564b936c5";
                        let fee_rate = 0.00034_f64;
                        let expire_time_ms = chrono::Utc::now().timestamp_millis() as u64 + (30 * 24 * 60 * 60 * 1000);
                        let expire_time_hours = expire_time_ms / (60 * 60 * 1000);

                        let mut futures = Vec::new();
                        for &(is_buy, price, size_eth) in &[(true, bid_price, bid_size), (false, ask_price, ask_size)] {
                            if size_eth < cfg.min_order_size.max(0.01) { continue; }
                            let client_arc = client_arc.clone();

                            let req_future = async move {
                                let price = (price * 100.0).round() / 100.0;
                                let value_usd = price * size_eth;
                                let amount_synthetic = (size_eth * 1_000_000_000.0) as u64;
                                let amount_collateral = (value_usd * 1_000_000.0).round() as u64;
                                let exact_fee = value_usd * fee_rate;
                                let amount_fee_quantum = (exact_fee * 1_000_000.0).ceil();
                                let amount_fee_str = format!("{:.6}", amount_fee_quantum / 1_000_000.0);
                                let amount_fee = amount_fee_quantum as u64;
                                let initial_nonce = rand::random::<u32>() as u64;
                                let client_order_id = format!("MM-{}", initial_nonce);

                                use sha2::{Sha256, Digest};
                                let mut hasher = Sha256::new();
                                hasher.update(client_order_id.as_bytes());
                                let l2_nonce_hex = hex::encode(hasher.finalize());
                                let l2_nonce = u64::from_str_radix(&l2_nonce_hex[..8], 16).unwrap();

                                let hash_result = client_arc.signature_manager.calc_limit_order_hash(
                                    synthetic_id, collateral_id, collateral_id,
                                    is_buy, amount_synthetic, amount_collateral, amount_fee,
                                    l2_nonce, account_id, expire_time_hours
                                );
                                if let Ok(hash) = hash_result
                                    && let Ok(l2_sig) = client_arc.signature_manager.sign_l2_action(hash) {
                                    let req = CreateOrderRequest {
                                        price: format!("{:.2}", price),
                                        size: format!("{:.3}", size_eth),
                                        r#type: OrderType::Limit,
                                        time_in_force: TimeInForce::PostOnly,
                                        account_id, contract_id: 10000002,
                                        side: if is_buy { OrderSide::Buy } else { OrderSide::Sell },
                                        client_order_id, expire_time: expire_time_ms - 864_000_000,
                                        l2_nonce, l2_value: format!("{:.4}", value_usd),
                                        l2_size: format!("{:.3}", size_eth),
                                        l2_limit_fee: amount_fee_str,
                                        l2_expire_time: expire_time_ms,
                                        l2_signature: l2_sig,
                                    };
                                    match client_arc.create_order(&req).await {
                                        Ok(resp) => tracing::info!("✅ [EX-v3] {:?}: {}", if is_buy {"Bid"} else {"Ask"}, resp),
                                        Err(e) => tracing::error!("❌ [EX-v3] {:?}: {:?}", if is_buy {"Bid"} else {"Ask"}, e),
                                    }
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
