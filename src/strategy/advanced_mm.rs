/// Advanced Market Making Strategy with institutional-grade optimizations
///
/// Features:
/// - EWMA volatility estimation
/// - Avellaneda-Stoikov optimal quoting
/// - Adverse selection detection
/// - Orderbook imbalance signals
/// - Incremental order updates
/// - Real-time PnL tracking
use crate::backpack_api::client::BackpackClient;
use crate::backpack_api::model::*;
use crate::config::{ExchangeConfig, format_price, format_size};
use crate::shm_reader::ShmBboMessage;
use crate::strategy::Strategy;
use std::collections::{VecDeque, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tracing::{error, info, warn};

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
enum Side {
    Buy,
    Sell,
}

/// Real-time PnL tracker (reserved for Phase 2 implementation)
#[allow(dead_code)]
struct PnLTracker {
    realized_pnl: f64,
    unrealized_pnl: f64,
    position: f64,
    avg_entry_price: f64,
    fees_paid: f64,
    last_update: Instant,
}

impl PnLTracker {
    fn new() -> Self {
        Self {
            realized_pnl: 0.0,
            unrealized_pnl: 0.0,
            position: 0.0,
            avg_entry_price: 0.0,
            fees_paid: 0.0,
            last_update: Instant::now(),
        }
    }

    #[allow(dead_code)]
    fn update_on_fill(&mut self, side: Side, price: f64, size: f64, fee: f64) {
        match side {
            Side::Buy => {
                if self.position < 0.0 {
                    // Closing short position
                    let close_size = size.min(self.position.abs());
                    let pnl = (self.avg_entry_price - price) * close_size;
                    self.realized_pnl += pnl;
                    self.position += close_size;

                    // Opening new long if size > close_size
                    if size > close_size {
                        let open_size = size - close_size;
                        self.avg_entry_price = price;
                        self.position += open_size;
                    }
                } else {
                    // Adding to long or opening new long
                    let new_pos = self.position + size;
                    self.avg_entry_price = (self.avg_entry_price * self.position + price * size) / new_pos;
                    self.position = new_pos;
                }
            }
            Side::Sell => {
                if self.position > 0.0 {
                    // Closing long position
                    let close_size = size.min(self.position);
                    let pnl = (price - self.avg_entry_price) * close_size;
                    self.realized_pnl += pnl;
                    self.position -= close_size;

                    // Opening new short if size > close_size
                    if size > close_size {
                        let open_size = size - close_size;
                        self.avg_entry_price = price;
                        self.position -= open_size;
                    }
                } else {
                    // Adding to short or opening new short
                    let new_pos = self.position - size;
                    self.avg_entry_price = (self.avg_entry_price * self.position.abs() + price * size) / new_pos.abs();
                    self.position = new_pos;
                }
            }
        }
        self.fees_paid += fee;
        self.last_update = Instant::now();
    }

    #[allow(dead_code)]
    fn calc_unrealized_pnl(&mut self, current_price: f64) -> f64 {
        if self.position == 0.0 {
            self.unrealized_pnl = 0.0;
        } else {
            self.unrealized_pnl = (current_price - self.avg_entry_price) * self.position;
        }
        self.unrealized_pnl
    }

    #[allow(dead_code)]
    fn total_pnl(&self) -> f64 {
        self.realized_pnl + self.unrealized_pnl - self.fees_paid
    }
}

pub struct AdvancedMMStrategy {
    exchange_id: u8,
    symbol_id: u16,
    cfg: ExchangeConfig,
    api_client: Option<Arc<BackpackClient>>,

    // Price tracking
    last_mid: f64,
    last_quoted_mid: f64,
    last_update: Option<Instant>,
    last_bbo: Option<ShmBboMessage>,

    // Volatility estimation
    mid_history: VecDeque<f64>,
    ewma_variance: f64,
    ewma_lambda: f64,

    // PnL tracking (reserved for Phase 2 implementation)
    #[allow(dead_code)]
    pnl_tracker: PnLTracker,

    // Adverse selection detection
    last_fill_time: Option<Instant>,
    last_fill_price: f64,
    adverse_selection_count: u32,

    // Dynamic limits
    max_position: f64,
    base_size: f64,
    stop_loss_usd: f64,
    last_balance_refresh: Option<Instant>,
    account_equity_usdc: f64,

    // Order state cache (reserved for Phase 2 incremental quoting)
    #[allow(dead_code)]
    active_orders: HashMap<String, (f64, f64, String)>, // order_id -> (price, size, side)

    // Phase 2: Incremental quoting state
    last_quoted_bid: f64,
    last_quoted_ask: f64,
    last_quoted_pos: f64,
}

impl AdvancedMMStrategy {
    pub fn new(
        exchange_id: u8,
        symbol_id: u16,
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
                    info!("🎒 Loaded Advanced MM Strategy for Backpack");
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
            last_bbo: None,
            mid_history: VecDeque::with_capacity(vol_window + 1),
            ewma_variance: 0.0,
            ewma_lambda: 0.94, // Standard EWMA decay
            pnl_tracker: PnLTracker::new(),
            last_fill_time: None,
            last_fill_price: 0.0,
            adverse_selection_count: 0,
            max_position: 0.3,
            base_size: 0.05,
            stop_loss_usd: 5.0,
            last_balance_refresh: None,
            account_equity_usdc: 0.0,
            active_orders: HashMap::new(),
            last_quoted_bid: 0.0,
            last_quoted_ask: 0.0,
            last_quoted_pos: 0.0,
        }
    }

    fn symbol_name(&self) -> &str {
        if self.symbol_id == 1001 {
            "BTC_USDC_PERP"
        } else {
            "ETH_USDC_PERP"
        }
    }

    /// EWMA volatility (more responsive than simple std dev)
    fn ewma_volatility_bps(&mut self) -> f64 {
        if self.mid_history.len() < 2 {
            return 20.0;
        }

        let last_idx = self.mid_history.len() - 1;
        let ret = (self.mid_history[last_idx] - self.mid_history[last_idx - 1])
            / self.mid_history[last_idx - 1];

        self.ewma_variance = self.ewma_lambda * self.ewma_variance
            + (1.0 - self.ewma_lambda) * ret.powi(2);

        (self.ewma_variance.sqrt() * 10_000.0).max(10.0)
    }

    /// Momentum signal
    fn momentum_bps(&self) -> f64 {
        if self.mid_history.len() < 5 {
            return 0.0;
        }
        let recent = self.mid_history.back().unwrap();
        let lookback = self.mid_history.iter().rev().nth(4).unwrap();
        (recent - lookback) / lookback * 10_000.0
    }

    /// Orderbook imbalance signal
    fn orderbook_imbalance(&self) -> f64 {
        if let Some(bbo) = &self.last_bbo {
            let bid_depth = bbo.bid_size;
            let ask_depth = bbo.ask_size;
            if bid_depth + ask_depth > 0.0 {
                return (bid_depth - ask_depth) / (bid_depth + ask_depth);
            }
        }
        0.0
    }

    /// Avellaneda-Stoikov optimal reservation price and spread (reserved for future use)
    #[allow(dead_code)]
    fn optimal_quotes(&self, mid: f64, inventory: f64, vol_bps: f64) -> (f64, f64, f64) {
        let gamma = self.cfg.gamma;
        let sigma = vol_bps / 10_000.0; // Convert bps to decimal
        let time_horizon = self.cfg.time_horizon_sec;

        // Reservation price (adjust mid based on inventory)
        let reservation_price = mid - inventory * gamma * sigma.powi(2) * time_horizon;

        // Optimal spread
        let spread_decimal = gamma * sigma.powi(2) * time_horizon
            + (2.0 / gamma) * (1.0 + gamma / 10.0).ln();
        let spread_bps = spread_decimal * 10_000.0;

        let half_spread = spread_bps / 2.0;
        (reservation_price, half_spread, half_spread)
    }

    /// Detect adverse selection
    fn check_adverse_selection(&mut self) -> bool {
        if let Some(last_fill) = self.last_fill_time {
            let elapsed_ms = last_fill.elapsed().as_millis();
            if elapsed_ms < 1000 {
                let price_move_bps = ((self.last_mid - self.last_fill_price).abs()
                    / self.last_fill_price) * 10_000.0;

                if price_move_bps > 8.0 {
                    self.adverse_selection_count += 1;
                    if self.adverse_selection_count > 3 {
                        warn!("⚠️ [AdvMM] Adverse selection detected! Widening spreads...");
                        return true;
                    }
                }
            }
        }
        false
    }

    /// Refresh account balance
    fn maybe_refresh_balance(&mut self) {
        let should_refresh = match self.last_balance_refresh {
            None => true,
            Some(last) => last.elapsed() > Duration::from_secs(self.cfg.balance_refresh_secs),
        };
        if !should_refresh || self.last_mid <= 0.0 {
            return;
        }

        if let Some(client) = &self.api_client {
            let client_arc = client.clone();
            let mid = self.last_mid;
            let risk_fraction = self.cfg.risk_fraction;
            let stop_pct = self.cfg.stop_loss_pct;

            if let Ok(handle) = Handle::try_current() {
                let result = tokio::task::block_in_place(|| {
                    handle.block_on(async { client_arc.get_total_equity().await })
                });
                if let Ok(equity) = result {
                    if equity > 0.0 {
                        self.account_equity_usdc = equity;
                        let risk_usd = equity * risk_fraction;
                        self.max_position = risk_usd / mid;
                        self.base_size = (self.max_position / 3.0).max(0.01);
                        self.stop_loss_usd = equity * stop_pct * 10.0;
                        self.last_balance_refresh = Some(Instant::now());

                        info!(
                            "💰 [AdvMM] Equity: ${:.2} | MaxPos: {:.4} | BaseSize: {:.4} | StopLoss: ${:.2}",
                            equity, self.max_position, self.base_size, self.stop_loss_usd
                        );
                    } else {
                        self.last_balance_refresh = Some(Instant::now());
                    }
                }
            }
        }
    }
}

impl Strategy for AdvancedMMStrategy {
    fn name(&self) -> &str {
        "AdvancedMM-v4"
    }

    fn on_bbo_update(&mut self, symbol_id: u16, exchange_id: u8, bbo: &ShmBboMessage) {
        if exchange_id != self.exchange_id || symbol_id != self.symbol_id {
            return;
        }
        if bbo.bid_price > 0.0 && bbo.ask_price > 0.0 {
            self.last_mid = (bbo.bid_price + bbo.ask_price) / 2.0;
            self.last_bbo = Some(*bbo);
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
                            / self.last_quoted_mid * 10_000.0;
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

                let vol_bps = self.ewma_volatility_bps();
                let momentum = self.momentum_bps();
                let imbalance = self.orderbook_imbalance();
                let adverse_sel = self.check_adverse_selection();

                let max_position = self.max_position;
                let base_size = self.base_size;
                let stop_loss_usd = self.stop_loss_usd;

                // Phase 2: Capture last quoted state for incremental quoting
                let last_quoted_bid = self.last_quoted_bid;
                let last_quoted_ask = self.last_quoted_ask;
                let last_quoted_pos = self.last_quoted_pos;

                if let Ok(handle) = Handle::try_current() {
                    handle.spawn(async move {
                        // Fetch positions
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
                            Err(e) => warn!("⚠️ [AdvMM] Position fetch err: {:?}", e),
                        }

                        // Stop-loss check
                        if live_pos.abs() > 0.001 && entry_price > 0.0 {
                            let unrealized = (mid_price - entry_price) * live_pos;
                            if unrealized < -stop_loss_usd {
                                warn!("🛑 [AdvMM] STOP LOSS! Pos={:.4}@{:.2} UPnL=${:.2}",
                                    live_pos, entry_price, unrealized);
                                let close_side = if live_pos > 0.0 { "Ask" } else { "Bid" };
                                let close_price = if live_pos > 0.0 {
                                    mid_price * 0.998
                                } else {
                                    mid_price * 1.002
                                };
                                let req = BackpackOrderRequest {
                                    symbol: symbol_name.clone(),
                                    side: close_side.to_string(),
                                    order_type: "Limit".to_string(),
                                    price: format_price(close_price, cfg.tick_size),
                                    quantity: format_size(live_pos.abs(), cfg.step_size),
                                    client_id: None,
                                    post_only: Some(false),
                                    time_in_force: Some("IOC".to_string()),
                                };
                                match client_arc.create_order(&req).await {
                                    Ok(resp) => warn!("🛑 [AdvMM] Stop-loss filled: {}", resp.id),
                                    Err(e) => error!("🛑 [AdvMM] Stop-loss FAILED: {:?}", e),
                                }
                                return;
                            }
                        }

                        // Cancel existing orders
                        if let Err(e) = client_arc.cancel_all_orders(&symbol_name).await {
                            warn!("⚠️ [AdvMM] Cancel error: {:?}", e);
                        } else {
                            // Clear order cache on successful cancel
                            // Note: active_orders is not accessible here in spawned task
                            // This will be handled in Phase 2 with state refactoring
                        }

                        // === ADVANCED QUOTING LOGIC ===

                        // 1. Avellaneda-Stoikov optimal quotes
                        let inventory_ratio = live_pos / max_position;
                        let (reservation_price, mut bid_spread, mut ask_spread) = {
                            let gamma = cfg.gamma;
                            let sigma = vol_bps / 10_000.0;
                            let time_horizon = cfg.time_horizon_sec;
                            let res_price = mid_price - inventory_ratio * gamma * sigma.powi(2) * time_horizon * mid_price;
                            let spread_dec = gamma * sigma.powi(2) * time_horizon + (2.0 / gamma) * (1.0 + gamma / 10.0).ln();
                            let spread = (spread_dec * 10_000.0).max(cfg.min_spread_bps);
                            (res_price, spread, spread)
                        };

                        // 2. Adjust for momentum
                        if momentum > cfg.momentum_threshold_bps {
                            bid_spread *= cfg.momentum_spread_mult;
                        } else if momentum < -cfg.momentum_threshold_bps {
                            ask_spread *= cfg.momentum_spread_mult;
                        }

                        // 3. Adjust for orderbook imbalance
                        let imbalance_adj = imbalance * 3.0; // ±3 bps
                        bid_spread -= imbalance_adj;
                        ask_spread += imbalance_adj;

                        // 4. Widen if adverse selection detected
                        if adverse_sel {
                            bid_spread *= 1.5;
                            ask_spread *= 1.5;
                        }

                        // 5. Calculate final prices
                        let bid_price = reservation_price * (1.0 - bid_spread / 10_000.0);
                        let ask_price = reservation_price * (1.0 + ask_spread / 10_000.0);

                        // === PHASE 2: INCREMENTAL QUOTING ===
                        // Only requote if price deviation exceeds threshold OR position changed significantly
                        let bid_deviation_bps = if last_quoted_bid > 0.0 {
                            ((bid_price - last_quoted_bid).abs() / last_quoted_bid) * 10_000.0
                        } else {
                            f64::MAX
                        };
                        let ask_deviation_bps = if last_quoted_ask > 0.0 {
                            ((ask_price - last_quoted_ask).abs() / last_quoted_ask) * 10_000.0
                        } else {
                            f64::MAX
                        };
                        let pos_changed = (live_pos - last_quoted_pos).abs() > 0.01;

                        let should_requote = bid_deviation_bps > cfg.requote_threshold_bps
                            || ask_deviation_bps > cfg.requote_threshold_bps
                            || pos_changed
                            || last_quoted_bid == 0.0;

                        if !should_requote {
                            info!("⏭️ [AdvMM] Skip requote: bid_dev={:.2}bps ask_dev={:.2}bps pos_delta={:.3}",
                                bid_deviation_bps, ask_deviation_bps, (live_pos - last_quoted_pos).abs());
                            return;
                        }

                        // 6. Dynamic sizing
                        let pos_ratio = live_pos.abs() / max_position;
                        let scaled = base_size * (1.0 - pos_ratio * 0.8).max(0.01);
                        let mut bid_size = scaled;
                        let mut ask_size = scaled;
                        if live_pos >= max_position { bid_size = 0.0; }
                        if live_pos <= -max_position { ask_size = 0.0; }

                        info!("🚀v4 Vol={:.1} Mom={:.1} Imb={:.2} | Bid:{:.3}@{:.2}(sp={:.0}) Ask:{:.3}@{:.2}(sp={:.0}) Pos={:.3}",
                            vol_bps, momentum, imbalance, bid_size, bid_price, bid_spread, ask_size, ask_price, ask_spread, live_pos);

                        // Submit orders in parallel
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
                                    price: format_price(price, cfg.tick_size),
                                    quantity: format_size(size, cfg.step_size),
                                    client_id: None,
                                    post_only: Some(true),
                                    time_in_force: None,
                                };
                                match client_arc.create_order(&req).await {
                                    Ok(resp) => info!("✅ [AdvMM] {:?}: {}", if is_buy {"Bid"} else {"Ask"}, resp.id),
                                    Err(e) => error!("❌ [AdvMM] {:?}: {:?}", if is_buy {"Bid"} else {"Ask"}, e),
                                }
                            };
                            futures.push(req_future);
                        }
                        futures::future::join_all(futures).await;

                        // Note: State update (last_quoted_bid/ask/pos) will be handled in Phase 3
                        // with proper Arc<RwLock> shared state architecture
                    });
                }
            }
        }
    }
}
