use crate::shm_reader::ShmBboMessage;
use crate::strategy::Strategy;

use crate::edgex_api::client::EdgeXClient;
use crate::edgex_api::model::{CreateOrderRequest, OrderSide, OrderType, TimeInForce};
use std::sync::Arc;
use tokio::runtime::Handle;
use std::time::{Instant, Duration};

/// MarketMakerStrategy executes a statistical grid or market-making algorithm
/// exclusively on a single exchange to provide liquidity and capture spread.
pub struct MarketMakerStrategy {
    target_exchange_id: u8,
    symbol_id: u16,
    half_spread_bps: f64,
    edgex_client: Option<Arc<EdgeXClient>>,
    account_id: u64,
    last_update: Option<Instant>,
    last_mid: f64,
}

impl MarketMakerStrategy {
    pub fn new(target_exchange_id: u8, symbol_id: u16, half_spread_bps: f64) -> Self {
        // Attempt to load EdgeX keys from .env.edgex
        // In production, use standard configuration frameworks
        let mut edgex_client = None;
        let mut account_id = 0;

        if let Ok(env_str) = std::fs::read_to_string("/home/metaverse/.openclaw/workspace/aleph-tx/.env.edgex") {
            let mut key = String::new();
            for line in env_str.lines() {
                if let Some(rest) = line.strip_prefix("EDGEX_ACCOUNT_ID=") {
                    account_id = rest.trim().parse().unwrap_or(0);
                }
                if let Some(rest) = line.strip_prefix("EDGEX_STARK_PRIVATE_KEY=") {
                    key = rest.trim().to_string();
                }
            }
            if account_id > 0 && !key.is_empty() {
                if let Ok(client) = EdgeXClient::new(&key, None) {
                    edgex_client = Some(Arc::new(client));
                    tracing::info!("✅ Loaded EdgeX API Client natively in Rust strategy!");
                }
            }
        }

        Self {
            target_exchange_id,
            symbol_id,
            half_spread_bps,
            edgex_client,
            account_id,
            last_update: None,
            last_mid: 0.0,
        }
    }
}

impl Strategy for MarketMakerStrategy {
    fn name(&self) -> &str {
        "Single-Exchange Market Maker"
    }

    fn on_bbo_update(&mut self, symbol_id: u16, exchange_id: u8, bbo: &ShmBboMessage) {
        if symbol_id != self.symbol_id || exchange_id != self.target_exchange_id {
            return;
        }

        if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
            let mid_price = (bbo.bid_price + bbo.ask_price) / 2.0;
            self.last_mid = mid_price;
        }
    }

    fn on_idle(&mut self) {
        if self.last_mid == 0.0 {
            return;
        }
        
        let now = Instant::now();
        let should_update = match self.last_update {
            None => true,
            Some(last) => now.duration_since(last) > Duration::from_secs(15),
        };

        if should_update {
            self.last_update = Some(now);
            
            if let Some(client) = &self.edgex_client {
                let mid_price = self.last_mid;
                let client_arc: Arc<EdgeXClient> = client.clone();
                let account_id = self.account_id;
                let bps = self.half_spread_bps;

                if let Ok(handle) = Handle::try_current() {
                    handle.spawn(async move {
                        // 1. Cancel all existing quotes for ETHUSD (Contract 10000002)
                        use crate::edgex_api::model::CancelAllOrderRequest;
                        let cancel_req = CancelAllOrderRequest {
                            account_id,
                            filter_contract_id_list: vec![10000002],
                        };
                        match client_arc.cancel_all_orders(&cancel_req).await {
                            Ok(_) => tracing::info!("♻️ [EdgeX MM] Cancelled all resting orders."),
                            Err(e) => tracing::warn!("⚠️ [EdgeX MM] Cancel error: {:?}", e),
                        }

                        // 2. Compute new Bid and Ask
                        let bid_price = mid_price * (1.0 - (bps / 10000.0));
                        let ask_price = mid_price * (1.0 + (bps / 10000.0));
                        let size_eth = 0.10_f64; // EdgeX minimum order size is 0.1 ETH
                        
                        let synthetic_id = "0x4554482d3900000000000000000000"; // ETHUSD
                        let collateral_id = "0x2ce625e94458d39dd0bf3b45a843544dd4a14b8169045a3a3d15aa564b936c5"; // USD
                        
                        let amount_synthetic = (size_eth * 1_000_000_000.0) as u64; 
                        
                        let fee_rate = 0.00038_f64; // taker rate for safety
                        let expire_time_ms = chrono::Utc::now().timestamp_millis() as u64 + (30 * 24 * 60 * 60 * 1000);
                        let expire_time_hours = expire_time_ms / (60 * 60 * 1000);

                        tracing::info!("🚀 Submitting MM Orders for {:.3} ETH @ Bid: {:.2} Ask: {:.2} (Mid: {:.2})", size_eth, bid_price, ask_price, mid_price);

                        for &(is_buy, price) in &[(true, bid_price), (false, ask_price)] {
                            // Round the price explicitly to 2 decimal places to match EdgeX's JSON string precision
                            let price = (price * 100.0).round() / 100.0;
                            let value_usd = price * size_eth;
                            
                            // Amount collateral must cleanly match mathematical precise calculations
                            let amount_collateral = (value_usd * 1_000_000.0).round() as u64;
                            
                            // Fee must be ceilinged BEFORE multiplying by 10^6 !!
                            let fee_usd_ceil = (value_usd * fee_rate).ceil(); // 1.0 (if USD)
                            let amount_fee_str = format!("{:.0}", fee_usd_ceil);
                            let amount_fee = (fee_usd_ceil * 1_000_000.0) as u64; // e.g. 1,000,000
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

                            if let Ok(hash) = hash_result {
                                if let Ok(l2_sig) = client_arc.signature_manager.sign_l2_action(hash) {
                                    let side = if is_buy { OrderSide::Buy } else { OrderSide::Sell };
                                    let req = CreateOrderRequest {
                                        price: format!("{:.2}", price),
                                        size: format!("{:.3}", size_eth),
                                        r#type: OrderType::Limit,
                                        time_in_force: TimeInForce::GoodTilCancel, // resting order
                                        account_id,
                                        contract_id: 10000002,
                                        side,
                                        client_order_id,
                                        expire_time: expire_time_ms - 864_000_000,
                                        l2_nonce,
                                        l2_value: format!("{:.4}", value_usd), // Unscaled size
                                        l2_size: format!("{:.3}", size_eth),   // Unscaled size
                                        l2_limit_fee: amount_fee_str,          // Whole unit fee string !
                                        l2_expire_time: expire_time_ms,
                                        l2_signature: l2_sig,
                                    };

                                    match client_arc.create_order(&req).await {
                                        Ok(resp) => tracing::info!("✅ [EdgeX MM] Order {:?} Submitted: {}", if is_buy { "Bid" } else { "Ask" }, resp),
                                        Err(e) => tracing::error!("❌ [EdgeX MM] Order {:?} Failed: {:?}", if is_buy { "Bid" } else { "Ask" }, e),
                                    }
                                }
                            }
                        }
                    });
                }
            }
        }
    }
}
