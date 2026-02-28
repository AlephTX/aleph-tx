use crate::backpack_api::client::BackpackClient;
use crate::backpack_api::model::*;
use crate::shm_reader::ShmBboMessage;
use crate::strategy::Strategy;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tracing::{error, info, warn};

pub struct BackpackMMStrategy {
    exchange_id: u8,
    symbol_id: u16,
    half_spread_bps: f64,
    api_client: Option<Arc<BackpackClient>>,
    last_update: Option<Instant>,
    last_mid: f64,
    last_quoted_mid: f64,
    net_position_base: f64,
}

impl BackpackMMStrategy {
    pub fn new(exchange_id: u8, symbol_id: u16, half_spread_bps: f64) -> Self {
        // Load keys from .env.backpack
        let env_str =
            std::fs::read_to_string("/home/metaverse/.openclaw/workspace/aleph-tx/.env.backpack")
                .unwrap_or_default();
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
                    info!("🎒 Loaded Backpack API Client natively in Rust strategy!");
                    Some(Arc::new(client))
                }
                Err(e) => {
                    warn!("Failed to init Backpack Client: {}", e);
                    None
                }
            }
        } else {
            warn!("No Backpack API keys found in .env.backpack. Running in dry mode.");
            None
        };

        Self {
            exchange_id,
            symbol_id,
            half_spread_bps,
            api_client,
            last_update: None,
            last_mid: 0.0,
            last_quoted_mid: 0.0,
            net_position_base: 0.0,
        }
    }

    fn symbol_name(&self) -> &str {
        if self.symbol_id == 1001 {
            "BTC_USDC_PERP"
        } else {
            "ETH_USDC_PERP"
        }
    }
}

impl Strategy for BackpackMMStrategy {
    fn name(&self) -> &str {
        "BackpackMM"
    }

    fn on_bbo_update(&mut self, symbol_id: u16, exchange_id: u8, bbo: &ShmBboMessage) {
        if exchange_id != self.exchange_id || symbol_id != self.symbol_id {
            return;
        }

        if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
            self.last_mid = (bbo.bid_price + bbo.ask_price) / 2.0;
        }
    }

    fn on_idle(&mut self) {
        if self.last_mid == 0.0 {
            return;
        }

        let now = Instant::now();
        let should_update = match self.last_update {
            None => true,
            Some(last) => {
                let time_elapsed_since_last = now.duration_since(last);

                if time_elapsed_since_last < Duration::from_secs(1) {
                    false
                } else {
                    let time_elapsed = time_elapsed_since_last > Duration::from_secs(3);
                    let price_deviated = if self.last_quoted_mid > 0.0 {
                        let deviation_bps = (self.last_mid - self.last_quoted_mid).abs()
                            / self.last_quoted_mid
                            * 10_000.0;
                        deviation_bps > 5.0
                    } else {
                        false
                    };
                    time_elapsed || price_deviated
                }
            }
        };

        if should_update {
            self.last_update = Some(now);
            self.last_quoted_mid = self.last_mid;

            if let Some(client) = &self.api_client {
                let mut mid_price = self.last_mid;
                let client_arc = client.clone();
                let bps = self.half_spread_bps;
                let symbol_name = self.symbol_name().to_string();
                let net_position_base = self.net_position_base;

                if let Ok(handle) = Handle::try_current() {
                    handle.spawn(async move {
                        // 1. Fetch live open positions to override Net Position locally
                        let mut live_pos = net_position_base;
                        let base_currency = symbol_name.split('_').next().unwrap_or("ETH");

                        match client_arc.get_open_positions().await {
                            Ok(positions) => {
                                for pos in positions {
                                    if pos.symbol == base_currency {
                                        live_pos = pos.quantity.parse().unwrap_or(0.0);
                                    }
                                }
                            }
                            Err(e) => warn!("⚠️ [Backpack MM] Failed to fetch positions: {:?}", e),
                        }

                        // 2. Cancel Existing Quotes
                        match client_arc.cancel_all_orders(&symbol_name).await {
                            Ok(_) => info!("♻️ [Backpack MM] Cancelled resting orders (Net Pos: {:.3} {}).", live_pos, base_currency),
                            Err(e) => warn!("⚠️ [Backpack MM] Cancel error: {:?}", e),
                        }

                        // 3. Inventory Skewing
                        let max_position = 0.5_f64;
                        let skew_factor = live_pos / max_position;

                        let max_shift_bps = bps * 0.25;
                        let shift_bps = skew_factor * max_shift_bps;

                        mid_price *= 1.0 - (shift_bps / 10000.0);

                        let bid_price = mid_price * (1.0 - (bps / 10000.0));
                        let ask_price = mid_price * (1.0 + (bps / 10000.0));

                        // 4. Base Sizing
                        let base_size = 0.10_f64;
                        let mut bid_size = base_size;
                        let mut ask_size = base_size;

                        if live_pos >= max_position {
                            bid_size = 0.0;
                        } else if live_pos <= -max_position {
                            ask_size = 0.0;
                        }

                        info!("🎒 Skewed Orders: NetPos={:.3} | Bid: {:.3}@{:.2} Ask: {:.3}@{:.2} (Shifted Mid: {:.2})",
                            live_pos, bid_size, bid_price, ask_size, ask_price, mid_price);

                        for &(is_buy, price, size) in &[(true, bid_price, bid_size), (false, ask_price, ask_size)] {
                            if size < 0.01 { continue; }

                            let req = BackpackOrderRequest {
                                symbol: symbol_name.clone(),
                                side: if is_buy { "Bid".to_string() } else { "Ask".to_string() },
                                order_type: "Limit".to_string(),
                                price: format!("{:.2}", price),
                                quantity: format!("{:.2}", size),
                                client_id: None,
                                post_only: Some(true),
                                time_in_force: None, // Backpack handles PostOnly via the explicit struct boolean
                            };

                            match client_arc.create_order(&req).await {
                                Ok(resp) => info!("✅ [Backpack MM] Order {:?} Submitted: ID {}", if is_buy { "Bid" } else { "Ask" }, resp.id),
                                Err(e) => error!("❌ [Backpack MM] Order {:?} Failed: {:?}", if is_buy { "Bid" } else { "Ask" }, e),
                            }
                        }
                    });
                }
            }
        }
    }
}
