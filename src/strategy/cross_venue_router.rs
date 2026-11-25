//! Causal cross-venue execution router (`ARB-IMP-07` & `CRASH-IMP-03`).
//!
//! Enforces venue speed-bump topology selection, WAL exposure reservations,
//! cross-stablecoin / tail-risk supervisory gates, portfolio leg caps, per-venue
//! sliding-window order budgets (exempting `EmergencyHedge`), and marginal L2
//! depth sizing without performing transport I/O.

use crate::{
    hedging::floor_step,
    order_budget::{BudgetDecision, OrderIntentClass, VenueOrderBudget},
    strategy::{
        portfolio_risk::CarryBudgetAllocation,
        relative_value::{
            Book, Level, MarginalCrossParams, MarginalCrossQuote, crossable_marginal_depth,
        },
        tail_risk::{
            StablecoinPegSample, TailRiskAction, TailRiskConfig, VenueMarginSnapshot,
            evaluate_tail_risk,
        },
    },
    venues::{
        CrossStablecoinHedgePolicy, ExecutionTopology, VenueExecutionCapability,
        select_execution_topology, select_execution_topology_with_policy,
    },
};
use anyhow::{Context, Result, bail, ensure};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

const BPS: Decimal = Decimal::from_parts(10_000, 0, 0, false, 0);

/// Maximum per-side depth levels evaluated by the router.
pub const MAX_ROUTER_BOOK_LEVELS: usize = 20;

/// Default maximum age in milliseconds admitted for entry books.
pub const DEFAULT_MAX_ENTRY_BOOK_AGE_MS: u64 = 1_000;
/// Default maximum age in milliseconds admitted for dynamic follow-up hedge books.
pub const DEFAULT_MAX_FOLLOWUP_BOOK_AGE_MS: u64 = 1_000;

/// Configuration for [`CrossVenueExecutionRouter`].
///
/// [`Default`] leaves `max_entry_notional_usd` at `0` so unconfigured routers
/// fail closed in [`CrossVenueRouterConfig::validate`] until explicitly sized.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrossVenueRouterConfig {
    /// Require dual-taker execution (`SlowLegFirst` under asymmetric speed bumps,
    /// `ConcurrentProtectedIoc` only when both legs are fast). Defaults to `false`
    /// so asymmetric pairs route `MakerFirst`.
    pub dual_taker_required: bool,
    /// Minimum marginal net edge in basis points (`>= 0`).
    #[serde(with = "rust_decimal::serde::str")]
    pub min_net_edge_bps: Decimal,
    /// Fraction of crossable depth targeted per entry (`(0, 1]`).
    #[serde(with = "rust_decimal::serde::str")]
    pub depth_fraction: Decimal,
    /// Hard maximum single-leg entry notional in USD (`> 0`).
    #[serde(with = "rust_decimal::serde::str")]
    pub max_entry_notional_usd: Decimal,
    /// Slippage protection band in basis points applied to `ProtectedIoc` orders (`[0, 10000)`).
    #[serde(with = "rust_decimal::serde::str")]
    pub ioc_slippage_bps: Decimal,
    /// Maximum number of book levels per side considered (`1..=20`).
    pub max_book_levels: usize,
    /// Maximum age in milliseconds accepted for entry `Book` snapshots (`> 0`).
    pub max_entry_book_age_ms: u64,
    /// Maximum age in milliseconds accepted when repricing a follow-up hedge from live `Book` (`> 0`).
    pub max_followup_book_age_ms: u64,
    /// Optional explicit cross-stablecoin hedge authorization and depeg policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_stablecoin_policy: Option<CrossStablecoinHedgePolicy>,
}

impl Default for CrossVenueRouterConfig {
    fn default() -> Self {
        Self {
            dual_taker_required: false,
            min_net_edge_bps: Decimal::ZERO,
            depth_fraction: Decimal::new(25, 2),
            max_entry_notional_usd: Decimal::ZERO,
            ioc_slippage_bps: Decimal::new(5, 0),
            max_book_levels: 5,
            max_entry_book_age_ms: DEFAULT_MAX_ENTRY_BOOK_AGE_MS,
            max_followup_book_age_ms: DEFAULT_MAX_FOLLOWUP_BOOK_AGE_MS,
            cross_stablecoin_policy: None,
        }
    }
}

impl CrossVenueRouterConfig {
    /// Validate configuration bounds fail-closed.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.min_net_edge_bps >= Decimal::ZERO,
            "min_net_edge_bps must be >= 0"
        );
        ensure!(
            self.depth_fraction > Decimal::ZERO && self.depth_fraction <= Decimal::ONE,
            "depth_fraction must be in (0, 1]"
        );
        ensure!(
            self.max_entry_notional_usd > Decimal::ZERO,
            "max_entry_notional_usd must be > 0"
        );
        ensure!(
            self.ioc_slippage_bps >= Decimal::ZERO && self.ioc_slippage_bps < BPS,
            "ioc_slippage_bps must be in [0, 10000)"
        );
        ensure!(
            (1..=MAX_ROUTER_BOOK_LEVELS).contains(&self.max_book_levels),
            "max_book_levels must be in 1..={MAX_ROUTER_BOOK_LEVELS}"
        );
        ensure!(
            self.max_entry_book_age_ms > 0,
            "max_entry_book_age_ms must be > 0"
        );
        ensure!(
            self.max_followup_book_age_ms > 0,
            "max_followup_book_age_ms must be > 0"
        );
        if let Some(policy) = &self.cross_stablecoin_policy {
            policy.validate()?;
        }
        Ok(())
    }
}

/// Wire order execution semantics emitted by the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchOrderKind {
    PostOnlyMaker,
    ProtectedIoc,
}

/// Paired trade direction across `[leg0, leg1]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteDirection {
    BuyLeg0SellLeg1,
    SellLeg0BuyLeg1,
}

impl RouteDirection {
    /// Returns `(buy_leg_index, sell_leg_index)`.
    pub fn buy_sell_legs(self) -> (usize, usize) {
        match self {
            Self::BuyLeg0SellLeg1 => (0, 1),
            Self::SellLeg0BuyLeg1 => (1, 0),
        }
    }

    /// Returns whether `leg_index` (`0` or `1`) is on the buy side.
    pub fn is_buy_on_leg(self, leg_index: usize) -> Result<bool> {
        match (self, leg_index) {
            (Self::BuyLeg0SellLeg1, 0) | (Self::SellLeg0BuyLeg1, 1) => Ok(true),
            (Self::BuyLeg0SellLeg1, 1) | (Self::SellLeg0BuyLeg1, 0) => Ok(false),
            _ => bail!("invalid two-leg index {leg_index}"),
        }
    }
}

/// Concrete bounded order intent produced for a single venue leg.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutedLegOrder {
    pub leg_index: usize,
    pub venue_id: String,
    pub liquidity_domain: String,
    pub is_buy: bool,
    pub reduce_only: bool,
    pub kind: DispatchOrderKind,
    pub action_class: OrderIntentClass,
    #[serde(with = "rust_decimal::serde::str")]
    pub size_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub limit_price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub worst_case_notional_usd: Decimal,
}

/// Write-ahead exposure reservation guarding in-flight sequential or dual-leg execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalExposureReservation {
    pub reservation_id: u64,
    pub topology: ExecutionTopology,
    pub direction: RouteDirection,
    #[serde(with = "rust_decimal::serde::str")]
    pub reserved_base_qty: Decimal,
    /// Worst-case single-leg unhedged USD exposure reserved prior to dispatch.
    #[serde(with = "rust_decimal::serde::str")]
    pub reserved_notional_usd: Decimal,
    pub created_at_ms: u64,
    pub first_leg_index: usize,
    pub hedge_leg_index: usize,
    pub hedge_is_buy: bool,
    pub settled_or_released: bool,
}

impl WalExposureReservation {
    /// Returns `true` until authoritatively released on zero fill or settled post-hedge.
    pub fn is_active(&self) -> bool {
        !self.settled_or_released
    }
}

/// Authoritative terminal outcome observed for the first leg of a two-step route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlowLegTerminalOutcome {
    Confirmed {
        #[serde(with = "rust_decimal::serde::str")]
        filled_base: Decimal,
        #[serde(
            default,
            with = "rust_decimal::serde::str_option",
            skip_serializing_if = "Option::is_none"
        )]
        avg_fill_price: Option<Decimal>,
    },
    RejectedOrCancelledZeroFill,
}

/// Fail-closed reason explaining why a cross-venue entry was blocked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouterBlockReason {
    InvalidConfiguration(String),
    WalReservationActive,
    TopologyRejected(String),
    TailRiskBlocked(TailRiskAction),
    OrderBudgetBlocked {
        leg_index: usize,
        cooldown_remaining_ms: u64,
    },
    ZeroPortfolioBudgetCap,
    InsufficientCrossableDepthOrEdge,
}

/// Causal entry dispatch plan produced by [`CrossVenueExecutionRouter::plan_entry`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouterEntryPlan {
    MakerFirst {
        reservation: WalExposureReservation,
        maker_order: RoutedLegOrder,
        planned_hedge_leg: usize,
        #[serde(with = "rust_decimal::serde::str")]
        expected_edge_bps: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        marginal_edge_bps: Decimal,
    },
    SlowLegFirst {
        reservation: WalExposureReservation,
        first_ioc: RoutedLegOrder,
        pending_second_leg: usize,
        #[serde(with = "rust_decimal::serde::str")]
        expected_edge_bps: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        marginal_edge_bps: Decimal,
    },
    ConcurrentDualIoc {
        reservation: WalExposureReservation,
        leg0_ioc: RoutedLegOrder,
        leg1_ioc: RoutedLegOrder,
        #[serde(with = "rust_decimal::serde::str")]
        expected_edge_bps: Decimal,
        #[serde(with = "rust_decimal::serde::str")]
        marginal_edge_bps: Decimal,
    },
    Blocked {
        reason: RouterBlockReason,
    },
}

/// Follow-up action emitted when the first leg reaches an authoritative terminal state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FollowupPlan {
    NoFillReleaseWal {
        reservation_id: u64,
    },
    DispatchFastHedgeIoc {
        reservation_id: u64,
        hedge_order: RoutedLegOrder,
    },
    RejectUnverifiedTerminalState,
}

/// Supervisory tail-risk inputs passed to [`CrossVenueExecutionRouter::plan_entry`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RouterTailRiskInput<'a> {
    pub action_override: Option<TailRiskAction>,
    pub venue_margins: &'a [VenueMarginSnapshot],
    pub stablecoin_pegs: &'a [StablecoinPegSample],
    pub tail_risk_config: Option<&'a TailRiskConfig>,
}

impl<'a> RouterTailRiskInput<'a> {
    pub fn from_action(action: TailRiskAction) -> Self {
        Self {
            action_override: Some(action),
            ..Self::default()
        }
    }

    pub fn from_snapshots(
        venue_margins: &'a [VenueMarginSnapshot],
        stablecoin_pegs: &'a [StablecoinPegSample],
    ) -> Self {
        Self {
            action_override: None,
            venue_margins,
            stablecoin_pegs,
            tail_risk_config: None,
        }
    }
}

/// Input parameters for [`CrossVenueExecutionRouter::plan_entry`].
#[derive(Debug, Clone, Copy)]
pub struct RouteEntryParams<'a> {
    pub now_ms: u64,
    pub direction: RouteDirection,
    pub legs: [&'a VenueExecutionCapability; 2],
    pub books: [&'a Book; 2],
    pub base_step: Decimal,
    pub base_per_contract: [Decimal; 2],
    pub price_ticks: [Decimal; 2],
    pub portfolio_leg_cap_usd: Option<Decimal>,
    pub tail_risk: RouterTailRiskInput<'a>,
}

#[derive(Debug, Clone)]
struct ActiveRouteContext {
    hedge_venue_id: String,
    hedge_liquidity_domain: String,
    hedge_market: String,
    hedge_tick: Decimal,
    hedge_base_per_contract: Decimal,
    planned_hedge_limit_price: Decimal,
    planned_hedge_book_timestamp_ms: u64,
    first_leg_filled_base: Option<Decimal>,
    hedge_intent_emitted: bool,
}

/// Stateful cross-venue execution router enforcing speed-bump causality,
/// WAL exposure reservation, portfolio caps, tail-risk gates, and order budgets.
#[derive(Debug, Clone)]
pub struct CrossVenueExecutionRouter {
    config: CrossVenueRouterConfig,
    active_reservation: Option<WalExposureReservation>,
    active_context: Option<ActiveRouteContext>,
    next_reservation_id: u64,
}

impl CrossVenueExecutionRouter {
    /// Construct a validated router with no active exposure reservation.
    pub fn new(config: CrossVenueRouterConfig) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            config,
            active_reservation: None,
            active_context: None,
            next_reservation_id: 1,
        })
    }

    pub fn config(&self) -> &CrossVenueRouterConfig {
        &self.config
    }

    pub fn active_reservation(&self) -> Option<&WalExposureReservation> {
        self.active_reservation.as_ref()
    }

    /// Evaluate all safety gates, select causal execution topology, size via
    /// marginal L2 depth, reserve WAL exposure, and emit bounded order intents.
    pub fn plan_entry(
        &mut self,
        params: RouteEntryParams<'_>,
        budgets: [&mut VenueOrderBudget; 2],
    ) -> RouterEntryPlan {
        if let Err(err) = self.validate_entry_inputs(&params) {
            return RouterEntryPlan::Blocked {
                reason: RouterBlockReason::InvalidConfiguration(err.to_string()),
            };
        }

        if self
            .active_reservation
            .as_ref()
            .is_some_and(WalExposureReservation::is_active)
        {
            return RouterEntryPlan::Blocked {
                reason: RouterBlockReason::WalReservationActive,
            };
        }

        let topology = match self.evaluate_topology_and_tail_risk(&params) {
            Ok(topology) => topology,
            Err(reason) => return RouterEntryPlan::Blocked { reason },
        };

        let mut effective_notional_cap = self.config.max_entry_notional_usd;
        if let Some(cap) = params.portfolio_leg_cap_usd {
            if cap <= Decimal::ZERO {
                return RouterEntryPlan::Blocked {
                    reason: RouterBlockReason::ZeroPortfolioBudgetCap,
                };
            }
            effective_notional_cap = effective_notional_cap.min(cap);
        }

        if !valid_pair_books(
            params.books,
            params.now_ms,
            self.config.max_entry_book_age_ms,
        ) {
            return RouterEntryPlan::Blocked {
                reason: RouterBlockReason::InsufficientCrossableDepthOrEdge,
            };
        }
        effective_notional_cap = effective_notional_cap
            .min(params.books[0].available_notional)
            .min(params.books[1].available_notional);

        let (buy_leg, sell_leg) = params.direction.buy_sell_legs();
        let leg_fees = match derive_leg_fee_bps(topology, params.legs) {
            Ok(fees) => fees,
            Err(err) => {
                return RouterEntryPlan::Blocked {
                    reason: RouterBlockReason::InvalidConfiguration(err.to_string()),
                };
            }
        };

        let buy_asks = truncate_levels(&params.books[buy_leg].asks, self.config.max_book_levels);
        let sell_bids = truncate_levels(&params.books[sell_leg].bids, self.config.max_book_levels);

        let ref_price = params.books[buy_leg].asks[0]
            .price
            .max(params.books[sell_leg].bids[0].price);
        let Some(raw_cap_base) = effective_notional_cap.checked_div(ref_price) else {
            return RouterEntryPlan::Blocked {
                reason: RouterBlockReason::InsufficientCrossableDepthOrEdge,
            };
        };
        let prelim_max_base = match floor_step_checked(raw_cap_base, params.base_step) {
            Ok(qty) if qty >= params.base_step => qty,
            _ => {
                return RouterEntryPlan::Blocked {
                    reason: RouterBlockReason::InsufficientCrossableDepthOrEdge,
                };
            }
        };

        let cross_params = MarginalCrossParams {
            buy_fee_bps: leg_fees[buy_leg],
            sell_fee_bps: leg_fees[sell_leg],
            min_net_edge_bps: self.config.min_net_edge_bps,
            take_fraction: self.config.depth_fraction,
            max_base: prelim_max_base,
            min_base: params.base_step,
            step_base: params.base_step,
            buy_tick: params.price_ticks[buy_leg],
            sell_tick: params.price_ticks[sell_leg],
        };

        let mut quote = match crossable_marginal_depth(
            buy_asks,
            sell_bids,
            params.base_per_contract[buy_leg],
            params.base_per_contract[sell_leg],
            &cross_params,
        ) {
            Ok(Some(q)) => q,
            Ok(None) | Err(_) => {
                return RouterEntryPlan::Blocked {
                    reason: RouterBlockReason::InsufficientCrossableDepthOrEdge,
                };
            }
        };

        let mut leg_orders = match self.build_leg_orders(&params, topology, &quote) {
            Ok(orders) => orders,
            Err(_) => {
                return RouterEntryPlan::Blocked {
                    reason: RouterBlockReason::InsufficientCrossableDepthOrEdge,
                };
            }
        };

        // Ensure slippage-protected worst-case leg notional remains within budget cap.
        let worst_leg_px = leg_orders[0].limit_price.max(leg_orders[1].limit_price);
        if leg_orders[0].worst_case_notional_usd > effective_notional_cap
            || leg_orders[1].worst_case_notional_usd > effective_notional_cap
        {
            let Some(raw_capped) = effective_notional_cap.checked_div(worst_leg_px) else {
                return RouterEntryPlan::Blocked {
                    reason: RouterBlockReason::InsufficientCrossableDepthOrEdge,
                };
            };
            let capped_base = match floor_step_checked(raw_capped, params.base_step) {
                Ok(qty) if qty >= params.base_step => qty,
                _ => {
                    return RouterEntryPlan::Blocked {
                        reason: RouterBlockReason::InsufficientCrossableDepthOrEdge,
                    };
                }
            };
            let tightened = MarginalCrossParams {
                max_base: capped_base,
                ..cross_params
            };
            quote = match crossable_marginal_depth(
                buy_asks,
                sell_bids,
                params.base_per_contract[buy_leg],
                params.base_per_contract[sell_leg],
                &tightened,
            ) {
                Ok(Some(q)) => q,
                Ok(None) | Err(_) => {
                    return RouterEntryPlan::Blocked {
                        reason: RouterBlockReason::InsufficientCrossableDepthOrEdge,
                    };
                }
            };
            leg_orders = match self.build_leg_orders(&params, topology, &quote) {
                Ok(orders) => orders,
                Err(_) => {
                    return RouterEntryPlan::Blocked {
                        reason: RouterBlockReason::InsufficientCrossableDepthOrEdge,
                    };
                }
            };
        }

        if let Err(reason) = admit_and_record_entry_budgets(params.now_ms, topology, budgets) {
            return RouterEntryPlan::Blocked { reason };
        }

        let (first_leg_index, hedge_leg_index) = match topology {
            ExecutionTopology::MakerFirst {
                maker_leg,
                hedge_leg,
            } => (maker_leg, hedge_leg),
            ExecutionTopology::SlowLegFirst {
                first_leg,
                second_leg,
            } => (first_leg, second_leg),
            ExecutionTopology::ConcurrentProtectedIoc => (0, 1),
        };
        let hedge_is_buy = match params.direction.is_buy_on_leg(hedge_leg_index) {
            Ok(flag) => flag,
            Err(err) => {
                return RouterEntryPlan::Blocked {
                    reason: RouterBlockReason::InvalidConfiguration(err.to_string()),
                };
            }
        };

        let reservation_id = self.next_reservation_id;
        self.next_reservation_id = self.next_reservation_id.saturating_add(1);
        let reserved_notional_usd = leg_orders[0]
            .worst_case_notional_usd
            .max(leg_orders[1].worst_case_notional_usd);

        let reservation = WalExposureReservation {
            reservation_id,
            topology,
            direction: params.direction,
            reserved_base_qty: quote.base_quantity,
            reserved_notional_usd,
            created_at_ms: params.now_ms,
            first_leg_index,
            hedge_leg_index,
            hedge_is_buy,
            settled_or_released: false,
        };

        self.active_reservation = Some(reservation.clone());
        self.active_context = Some(ActiveRouteContext {
            hedge_venue_id: params.legs[hedge_leg_index].venue_id.to_string(),
            hedge_liquidity_domain: params.legs[hedge_leg_index].liquidity_domain.to_string(),
            hedge_market: params.books[hedge_leg_index].market.clone(),
            hedge_tick: params.price_ticks[hedge_leg_index],
            hedge_base_per_contract: params.base_per_contract[hedge_leg_index],
            planned_hedge_limit_price: leg_orders[hedge_leg_index].limit_price,
            planned_hedge_book_timestamp_ms: params.books[hedge_leg_index].timestamp_ms,
            first_leg_filled_base: None,
            hedge_intent_emitted: false,
        });

        let [leg0_order, leg1_order] = leg_orders;
        match topology {
            ExecutionTopology::MakerFirst {
                maker_leg,
                hedge_leg,
            } => {
                let maker_order = if maker_leg == 0 {
                    leg0_order
                } else {
                    leg1_order
                };
                RouterEntryPlan::MakerFirst {
                    reservation,
                    maker_order,
                    planned_hedge_leg: hedge_leg,
                    expected_edge_bps: quote.vwap_net_edge_bps,
                    marginal_edge_bps: quote.marginal_net_edge_bps,
                }
            }
            ExecutionTopology::SlowLegFirst {
                first_leg,
                second_leg,
            } => {
                let first_ioc = if first_leg == 0 {
                    leg0_order
                } else {
                    leg1_order
                };
                RouterEntryPlan::SlowLegFirst {
                    reservation,
                    first_ioc,
                    pending_second_leg: second_leg,
                    expected_edge_bps: quote.vwap_net_edge_bps,
                    marginal_edge_bps: quote.marginal_net_edge_bps,
                }
            }
            ExecutionTopology::ConcurrentProtectedIoc => RouterEntryPlan::ConcurrentDualIoc {
                reservation,
                leg0_ioc: leg0_order,
                leg1_ioc: leg1_order,
                expected_edge_bps: quote.vwap_net_edge_bps,
                marginal_edge_bps: quote.marginal_net_edge_bps,
            },
        }
    }

    /// Process the authoritative terminal outcome of the first leg in a `MakerFirst`
    /// or `SlowLegFirst` reservation.
    ///
    /// - Zero-fill or clean rejection releases the WAL reservation before any positive fill.
    /// - After a valid positive fill, only the exact same base quantity can retry before intent emission.
    /// - Positive confirmed fill dispatches a fast `ProtectedIoc` hedge tagged
    ///   [`OrderIntentClass::EmergencyHedge`] (never blocked by 429 cooldown/halt)
    ///   while keeping the WAL reservation active until [`Self::confirm_hedge_settled`].
    /// - Mismatched IDs, overfills, duplicate or conflicting outcomes after hedge
    ///   intent emission, or non-sequential topologies fail closed.
    pub fn on_first_leg_terminal(
        &mut self,
        now_ms: u64,
        reservation_id: u64,
        outcome: SlowLegTerminalOutcome,
        current_hedge_book: Option<&Book>,
        hedge_budget: &mut VenueOrderBudget,
    ) -> FollowupPlan {
        let Some(res) = self.active_reservation.as_ref() else {
            return FollowupPlan::RejectUnverifiedTerminalState;
        };
        if !res.is_active()
            || res.reservation_id != reservation_id
            || res.topology == ExecutionTopology::ConcurrentProtectedIoc
            || now_ms < res.created_at_ms
        {
            return FollowupPlan::RejectUnverifiedTerminalState;
        }

        let Some(ctx) = self.active_context.as_mut() else {
            return FollowupPlan::RejectUnverifiedTerminalState;
        };
        // After emitting a hedge intent, duplicate or conflicting first-leg
        // evidence cannot emit another intent or release possible exposure.
        if ctx.hedge_intent_emitted {
            return FollowupPlan::RejectUnverifiedTerminalState;
        }

        if let Some(observed_base) = ctx.first_leg_filled_base
            && !matches!(
                &outcome,
                SlowLegTerminalOutcome::Confirmed { filled_base, .. } if *filled_base == observed_base
            )
        {
            return FollowupPlan::RejectUnverifiedTerminalState;
        }

        match outcome {
            SlowLegTerminalOutcome::RejectedOrCancelledZeroFill => {
                self.active_reservation = None;
                self.active_context = None;
                FollowupPlan::NoFillReleaseWal { reservation_id }
            }
            SlowLegTerminalOutcome::Confirmed {
                filled_base,
                avg_fill_price: _,
            } => {
                if filled_base < Decimal::ZERO || filled_base > res.reserved_base_qty {
                    return FollowupPlan::RejectUnverifiedTerminalState;
                }
                if filled_base == Decimal::ZERO {
                    self.active_reservation = None;
                    self.active_context = None;
                    return FollowupPlan::NoFillReleaseWal { reservation_id };
                }

                // A confirmed fill is evidence of exposure even when the hedge cannot be priced.
                ctx.first_leg_filled_base = Some(filled_base);

                let hedge_limit_price = match Self::compute_followup_hedge_limit(
                    &self.config,
                    now_ms,
                    res.hedge_is_buy,
                    filled_base,
                    current_hedge_book,
                    ctx,
                ) {
                    Ok(px) if px > Decimal::ZERO => px,
                    _ => return FollowupPlan::RejectUnverifiedTerminalState,
                };
                let Some(worst_case_notional_usd) = filled_base.checked_mul(hedge_limit_price)
                else {
                    return FollowupPlan::RejectUnverifiedTerminalState;
                };

                let decision = hedge_budget.admit(now_ms, OrderIntentClass::EmergencyHedge, 1);
                if !decision.is_allowed() {
                    return FollowupPlan::RejectUnverifiedTerminalState;
                }
                hedge_budget.record_dispatch(now_ms, 1);
                ctx.hedge_intent_emitted = true;

                FollowupPlan::DispatchFastHedgeIoc {
                    reservation_id,
                    hedge_order: RoutedLegOrder {
                        leg_index: res.hedge_leg_index,
                        venue_id: ctx.hedge_venue_id.clone(),
                        liquidity_domain: ctx.hedge_liquidity_domain.clone(),
                        is_buy: res.hedge_is_buy,
                        reduce_only: false,
                        kind: DispatchOrderKind::ProtectedIoc,
                        action_class: OrderIntentClass::EmergencyHedge,
                        size_base: filled_base,
                        limit_price: hedge_limit_price,
                        worst_case_notional_usd,
                    },
                }
            }
        }
    }

    /// Release an active WAL reservation after the hedge (or both concurrent IOC legs)
    /// has authoritatively settled.
    pub fn confirm_hedge_settled(&mut self, reservation_id: u64) -> Result<WalExposureReservation> {
        let Some(mut res) = self.active_reservation.take() else {
            bail!("no active WAL reservation to settle");
        };
        if !res.is_active() || res.reservation_id != reservation_id {
            self.active_reservation = Some(res);
            bail!("WAL reservation mismatch for id {reservation_id}");
        }
        if res.topology != ExecutionTopology::ConcurrentProtectedIoc
            && !self
                .active_context
                .as_ref()
                .is_some_and(|ctx| ctx.hedge_intent_emitted)
        {
            self.active_reservation = Some(res);
            bail!(
                "sequential WAL reservation {reservation_id} cannot settle before hedge intent is emitted"
            );
        }
        res.settled_or_released = true;
        self.active_context = None;
        Ok(res)
    }

    fn validate_entry_inputs(&self, params: &RouteEntryParams<'_>) -> Result<()> {
        self.config.validate()?;
        ensure!(params.now_ms > 0, "now_ms must be > 0");
        ensure!(params.base_step > Decimal::ZERO, "base_step must be > 0");
        for (i, tick) in params.price_ticks.iter().enumerate() {
            ensure!(*tick > Decimal::ZERO, "price_ticks[{i}] must be > 0");
        }
        for (i, bpc) in params.base_per_contract.iter().enumerate() {
            ensure!(*bpc > Decimal::ZERO, "base_per_contract[{i}] must be > 0");
        }
        Ok(())
    }

    fn evaluate_topology_and_tail_risk(
        &self,
        params: &RouteEntryParams<'_>,
    ) -> std::result::Result<ExecutionTopology, RouterBlockReason> {
        let mut worst_action = params
            .tail_risk
            .action_override
            .unwrap_or(TailRiskAction::Normal);
        if worst_action == TailRiskAction::HaltAndFlatten {
            return Err(RouterBlockReason::TailRiskBlocked(worst_action));
        }

        if let Some(policy) = &self.config.cross_stablecoin_policy {
            let eval = select_execution_topology_with_policy(
                params.legs[0],
                params.legs[1],
                self.config.dual_taker_required,
                params.now_ms,
                params.tail_risk.venue_margins,
                params.tail_risk.stablecoin_pegs,
                policy,
            )
            .map_err(|err| RouterBlockReason::TopologyRejected(err.to_string()))?;

            worst_action = worst_action.max(eval.action);
            if worst_action != TailRiskAction::Normal {
                return Err(RouterBlockReason::TailRiskBlocked(worst_action));
            }
            return Ok(eval.topology);
        }

        if params.tail_risk.tail_risk_config.is_some() || !params.tail_risk.venue_margins.is_empty()
        {
            let default_cfg = TailRiskConfig::default();
            let cfg = params.tail_risk.tail_risk_config.unwrap_or(&default_cfg);
            let assessment = evaluate_tail_risk(
                params.now_ms,
                params.tail_risk.venue_margins,
                params.tail_risk.stablecoin_pegs,
                cfg,
            )
            .map_err(|err| RouterBlockReason::TopologyRejected(err.to_string()))?;
            worst_action = worst_action.max(assessment.action);
        } else if !params.tail_risk.stablecoin_pegs.is_empty() {
            return Err(RouterBlockReason::TopologyRejected(
                "stablecoin peg samples require venue margin snapshots".to_string(),
            ));
        }

        if worst_action != TailRiskAction::Normal {
            return Err(RouterBlockReason::TailRiskBlocked(worst_action));
        }

        select_execution_topology(
            params.legs[0],
            params.legs[1],
            self.config.dual_taker_required,
        )
        .map_err(|err| RouterBlockReason::TopologyRejected(err.to_string()))
    }

    fn build_leg_orders(
        &self,
        params: &RouteEntryParams<'_>,
        topology: ExecutionTopology,
        quote: &MarginalCrossQuote,
    ) -> Result<[RoutedLegOrder; 2]> {
        let build_one = |leg_idx: usize| -> Result<RoutedLegOrder> {
            let is_buy = params.direction.is_buy_on_leg(leg_idx)?;
            let kind = match topology {
                ExecutionTopology::MakerFirst { maker_leg, .. } if maker_leg == leg_idx => {
                    DispatchOrderKind::PostOnlyMaker
                }
                ExecutionTopology::MakerFirst { .. }
                | ExecutionTopology::SlowLegFirst { .. }
                | ExecutionTopology::ConcurrentProtectedIoc => DispatchOrderKind::ProtectedIoc,
            };
            let tick = params.price_ticks[leg_idx];
            let book = params.books[leg_idx];
            let limit_price = match kind {
                DispatchOrderKind::PostOnlyMaker => {
                    compute_post_only_limit(is_buy, book, quote, tick)?
                }
                DispatchOrderKind::ProtectedIoc => {
                    let base_px = if is_buy {
                        quote.buy_limit_price
                    } else {
                        quote.sell_limit_price
                    };
                    apply_ioc_protection(is_buy, base_px, self.config.ioc_slippage_bps, tick)?
                }
            };
            let worst_case_notional_usd = quote
                .base_quantity
                .checked_mul(limit_price)
                .context("leg notional overflow")?;
            Ok(RoutedLegOrder {
                leg_index: leg_idx,
                venue_id: params.legs[leg_idx].venue_id.to_string(),
                liquidity_domain: params.legs[leg_idx].liquidity_domain.to_string(),
                is_buy,
                reduce_only: false,
                kind,
                action_class: OrderIntentClass::RiskIncreasingEntry,
                size_base: quote.base_quantity,
                limit_price,
                worst_case_notional_usd,
            })
        };

        Ok([build_one(0)?, build_one(1)?])
    }

    fn compute_followup_hedge_limit(
        config: &CrossVenueRouterConfig,
        now_ms: u64,
        hedge_is_buy: bool,
        filled_base: Decimal,
        current_hedge_book: Option<&Book>,
        ctx: &ActiveRouteContext,
    ) -> Result<Decimal> {
        let Some(book) = current_hedge_book else {
            return Ok(ctx.planned_hedge_limit_price);
        };
        ensure!(
            book.market == ctx.hedge_market,
            "follow-up hedge book market mismatch"
        );
        ensure!(
            book.timestamp_ms > 0
                && book.timestamp_ms <= now_ms
                && book.timestamp_ms >= ctx.planned_hedge_book_timestamp_ms,
            "follow-up hedge book clock violates causality or regressed"
        );
        ensure!(
            now_ms - book.timestamp_ms <= config.max_followup_book_age_ms,
            "follow-up hedge book exceeded max_followup_book_age_ms"
        );
        if !book.tradable {
            return Ok(ctx.planned_hedge_limit_price);
        }
        let levels = if hedge_is_buy { &book.asks } else { &book.bids };
        if levels.is_empty() {
            return Ok(ctx.planned_hedge_limit_price);
        }
        let worst_px = sweep_worst_level_price(levels, ctx.hedge_base_per_contract, filled_base)?;
        apply_ioc_protection(
            hedge_is_buy,
            worst_px,
            config.ioc_slippage_bps,
            ctx.hedge_tick,
        )
    }
}

/// Convert a [`CarryBudgetAllocation`] leg subset into a conservative pair notional cap in USD.
pub fn portfolio_pair_cap_usd(
    allocation: &CarryBudgetAllocation,
    leg_indices: [usize; 2],
) -> Result<Decimal> {
    ensure!(
        leg_indices[0] != leg_indices[1],
        "leg_indices must reference two distinct legs"
    );
    let mut min_cap = Decimal::MAX;
    for idx in leg_indices {
        let val = *allocation
            .leg_notionals_usd
            .get(idx)
            .with_context(|| format!("allocation missing leg_notionals_usd[{idx}]"))?;
        ensure!(
            val.is_finite() && val >= 0.0,
            "leg_notionals_usd[{idx}] must be finite and >= 0"
        );
        let dec = Decimal::from_f64_retain(val)
            .with_context(|| format!("leg_notionals_usd[{idx}] out of Decimal range"))?;
        min_cap = min_cap.min(dec);
    }
    Ok(min_cap)
}

fn valid_pair_books(books: [&Book; 2], now_ms: u64, max_entry_book_age_ms: u64) -> bool {
    now_ms > 0
        && max_entry_book_age_ms > 0
        && books.iter().all(|b| {
            !b.market.trim().is_empty()
                && b.timestamp_ms > 0
                && b.timestamp_ms <= now_ms
                && now_ms - b.timestamp_ms <= max_entry_book_age_ms
                && b.tradable
                && b.available_notional > Decimal::ZERO
                && !b.bids.is_empty()
                && !b.asks.is_empty()
                && b.bids[0].price > Decimal::ZERO
                && b.asks[0].price > Decimal::ZERO
                && b.bids[0].price < b.asks[0].price
        })
}

fn truncate_levels(levels: &[Level], max_levels: usize) -> &[Level] {
    &levels[..levels.len().min(max_levels)]
}

fn derive_leg_fee_bps(
    topology: ExecutionTopology,
    legs: [&VenueExecutionCapability; 2],
) -> Result<[Decimal; 2]> {
    for (idx, cap) in legs.iter().enumerate() {
        ensure!(
            cap.taker_fee_bps >= Decimal::ZERO && cap.taker_fee_bps < BPS,
            "legs[{idx}].taker_fee_bps must be in [0, 10000)"
        );
        ensure!(
            cap.maker_fee_bps > -BPS && cap.maker_fee_bps < BPS,
            "legs[{idx}].maker_fee_bps must be in (-10000, 10000)"
        );
    }
    Ok(match topology {
        ExecutionTopology::MakerFirst {
            maker_leg,
            hedge_leg,
        } => {
            let mut out = [Decimal::ZERO; 2];
            out[maker_leg] = legs[maker_leg].maker_fee_bps;
            out[hedge_leg] = legs[hedge_leg].taker_fee_bps;
            out
        }
        ExecutionTopology::SlowLegFirst { .. } | ExecutionTopology::ConcurrentProtectedIoc => {
            [legs[0].taker_fee_bps, legs[1].taker_fee_bps]
        }
    })
}

fn floor_step_checked(value: Decimal, step: Decimal) -> Result<Decimal> {
    ensure!(
        value >= Decimal::ZERO && step > Decimal::ZERO,
        "floor_step_checked requires non-negative value and positive step"
    );
    floor_step(value, step)
}

fn round_price_down_to_tick(price: Decimal, tick: Decimal) -> Result<Decimal> {
    ensure!(
        price >= Decimal::ZERO && tick > Decimal::ZERO,
        "round_price_down_to_tick requires non-negative price and positive tick"
    );
    floor_step(price, tick)
}

fn round_price_up_to_tick(price: Decimal, tick: Decimal) -> Result<Decimal> {
    let down = round_price_down_to_tick(price, tick)?;
    if down == price {
        Ok(down)
    } else {
        down.checked_add(tick)
            .context("price tick rounding overflow")
    }
}

fn apply_ioc_protection(
    is_buy: bool,
    base_price: Decimal,
    slippage_bps: Decimal,
    tick: Decimal,
) -> Result<Decimal> {
    ensure!(base_price > Decimal::ZERO, "base_price must be positive");
    let slip_frac = slippage_bps
        .checked_div(BPS)
        .context("slippage fraction overflow")?;
    if is_buy {
        let factor = Decimal::ONE
            .checked_add(slip_frac)
            .context("buy slippage factor overflow")?;
        let raw = base_price
            .checked_mul(factor)
            .context("buy protected limit overflow")?;
        round_price_up_to_tick(raw, tick)
    } else {
        let factor = Decimal::ONE
            .checked_sub(slip_frac)
            .context("sell slippage factor overflow")?;
        ensure!(factor > Decimal::ZERO, "sell slippage factor must be > 0");
        let raw = base_price
            .checked_mul(factor)
            .context("sell protected limit overflow")?;
        let rounded = round_price_down_to_tick(raw, tick)?;
        ensure!(
            rounded > Decimal::ZERO,
            "sell protected limit rounded to zero"
        );
        Ok(rounded)
    }
}

fn compute_post_only_limit(
    is_buy: bool,
    maker_book: &Book,
    quote: &MarginalCrossQuote,
    tick: Decimal,
) -> Result<Decimal> {
    if is_buy {
        let best_ask = maker_book.asks[0].price;
        let max_passive = best_ask
            .checked_sub(tick)
            .context("maker bid tick subtraction underflow")?;
        ensure!(
            max_passive > Decimal::ZERO,
            "maker bid post-only price non-positive"
        );
        let passive_cap = round_price_down_to_tick(max_passive, tick)?;
        ensure!(
            passive_cap > Decimal::ZERO && passive_cap < best_ask,
            "maker bid cannot remain strictly post-only"
        );
        Ok(quote.buy_limit_price.min(passive_cap))
    } else {
        let best_bid = maker_book.bids[0].price;
        let min_passive = best_bid
            .checked_add(tick)
            .context("maker ask tick addition overflow")?;
        let passive_floor = round_price_up_to_tick(min_passive, tick)?;
        ensure!(
            passive_floor > best_bid,
            "maker ask cannot remain strictly post-only"
        );
        Ok(quote.sell_limit_price.max(passive_floor))
    }
}

fn sweep_worst_level_price(
    levels: &[Level],
    base_per_contract: Decimal,
    target_base: Decimal,
) -> Result<Decimal> {
    ensure!(target_base > Decimal::ZERO, "target_base must be positive");
    let mut remaining = target_base;
    let mut worst_px = Decimal::ZERO;
    for level in levels {
        ensure!(
            level.price > Decimal::ZERO && level.contracts > Decimal::ZERO,
            "book level must be positive"
        );
        let level_base = level
            .contracts
            .checked_mul(base_per_contract)
            .context("hedge level base overflow")?;
        worst_px = level.price;
        if remaining <= level_base {
            break;
        }
        remaining -= level_base;
    }
    ensure!(worst_px > Decimal::ZERO, "empty hedge book");
    Ok(worst_px)
}

fn admit_and_record_entry_budgets(
    now_ms: u64,
    topology: ExecutionTopology,
    budgets: [&mut VenueOrderBudget; 2],
) -> std::result::Result<(), RouterBlockReason> {
    let check_leg = |budget: &mut VenueOrderBudget, leg_index: usize| {
        let decision = budget.admit(now_ms, OrderIntentClass::RiskIncreasingEntry, 1);
        if decision.is_allowed() {
            Ok(())
        } else {
            let cooldown_remaining_ms = match decision {
                BudgetDecision::BlockedCooldown { remaining_ms } => remaining_ms,
                _ => budget.cooldown_remaining_ms(now_ms),
            };
            Err(RouterBlockReason::OrderBudgetBlocked {
                leg_index,
                cooldown_remaining_ms,
            })
        }
    };

    match topology {
        ExecutionTopology::MakerFirst { maker_leg, .. } => {
            check_leg(budgets[maker_leg], maker_leg)?;
            budgets[maker_leg].record_dispatch(now_ms, 1);
        }
        ExecutionTopology::SlowLegFirst { first_leg, .. } => {
            check_leg(budgets[first_leg], first_leg)?;
            budgets[first_leg].record_dispatch(now_ms, 1);
        }
        ExecutionTopology::ConcurrentProtectedIoc => {
            let [b0, b1] = budgets;
            check_leg(b0, 0)?;
            check_leg(b1, 1)?;
            b0.record_dispatch(now_ms, 1);
            b1.record_dispatch(now_ms, 1);
        }
    }
    Ok(())
}

/// Deterministic fault-injection comparison result contrasting naive concurrent IOC
/// against [`CrossVenueExecutionRouter`] under a mid-flight market jump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FaultInjectionComparison {
    pub naive_topology: ExecutionTopology,
    pub router_maker_topology: ExecutionTopology,
    pub router_dual_taker_topology: ExecutionTopology,
    #[serde(with = "rust_decimal::serde::str")]
    pub naive_hl_fill_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub naive_lighter_fill_base: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub naive_legged_out_pnl_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub router_maker_legged_out_pnl_usd: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub router_dual_taker_legged_out_pnl_usd: Decimal,
}

/// Deterministic fault-injection harness comparing naive `ConcurrentProtectedIoc`
/// against [`CrossVenueExecutionRouter`] on Hyperliquid (`0 ms`) vs Lighter (`300 ms`)
/// under a `+25 bp` mid-flight price jump at `t = 150 ms`.
pub fn simulate_midflight_jump_comparison() -> Result<FaultInjectionComparison> {
    use crate::{order_budget::OrderBudgetConfig, venues::VenueRegistry};

    let registry = VenueRegistry::default();
    let hl_cap = registry.resolve_execution_capability("hyperliquid", "base", "crypto")?;
    let lighter_cap = registry.resolve_execution_capability("lighter", "standard", "crypto")?;
    let legs = [&hl_cap, &lighter_cap];

    let mk_book = |market: &str, ts_ms: u64, bid: Decimal, ask: Decimal| Book {
        market: market.to_string(),
        timestamp_ms: ts_ms,
        tradable: true,
        bids: vec![Level {
            price: bid,
            contracts: Decimal::from(10),
        }],
        asks: vec![Level {
            price: ask,
            contracts: Decimal::from(10),
        }],
        bounds: None,
        available_notional: Decimal::from(10_000),
    };

    let t0 = 1_700_000_000_000_u64;
    let hl_t0 = mk_book("ETH-HL", t0, Decimal::new(10018, 2), Decimal::new(10020, 2));
    let lt_t0 = mk_book("ETH-LT", t0, Decimal::new(9998, 2), Decimal::new(10000, 2));

    // +25 bp upward jump at t = 150 ms across both venues.
    let jump_factor = Decimal::ONE + Decimal::new(25, 0) / BPS;
    let hl_bid_t150 = (hl_t0.bids[0].price * jump_factor).round_dp(2);
    let hl_ask_t150 = (hl_t0.asks[0].price * jump_factor).round_dp(2);
    let lt_bid_t150 = (lt_t0.bids[0].price * jump_factor).round_dp(2);
    let lt_ask_t150 = (lt_t0.asks[0].price * jump_factor).round_dp(2);
    let hl_t150 = mk_book("ETH-HL", t0 + 150, hl_bid_t150, hl_ask_t150);
    let lt_t150 = mk_book("ETH-LT", t0 + 150, lt_bid_t150, lt_ask_t150);

    // 1. Naive ConcurrentProtectedIoc (ignores 300ms Lighter taker speed-bump).
    let naive_qty = Decimal::from(5);
    let naive_hl_sell_px = hl_t0.bids[0].price; // 100.18 filled at t = 5ms (0ms venue delay)
    let naive_lt_buy_limit = apply_ioc_protection(
        true,
        lt_t0.asks[0].price,
        Decimal::new(5, 0),
        Decimal::new(1, 2),
    )?;
    // At t = 300ms after +25bp jump, Lighter ask (100.25) > naive protected buy limit (100.05) -> 0 fill.
    let naive_lighter_fill_base = if lt_t150.asks[0].price <= naive_lt_buy_limit {
        naive_qty
    } else {
        Decimal::ZERO
    };
    let naive_legged_out_pnl_usd = if naive_lighter_fill_base == Decimal::ZERO {
        let flatten_px = hl_t150.asks[0].price;
        let gross = (naive_hl_sell_px - flatten_px) * naive_qty;
        let entry_fee = naive_hl_sell_px * naive_qty * hl_cap.taker_fee_bps / BPS;
        let flatten_fee = flatten_px * naive_qty * hl_cap.taker_fee_bps / BPS;
        gross - entry_fee - flatten_fee
    } else {
        Decimal::ZERO
    };

    let base_cfg = CrossVenueRouterConfig {
        dual_taker_required: false,
        min_net_edge_bps: Decimal::new(5, 0),
        depth_fraction: Decimal::new(5, 1),
        max_entry_notional_usd: Decimal::from(1_000),
        ioc_slippage_bps: Decimal::new(5, 0),
        max_book_levels: 5,
        max_entry_book_age_ms: DEFAULT_MAX_ENTRY_BOOK_AGE_MS,
        max_followup_book_age_ms: DEFAULT_MAX_FOLLOWUP_BOOK_AGE_MS,
        cross_stablecoin_policy: None,
    };

    // 2. Causal MakerFirst (dual_taker_required = false): Lighter PostOnly buy rests passively, never crosses.
    let mut maker_router = CrossVenueExecutionRouter::new(base_cfg.clone())?;
    let mut b0 = VenueOrderBudget::new(OrderBudgetConfig::default())?;
    let mut b1 = VenueOrderBudget::new(OrderBudgetConfig::default())?;
    let maker_plan = maker_router.plan_entry(
        RouteEntryParams {
            now_ms: t0,
            direction: RouteDirection::SellLeg0BuyLeg1,
            legs,
            books: [&hl_t0, &lt_t0],
            base_step: Decimal::new(1, 1),
            base_per_contract: [Decimal::ONE, Decimal::ONE],
            price_ticks: [Decimal::new(1, 2), Decimal::new(1, 2)],
            portfolio_leg_cap_usd: None,
            tail_risk: RouterTailRiskInput::default(),
        },
        [&mut b0, &mut b1],
    );
    let RouterEntryPlan::MakerFirst {
        reservation: maker_res,
        maker_order,
        ..
    } = maker_plan
    else {
        bail!("expected MakerFirst plan for HL vs Lighter with dual_taker_required=false");
    };
    // At t = 150ms market jumps +25bp UP away from passive bid -> cancelled with zero fill.
    ensure!(
        maker_order.limit_price < lt_t150.asks[0].price,
        "maker bid unexpectedly crossed jumped market"
    );
    let maker_followup = maker_router.on_first_leg_terminal(
        t0 + 160,
        maker_res.reservation_id,
        SlowLegTerminalOutcome::RejectedOrCancelledZeroFill,
        Some(&hl_t150),
        &mut b0,
    );
    ensure!(
        matches!(maker_followup, FollowupPlan::NoFillReleaseWal { .. }),
        "expected NoFillReleaseWal for MakerFirst"
    );

    // 3. Causal SlowLegFirst (dual_taker_required = true): Lighter slow IOC goes first, HL stays untouched.
    let mut taker_router = CrossVenueExecutionRouter::new(CrossVenueRouterConfig {
        dual_taker_required: true,
        ..base_cfg
    })?;
    let mut tb0 = VenueOrderBudget::new(OrderBudgetConfig::default())?;
    let mut tb1 = VenueOrderBudget::new(OrderBudgetConfig::default())?;
    let taker_plan = taker_router.plan_entry(
        RouteEntryParams {
            now_ms: t0,
            direction: RouteDirection::SellLeg0BuyLeg1,
            legs,
            books: [&hl_t0, &lt_t0],
            base_step: Decimal::new(1, 1),
            base_per_contract: [Decimal::ONE, Decimal::ONE],
            price_ticks: [Decimal::new(1, 2), Decimal::new(1, 2)],
            portfolio_leg_cap_usd: None,
            tail_risk: RouterTailRiskInput::default(),
        },
        [&mut tb0, &mut tb1],
    );
    let RouterEntryPlan::SlowLegFirst {
        reservation: taker_res,
        first_ioc,
        ..
    } = taker_plan
    else {
        bail!("expected SlowLegFirst plan for HL vs Lighter with dual_taker_required=true");
    };
    // At t = 300ms Lighter IOC misses jumped ask -> zero fill, HL untouched -> $0 PnL.
    let slow_outcome = if lt_t150.asks[0].price <= first_ioc.limit_price {
        SlowLegTerminalOutcome::Confirmed {
            filled_base: first_ioc.size_base,
            avg_fill_price: Some(lt_t150.asks[0].price),
        }
    } else {
        SlowLegTerminalOutcome::RejectedOrCancelledZeroFill
    };
    let taker_followup = taker_router.on_first_leg_terminal(
        t0 + 300,
        taker_res.reservation_id,
        slow_outcome,
        Some(&hl_t150),
        &mut tb0,
    );
    ensure!(
        matches!(taker_followup, FollowupPlan::NoFillReleaseWal { .. }),
        "expected NoFillReleaseWal for SlowLegFirst"
    );

    Ok(FaultInjectionComparison {
        naive_topology: ExecutionTopology::ConcurrentProtectedIoc,
        router_maker_topology: maker_res.topology,
        router_dual_taker_topology: taker_res.topology,
        naive_hl_fill_base: naive_qty,
        naive_lighter_fill_base,
        naive_legged_out_pnl_usd,
        router_maker_legged_out_pnl_usd: Decimal::ZERO,
        router_dual_taker_legged_out_pnl_usd: Decimal::ZERO,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        order_budget::OrderBudgetConfig,
        strategy::{
            portfolio_risk::{CarryBudgetConfig, allocate_carry_budgets},
            tail_risk::AdlRiskLevel,
        },
        venues::VenueRegistry,
    };
    use std::str::FromStr;

    const NOW_MS: u64 = 1_795_200_000_000;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    fn book_with_levels(
        market: &str,
        bids: Vec<(&str, &str)>,
        asks: Vec<(&str, &str)>,
        available_notional: &str,
    ) -> Book {
        Book {
            market: market.to_string(),
            timestamp_ms: NOW_MS,
            tradable: true,
            bids: bids
                .into_iter()
                .map(|(p, c)| Level {
                    price: dec(p),
                    contracts: dec(c),
                })
                .collect(),
            asks: asks
                .into_iter()
                .map(|(p, c)| Level {
                    price: dec(p),
                    contracts: dec(c),
                })
                .collect(),
            bounds: None,
            available_notional: dec(available_notional),
        }
    }

    fn default_cfg() -> CrossVenueRouterConfig {
        CrossVenueRouterConfig {
            dual_taker_required: false,
            min_net_edge_bps: dec("5"),
            depth_fraction: dec("0.5"),
            max_entry_notional_usd: dec("1000"),
            ioc_slippage_bps: dec("5"),
            max_book_levels: 5,
            max_entry_book_age_ms: DEFAULT_MAX_ENTRY_BOOK_AGE_MS,
            max_followup_book_age_ms: DEFAULT_MAX_FOLLOWUP_BOOK_AGE_MS,
            cross_stablecoin_policy: None,
        }
    }

    fn default_budgets() -> [VenueOrderBudget; 2] {
        let cfg = OrderBudgetConfig::default();
        [
            VenueOrderBudget::new(cfg).unwrap(),
            VenueOrderBudget::new(cfg).unwrap(),
        ]
    }

    #[test]
    fn config_validation_rejects_invalid_bounds() {
        assert!(default_cfg().validate().is_ok());

        let mut cfg = default_cfg();
        cfg.max_entry_notional_usd = Decimal::ZERO;
        assert!(cfg.validate().is_err());

        cfg = default_cfg();
        cfg.min_net_edge_bps = dec("-0.1");
        assert!(cfg.validate().is_err());

        cfg = default_cfg();
        cfg.depth_fraction = Decimal::ZERO;
        assert!(cfg.validate().is_err());

        cfg = default_cfg();
        cfg.depth_fraction = dec("1.01");
        assert!(cfg.validate().is_err());

        cfg = default_cfg();
        cfg.ioc_slippage_bps = dec("10000");
        assert!(cfg.validate().is_err());

        cfg = default_cfg();
        cfg.max_book_levels = 0;
        assert!(cfg.validate().is_err());

        cfg = default_cfg();
        cfg.max_book_levels = 21;
        assert!(cfg.validate().is_err());

        cfg = default_cfg();
        cfg.max_entry_book_age_ms = 0;
        assert!(cfg.validate().is_err());

        cfg = default_cfg();
        cfg.max_followup_book_age_ms = 0;
        assert!(cfg.validate().is_err());

        // CrossVenueRouterConfig::default() fails closed until notional budget is configured.
        assert!(CrossVenueRouterConfig::default().validate().is_err());
    }

    #[test]
    fn asymmetric_speed_bump_routes_maker_first_and_slow_leg_first() {
        let registry = VenueRegistry::default();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let lighter = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();

        let hl_book = book_with_levels(
            "ETH-HL",
            vec![("100.18", "10")],
            vec![("100.20", "10")],
            "5000",
        );
        let lt_book = book_with_levels(
            "ETH-LT",
            vec![("99.98", "10")],
            vec![("100.00", "10")],
            "5000",
        );

        // dual_taker_required=false -> MakerFirst on Lighter (0ms post-only, 300ms taker)
        let mut router = CrossVenueExecutionRouter::new(default_cfg()).unwrap();
        let [mut b0, mut b1] = default_budgets();
        let plan = router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::SellLeg0BuyLeg1,
                legs: [&hl, &lighter],
                books: [&hl_book, &lt_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        );

        let RouterEntryPlan::MakerFirst {
            reservation,
            maker_order,
            planned_hedge_leg,
            ..
        } = plan
        else {
            panic!("expected MakerFirst, got {plan:?}");
        };
        assert_eq!(planned_hedge_leg, 0);
        assert_eq!(maker_order.leg_index, 1);
        assert_eq!(maker_order.kind, DispatchOrderKind::PostOnlyMaker);
        assert_eq!(
            maker_order.action_class,
            OrderIntentClass::RiskIncreasingEntry
        );
        assert!(maker_order.is_buy);
        assert!(maker_order.limit_price < lt_book.asks[0].price);
        assert!(reservation.is_active());
        assert_eq!(b0.used_weight(NOW_MS), 0);
        assert_eq!(b1.used_weight(NOW_MS), 1);

        // dual_taker_required=true -> SlowLegFirst on Lighter (300ms taker)
        let mut taker_router = CrossVenueExecutionRouter::new(CrossVenueRouterConfig {
            dual_taker_required: true,
            ..default_cfg()
        })
        .unwrap();
        let [mut tb0, mut tb1] = default_budgets();
        let taker_plan = taker_router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::SellLeg0BuyLeg1,
                legs: [&hl, &lighter],
                books: [&hl_book, &lt_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut tb0, &mut tb1],
        );

        let RouterEntryPlan::SlowLegFirst {
            first_ioc,
            pending_second_leg,
            ..
        } = taker_plan
        else {
            panic!("expected SlowLegFirst, got {taker_plan:?}");
        };
        assert_eq!(pending_second_leg, 0);
        assert_eq!(first_ioc.leg_index, 1);
        assert_eq!(first_ioc.kind, DispatchOrderKind::ProtectedIoc);
        assert_eq!(first_ioc.limit_price, dec("100.05"));
        assert_eq!(tb0.used_weight(NOW_MS), 0);
        assert_eq!(tb1.used_weight(NOW_MS), 1);
    }

    #[test]
    fn concurrent_dual_ioc_only_when_both_legs_fast_and_dual_slow_rejects() {
        let registry = VenueRegistry::default();
        let arcus = registry
            .resolve_execution_capability("arcus", "level_0", "equity")
            .unwrap();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let book0 = book_with_levels(
            "NVDA-ARC",
            vec![("100.00", "10")],
            vec![("100.02", "10")],
            "5000",
        );
        let book1 = book_with_levels(
            "NVDA-HL",
            vec![("100.22", "10")],
            vec![("100.25", "10")],
            "5000",
        );

        let mut router = CrossVenueExecutionRouter::new(CrossVenueRouterConfig {
            dual_taker_required: true,
            ..default_cfg()
        })
        .unwrap();
        let [mut b0, mut b1] = default_budgets();
        let plan = router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::BuyLeg0SellLeg1,
                legs: [&arcus, &hl],
                books: [&book0, &book1],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        );

        let RouterEntryPlan::ConcurrentDualIoc {
            reservation,
            leg0_ioc,
            leg1_ioc,
            ..
        } = plan
        else {
            panic!("expected ConcurrentDualIoc, got {plan:?}");
        };
        assert_eq!(leg0_ioc.kind, DispatchOrderKind::ProtectedIoc);
        assert_eq!(leg1_ioc.kind, DispatchOrderKind::ProtectedIoc);
        assert_eq!(b0.used_weight(NOW_MS), 1);
        assert_eq!(b1.used_weight(NOW_MS), 1);
        assert!(
            router
                .confirm_hedge_settled(reservation.reservation_id)
                .is_ok()
        );
        assert!(router.active_reservation().is_none());

        // Dual slow pair (two 300ms legs) fails closed with TopologyRejected.
        let lighter = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();
        let mut lighter_b = lighter.clone();
        lighter_b.venue_id = "lighter_b";
        lighter_b.liquidity_domain = "lighter_b";
        let rejected = router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::BuyLeg0SellLeg1,
                legs: [&lighter, &lighter_b],
                books: [&book0, &book1],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        );
        assert!(matches!(
            rejected,
            RouterEntryPlan::Blocked {
                reason: RouterBlockReason::TopologyRejected(_)
            }
        ));
    }

    #[test]
    fn wal_reservation_blocks_concurrent_entries_until_released_or_settled() {
        let registry = VenueRegistry::default();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let lighter = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();
        let hl_book = book_with_levels(
            "ETH-HL",
            vec![("100.18", "10")],
            vec![("100.20", "10")],
            "5000",
        );
        let lt_book = book_with_levels(
            "ETH-LT",
            vec![("99.98", "10")],
            vec![("100.00", "10")],
            "5000",
        );

        let mut router = CrossVenueExecutionRouter::new(default_cfg()).unwrap();
        let [mut b0, mut b1] = default_budgets();
        let params = RouteEntryParams {
            now_ms: NOW_MS,
            direction: RouteDirection::SellLeg0BuyLeg1,
            legs: [&hl, &lighter],
            books: [&hl_book, &lt_book],
            base_step: dec("0.1"),
            base_per_contract: [Decimal::ONE, Decimal::ONE],
            price_ticks: [dec("0.01"), dec("0.01")],
            portfolio_leg_cap_usd: None,
            tail_risk: RouterTailRiskInput::default(),
        };

        let RouterEntryPlan::MakerFirst { reservation, .. } =
            router.plan_entry(params, [&mut b0, &mut b1])
        else {
            panic!("expected MakerFirst");
        };

        // Second concurrent entry blocked while WAL reservation active, without burning budget.
        let blocked = router.plan_entry(params, [&mut b0, &mut b1]);
        assert_eq!(
            blocked,
            RouterEntryPlan::Blocked {
                reason: RouterBlockReason::WalReservationActive
            }
        );
        assert_eq!(b1.used_weight(NOW_MS), 1);

        // Wrong reservation ID fails closed and preserves reservation.
        assert_eq!(
            router.on_first_leg_terminal(
                NOW_MS + 10,
                reservation.reservation_id + 99,
                SlowLegTerminalOutcome::RejectedOrCancelledZeroFill,
                Some(&hl_book),
                &mut b0,
            ),
            FollowupPlan::RejectUnverifiedTerminalState
        );
        assert!(router.active_reservation().is_some());

        // Confirmed partial fill emits fast EmergencyHedge and KEEPS WAL reservation active.
        let followup = router.on_first_leg_terminal(
            NOW_MS + 20,
            reservation.reservation_id,
            SlowLegTerminalOutcome::Confirmed {
                filled_base: dec("2.0"),
                avg_fill_price: Some(dec("99.99")),
            },
            Some(&hl_book),
            &mut b0,
        );
        let FollowupPlan::DispatchFastHedgeIoc {
            reservation_id,
            hedge_order,
        } = followup
        else {
            panic!("expected DispatchFastHedgeIoc");
        };
        assert_eq!(reservation_id, reservation.reservation_id);
        assert_eq!(hedge_order.action_class, OrderIntentClass::EmergencyHedge);
        assert_eq!(hedge_order.size_base, dec("2.0"));
        assert!(!hedge_order.is_buy);
        assert!(!hedge_order.reduce_only);
        assert!(router.active_reservation().is_some());

        // Still blocked until hedge settlement is explicitly confirmed.
        assert_eq!(
            router.plan_entry(params, [&mut b0, &mut b1]),
            RouterEntryPlan::Blocked {
                reason: RouterBlockReason::WalReservationActive
            }
        );
        let settled = router
            .confirm_hedge_settled(reservation.reservation_id)
            .unwrap();
        assert!(!settled.is_active());
        assert!(router.active_reservation().is_none());

        // Subsequent zero-fill cycle releases WAL cleanly.
        let RouterEntryPlan::MakerFirst {
            reservation: res2, ..
        } = router.plan_entry(params, [&mut b0, &mut b1])
        else {
            panic!("expected second MakerFirst");
        };
        assert_eq!(
            router.on_first_leg_terminal(
                NOW_MS + 30,
                res2.reservation_id,
                SlowLegTerminalOutcome::Confirmed {
                    filled_base: Decimal::ZERO,
                    avg_fill_price: None,
                },
                Some(&hl_book),
                &mut b0,
            ),
            FollowupPlan::NoFillReleaseWal {
                reservation_id: res2.reservation_id
            }
        );
        assert!(router.active_reservation().is_none());
    }

    #[test]
    fn emergency_hedge_bypasses_429_cooldown_and_halt_while_new_entry_blocks() {
        let registry = VenueRegistry::default();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let lighter = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();
        let hl_book = book_with_levels(
            "ETH-HL",
            vec![("100.18", "10")],
            vec![("100.20", "10")],
            "5000",
        );
        let lt_book = book_with_levels(
            "ETH-LT",
            vec![("99.98", "10")],
            vec![("100.00", "10")],
            "5000",
        );

        let mut router = CrossVenueExecutionRouter::new(default_cfg()).unwrap();
        let budget_cfg = OrderBudgetConfig {
            cooldown_ms_on_rate_limit: 15_000,
            max_consecutive_rate_limits_before_halt: 2,
            ..OrderBudgetConfig::default()
        };
        let mut b0 = VenueOrderBudget::new(budget_cfg).unwrap();
        let mut b1 = VenueOrderBudget::new(budget_cfg).unwrap();

        // Trigger 429 cooldown on leg 1 (Lighter) -> NewEntry blocks with cooldown.
        assert!(!b1.record_rate_limit(NOW_MS));
        let params = RouteEntryParams {
            now_ms: NOW_MS + 100,
            direction: RouteDirection::SellLeg0BuyLeg1,
            legs: [&hl, &lighter],
            books: [&hl_book, &lt_book],
            base_step: dec("0.1"),
            base_per_contract: [Decimal::ONE, Decimal::ONE],
            price_ticks: [dec("0.01"), dec("0.01")],
            portfolio_leg_cap_usd: None,
            tail_risk: RouterTailRiskInput::default(),
        };
        let blocked = router.plan_entry(params, [&mut b0, &mut b1]);
        assert_eq!(
            blocked,
            RouterEntryPlan::Blocked {
                reason: RouterBlockReason::OrderBudgetBlocked {
                    leg_index: 1,
                    cooldown_remaining_ms: 14_900,
                }
            }
        );

        // After cooldown expires on leg 1, open MakerFirst entry with fresh books.
        let open_ms = NOW_MS + 16_000;
        let mut hl_open = hl_book.clone();
        hl_open.timestamp_ms = open_ms;
        let mut lt_open = lt_book.clone();
        lt_open.timestamp_ms = open_ms;
        let RouterEntryPlan::MakerFirst { reservation, .. } = router.plan_entry(
            RouteEntryParams {
                now_ms: open_ms,
                books: [&hl_open, &lt_open],
                ..params
            },
            [&mut b0, &mut b1],
        ) else {
            panic!("expected MakerFirst after cooldown");
        };

        // Now leg 0 (HL hedge venue) enters 429 cooldown AND reaches sticky halt!
        assert!(!b0.record_rate_limit(open_ms + 10));
        assert!(b0.record_rate_limit(open_ms + 20));
        assert!(b0.is_halted());

        // Confirmed fill on leg 1 MUST STILL dispatch EmergencyHedge on leg 0!
        let followup = router.on_first_leg_terminal(
            open_ms + 30,
            reservation.reservation_id,
            SlowLegTerminalOutcome::Confirmed {
                filled_base: dec("1.5"),
                avg_fill_price: Some(dec("99.99")),
            },
            Some(&hl_open),
            &mut b0,
        );
        assert!(matches!(
            followup,
            FollowupPlan::DispatchFastHedgeIoc {
                hedge_order: RoutedLegOrder {
                    action_class: OrderIntentClass::EmergencyHedge,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn portfolio_budget_cap_and_zero_cap_fail_closed() {
        let registry = VenueRegistry::default();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let lighter = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();
        let hl_book = book_with_levels(
            "ETH-HL",
            vec![("100.18", "50")],
            vec![("100.20", "50")],
            "50000",
        );
        let lt_book = book_with_levels(
            "ETH-LT",
            vec![("99.98", "50")],
            vec![("100.00", "50")],
            "50000",
        );

        let mut router = CrossVenueExecutionRouter::new(CrossVenueRouterConfig {
            max_entry_notional_usd: dec("2000"),
            depth_fraction: Decimal::ONE,
            ..default_cfg()
        })
        .unwrap();
        let [mut b0, mut b1] = default_budgets();

        // Zero cap blocks immediately.
        let zero_plan = router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::SellLeg0BuyLeg1,
                legs: [&hl, &lighter],
                books: [&hl_book, &lt_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: Some(Decimal::ZERO),
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        );
        assert_eq!(
            zero_plan,
            RouterEntryPlan::Blocked {
                reason: RouterBlockReason::ZeroPortfolioBudgetCap
            }
        );

        // Compute allocation from CRASH-IMP-03 allocate_carry_budgets and bridge to pair cap.
        let alloc = allocate_carry_budgets(
            &[0.02, 0.02],
            &[vec![1.0, 0.2], vec![0.2, 1.0]],
            &[-0.01, 0.01, 0.015, 0.005, -0.005],
            0.005,
            0.0001,
            &CarryBudgetConfig {
                max_gross_notional_usd: 400.0,
                max_single_leg_weight: 0.6,
                max_es_loss_usd: 10.0,
                brake_scale: 1.0,
                ..CarryBudgetConfig::default()
            },
        )
        .unwrap();
        let cap_usd = portfolio_pair_cap_usd(&alloc, [0, 1]).unwrap();
        assert_eq!(cap_usd, dec("200"));

        let capped_plan = router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::SellLeg0BuyLeg1,
                legs: [&hl, &lighter],
                books: [&hl_book, &lt_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: Some(cap_usd),
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        );
        let RouterEntryPlan::MakerFirst { reservation, .. } = capped_plan else {
            panic!("expected capped MakerFirst, got {capped_plan:?}");
        };
        assert!(reservation.reserved_notional_usd <= cap_usd);
    }

    #[test]
    fn cross_stablecoin_policy_and_tail_risk_escalation_block_entries() {
        let registry = VenueRegistry::default();
        let rh = registry
            .resolve_execution_capability("lighter_robinhood", "standard", "equity")
            .unwrap();
        let arcus = registry
            .resolve_execution_capability("arcus", "level_0", "equity")
            .unwrap();

        let rh_book = book_with_levels(
            "NVDA-RH",
            vec![("100.00", "10")],
            vec![("100.02", "10")],
            "5000",
        );
        let arc_book = book_with_levels(
            "NVDA-ARC",
            vec![("100.22", "10")],
            vec![("100.25", "10")],
            "5000",
        );

        // Without policy, USDC vs USDG fails closed.
        let mut no_policy_router = CrossVenueExecutionRouter::new(default_cfg()).unwrap();
        let [mut b0, mut b1] = default_budgets();
        let no_policy_plan = no_policy_router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::BuyLeg0SellLeg1,
                legs: [&rh, &arcus],
                books: [&rh_book, &arc_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        );
        assert!(matches!(
            no_policy_plan,
            RouterEntryPlan::Blocked {
                reason: RouterBlockReason::TopologyRejected(_)
            }
        ));

        let policy = CrossStablecoinHedgePolicy {
            allowed_pairs: vec![("USDC".into(), "USDG".into())],
            tail_risk: TailRiskConfig::default(),
            min_fresh_peg_sources_per_asset: 2,
        };
        let mut policy_router = CrossVenueExecutionRouter::new(CrossVenueRouterConfig {
            cross_stablecoin_policy: Some(policy),
            ..default_cfg()
        })
        .unwrap();

        let venues = [
            VenueMarginSnapshot {
                venue: "lighter_robinhood".into(),
                collateral_asset: "USDG".into(),
                equity_usd: 1_000.0,
                margin_used_usd: 200.0,
                liquidation_distance_bps: Some(1_000.0),
                adl_level: AdlRiskLevel::Low,
            },
            VenueMarginSnapshot {
                venue: "arcus".into(),
                collateral_asset: "USDC".into(),
                equity_usd: 1_000.0,
                margin_used_usd: 200.0,
                liquidation_distance_bps: Some(1_000.0),
                adl_level: AdlRiskLevel::Low,
            },
        ];
        let peg = |asset: &str, price_usd: f64| StablecoinPegSample {
            asset: asset.into(),
            price_usd,
            available_at_ms: NOW_MS - 500,
        };

        // 35bp depeg triggers TailRiskBlocked(ReduceOnly).
        let mild_depeg = [
            peg("USDC", 1.0),
            peg("USDC", 1.0),
            peg("USDG", 0.9965),
            peg("USDG", 1.0),
        ];
        let ro_plan = policy_router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::BuyLeg0SellLeg1,
                legs: [&rh, &arcus],
                books: [&rh_book, &arc_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::from_snapshots(&venues, &mild_depeg),
            },
            [&mut b0, &mut b1],
        );
        assert_eq!(
            ro_plan,
            RouterEntryPlan::Blocked {
                reason: RouterBlockReason::TailRiskBlocked(TailRiskAction::ReduceOnly)
            }
        );

        // Healthy dual-source pegs + HaltAndFlatten override still fail closed.
        let healthy = [
            peg("USDC", 1.0),
            peg("USDC", 1.0),
            peg("USDG", 1.0),
            peg("USDG", 1.0),
        ];
        let halt_plan = policy_router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::BuyLeg0SellLeg1,
                legs: [&rh, &arcus],
                books: [&rh_book, &arc_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput {
                    action_override: Some(TailRiskAction::HaltAndFlatten),
                    ..RouterTailRiskInput::from_snapshots(&venues, &healthy)
                },
            },
            [&mut b0, &mut b1],
        );
        assert_eq!(
            halt_plan,
            RouterEntryPlan::Blocked {
                reason: RouterBlockReason::TailRiskBlocked(TailRiskAction::HaltAndFlatten)
            }
        );

        // Healthy dual-source quorum routes MakerFirst on Lighter Robinhood.
        let ok_plan = policy_router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::BuyLeg0SellLeg1,
                legs: [&rh, &arcus],
                books: [&rh_book, &arc_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::from_snapshots(&venues, &healthy),
            },
            [&mut b0, &mut b1],
        );
        assert!(matches!(ok_plan, RouterEntryPlan::MakerFirst { .. }));
    }

    #[test]
    fn marginal_depth_truncates_levels_and_switches_fee_regime_by_topology() {
        let registry = VenueRegistry::default();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let mut lighter_plus = registry
            .resolve_execution_capability("lighter", "premium", "crypto")
            .unwrap();
        // lighter premium: maker=0.4 bps, taker=2.8 bps, taker_delay=140ms
        // Gross spread = (100.12 - 100.05) / 100.05 = 6.996 bps.
        // Under MakerFirst (0.4 maker + 4.5 taker = 4.9 bps): net > 1 bps -> ADMITTED when min_net=1!
        // Under SlowLegFirst (2.8 taker + 4.5 taker = 7.3 bps): net < 0 bps -> BLOCKED!
        lighter_plus.taker_delay_ms = 140;
        let hl_book = book_with_levels(
            "ETH-HL",
            vec![("100.12", "5"), ("100.11", "5")],
            vec![("100.15", "5")],
            "5000",
        );
        let lt_book = book_with_levels(
            "ETH-LT",
            vec![("100.00", "5")],
            vec![("100.05", "1"), ("100.06", "5")],
            "5000",
        );

        let maker_cfg = CrossVenueRouterConfig {
            dual_taker_required: false,
            min_net_edge_bps: dec("1"),
            depth_fraction: Decimal::ONE,
            max_entry_notional_usd: dec("1000"),
            ioc_slippage_bps: dec("2"),
            max_book_levels: 1, // truncates level 2 ("100.06")
            max_entry_book_age_ms: DEFAULT_MAX_ENTRY_BOOK_AGE_MS,
            max_followup_book_age_ms: DEFAULT_MAX_FOLLOWUP_BOOK_AGE_MS,
            cross_stablecoin_policy: None,
        };
        let mut maker_router = CrossVenueExecutionRouter::new(maker_cfg.clone()).unwrap();
        let [mut b0, mut b1] = default_budgets();
        let maker_plan = maker_router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::SellLeg0BuyLeg1,
                legs: [&hl, &lighter_plus],
                books: [&hl_book, &lt_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        );
        let RouterEntryPlan::MakerFirst { maker_order, .. } = maker_plan else {
            panic!("expected MakerFirst, got {maker_plan:?}");
        };
        // Truncated to 1 book level -> only 1.0 base available at 100.05!
        assert_eq!(maker_order.size_base, dec("1.0"));

        let mut taker_router = CrossVenueExecutionRouter::new(CrossVenueRouterConfig {
            dual_taker_required: true,
            ..maker_cfg
        })
        .unwrap();
        let taker_plan = taker_router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::SellLeg0BuyLeg1,
                legs: [&hl, &lighter_plus],
                books: [&hl_book, &lt_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        );
        assert_eq!(
            taker_plan,
            RouterEntryPlan::Blocked {
                reason: RouterBlockReason::InsufficientCrossableDepthOrEdge
            }
        );
    }

    #[test]
    fn deterministic_fault_injection_hl_0ms_vs_lighter_300ms_25bp_jump() {
        let cmp = simulate_midflight_jump_comparison().unwrap();
        assert_eq!(
            cmp.naive_topology,
            ExecutionTopology::ConcurrentProtectedIoc
        );
        assert_eq!(
            cmp.router_maker_topology,
            ExecutionTopology::MakerFirst {
                maker_leg: 1,
                hedge_leg: 0
            }
        );
        assert_eq!(
            cmp.router_dual_taker_topology,
            ExecutionTopology::SlowLegFirst {
                first_leg: 1,
                second_leg: 0
            }
        );
        assert_eq!(cmp.naive_hl_fill_base, dec("5"));
        assert_eq!(cmp.naive_lighter_fill_base, Decimal::ZERO);
        assert!(cmp.naive_legged_out_pnl_usd < Decimal::ZERO);
        assert_eq!(cmp.router_maker_legged_out_pnl_usd, Decimal::ZERO);
        assert_eq!(cmp.router_dual_taker_legged_out_pnl_usd, Decimal::ZERO);
    }

    #[test]
    fn maker_first_post_only_price_never_crosses_on_buy_or_sell() {
        let registry = VenueRegistry::default();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let lighter = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();
        let [mut b0, mut b1] = default_budgets();

        // Buy on Lighter (wide book 99.80 / 100.00, tick 0.05) -> post-only limit 99.95 < 100.00.
        let hl_book = book_with_levels(
            "ETH-HL",
            vec![("100.20", "10")],
            vec![("100.25", "10")],
            "5000",
        );
        let lt_book = book_with_levels(
            "ETH-LT",
            vec![("99.80", "10")],
            vec![("100.00", "10")],
            "5000",
        );
        let mut r_buy = CrossVenueExecutionRouter::new(default_cfg()).unwrap();
        let RouterEntryPlan::MakerFirst {
            maker_order: buy_ord,
            ..
        } = r_buy.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::SellLeg0BuyLeg1,
                legs: [&hl, &lighter],
                books: [&hl_book, &lt_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.05"), dec("0.05")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        )
        else {
            panic!("expected MakerFirst buy");
        };
        assert_eq!(buy_ord.limit_price, dec("99.95"));
        assert!(buy_ord.limit_price < lt_book.asks[0].price);

        // Sell on Lighter (book 100.30 / 100.50, tick 0.05) -> post-only limit >= 100.35 > 100.30.
        let hl_book2 = book_with_levels(
            "ETH-HL",
            vec![("99.90", "10")],
            vec![("100.00", "10")],
            "5000",
        );
        let lt_book2 = book_with_levels(
            "ETH-LT",
            vec![("100.30", "10")],
            vec![("100.50", "10")],
            "5000",
        );
        let mut r_sell = CrossVenueExecutionRouter::new(default_cfg()).unwrap();
        let RouterEntryPlan::MakerFirst {
            maker_order: sell_ord,
            ..
        } = r_sell.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS + 100,
                direction: RouteDirection::BuyLeg0SellLeg1,
                legs: [&hl, &lighter],
                books: [&hl_book2, &lt_book2],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.05"), dec("0.05")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        )
        else {
            panic!("expected MakerFirst sell");
        };
        assert_eq!(sell_ord.limit_price, dec("100.35"));
        assert!(sell_ord.limit_price > lt_book2.bids[0].price);
    }

    #[test]
    fn followup_rejects_concurrent_topology_and_overfill() {
        let registry = VenueRegistry::default();
        let arcus = registry
            .resolve_execution_capability("arcus", "level_0", "equity")
            .unwrap();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let book0 = book_with_levels(
            "NVDA-ARC",
            vec![("100.00", "10")],
            vec![("100.02", "10")],
            "5000",
        );
        let book1 = book_with_levels(
            "NVDA-HL",
            vec![("100.22", "10")],
            vec![("100.25", "10")],
            "5000",
        );

        let mut router = CrossVenueExecutionRouter::new(CrossVenueRouterConfig {
            dual_taker_required: true,
            ..default_cfg()
        })
        .unwrap();
        let [mut b0, mut b1] = default_budgets();
        let RouterEntryPlan::ConcurrentDualIoc { reservation, .. } = router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::BuyLeg0SellLeg1,
                legs: [&arcus, &hl],
                books: [&book0, &book1],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        ) else {
            panic!("expected ConcurrentDualIoc");
        };
        assert_eq!(
            router.on_first_leg_terminal(
                NOW_MS + 10,
                reservation.reservation_id,
                SlowLegTerminalOutcome::Confirmed {
                    filled_base: dec("1"),
                    avg_fill_price: None,
                },
                Some(&book1),
                &mut b1,
            ),
            FollowupPlan::RejectUnverifiedTerminalState
        );
        assert!(
            router
                .confirm_hedge_settled(reservation.reservation_id)
                .is_ok()
        );

        // Overfill on MakerFirst is also rejected fail-closed.
        let lighter = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();
        let mut maker_router = CrossVenueExecutionRouter::new(default_cfg()).unwrap();
        let RouterEntryPlan::MakerFirst {
            reservation: res2, ..
        } = maker_router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS + 20,
                direction: RouteDirection::BuyLeg0SellLeg1,
                legs: [&hl, &lighter],
                books: [&book0, &book1],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        )
        else {
            panic!("expected MakerFirst");
        };
        assert_eq!(
            maker_router.on_first_leg_terminal(
                NOW_MS + 30,
                res2.reservation_id,
                SlowLegTerminalOutcome::Confirmed {
                    filled_base: res2.reserved_base_qty + dec("1"),
                    avg_fill_price: None,
                },
                Some(&book0),
                &mut b0,
            ),
            FollowupPlan::RejectUnverifiedTerminalState
        );
    }

    #[test]
    fn negative_maker_rebate_and_non_unit_base_per_contract_supported() {
        let registry = VenueRegistry::default();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let zero_maker = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();
        let mut rebate_maker = zero_maker.clone();
        rebate_maker.maker_fee_bps = dec("-0.6");

        // 4 contracts * 0.5 = 2.0 base on HL; 2 contracts * 2.0 = 4.0 base on Lighter.
        // Gross spread is ~4.0 bps; 0 bps maker vs 4.5 bps HL taker is negative edge,
        // while -0.6 bps maker rebate vs 4.5 bps taker yields net positive edge.
        let hl_book = book_with_levels(
            "BTC-HL",
            vec![("100.10", "4")],
            vec![("100.20", "4")],
            "5000",
        );
        let lt_book = book_with_levels(
            "BTC-LT",
            vec![("99.95", "2")],
            vec![("100.06", "2")],
            "5000",
        );

        let mut router = CrossVenueExecutionRouter::new(CrossVenueRouterConfig {
            min_net_edge_bps: dec("0"),
            depth_fraction: Decimal::ONE,
            ..default_cfg()
        })
        .unwrap();
        let [mut b0, mut b1] = default_budgets();

        let blocked = router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::SellLeg0BuyLeg1,
                legs: [&hl, &zero_maker],
                books: [&hl_book, &lt_book],
                base_step: dec("0.5"),
                base_per_contract: [dec("0.5"), dec("2")],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        );
        assert_eq!(
            blocked,
            RouterEntryPlan::Blocked {
                reason: RouterBlockReason::InsufficientCrossableDepthOrEdge
            }
        );

        let plan = router.plan_entry(
            RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::SellLeg0BuyLeg1,
                legs: [&hl, &rebate_maker],
                books: [&hl_book, &lt_book],
                base_step: dec("0.5"),
                base_per_contract: [dec("0.5"), dec("2")],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            },
            [&mut b0, &mut b1],
        );
        let RouterEntryPlan::MakerFirst { maker_order, .. } = plan else {
            panic!("expected MakerFirst with rebate, got {plan:?}");
        };
        assert_eq!(maker_order.size_base, dec("2.0"));
    }

    #[test]
    fn sequential_hedge_intent_is_unique_and_preserves_reservation() {
        let registry = VenueRegistry::default();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let lighter = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();
        let hl_book = book_with_levels(
            "ETH-HL",
            vec![("100.18", "10")],
            vec![("100.20", "10")],
            "5000",
        );
        let lt_book = book_with_levels(
            "ETH-LT",
            vec![("99.98", "10")],
            vec![("100.00", "10")],
            "5000",
        );

        for dual_taker_required in [false, true] {
            let [mut b0, mut b1] = default_budgets();
            let mut router = CrossVenueExecutionRouter::new(CrossVenueRouterConfig {
                dual_taker_required,
                ..default_cfg()
            })
            .unwrap();
            let params = RouteEntryParams {
                now_ms: NOW_MS,
                direction: RouteDirection::SellLeg0BuyLeg1,
                legs: [&hl, &lighter],
                books: [&hl_book, &lt_book],
                base_step: dec("0.1"),
                base_per_contract: [Decimal::ONE, Decimal::ONE],
                price_ticks: [dec("0.01"), dec("0.01")],
                portfolio_leg_cap_usd: None,
                tail_risk: RouterTailRiskInput::default(),
            };

            let reservation = match router.plan_entry(params, [&mut b0, &mut b1]) {
                RouterEntryPlan::MakerFirst { reservation, .. } if !dual_taker_required => {
                    reservation
                }
                RouterEntryPlan::SlowLegFirst { reservation, .. } if dual_taker_required => {
                    reservation
                }
                result => panic!("unexpected sequential entry plan: {result:?}"),
            };
            assert_eq!(reservation.hedge_leg_index, 0);
            let res_id = reservation.reservation_id;

            // Cannot settle sequential WAL before hedge intent is emitted.
            assert!(router.confirm_hedge_settled(res_id).is_err());
            assert_eq!(
                router
                    .active_reservation()
                    .map(|active| active.reservation_id),
                Some(res_id)
            );

            let weight_before = b0.used_weight(NOW_MS + 10);
            let terminal = SlowLegTerminalOutcome::Confirmed {
                filled_base: dec("2.0"),
                avg_fill_price: Some(dec("100")),
            };
            let first_plan = router.on_first_leg_terminal(
                NOW_MS + 10,
                res_id,
                terminal.clone(),
                Some(&hl_book),
                &mut b0,
            );
            assert!(matches!(
                first_plan,
                FollowupPlan::DispatchFastHedgeIoc {
                    reservation_id,
                    ref hedge_order,
                } if reservation_id == res_id && hedge_order.size_base == dec("2.0")
            ));
            let weight_after_first = b0.used_weight(NOW_MS + 10);
            assert_eq!(weight_after_first - weight_before, 1);
            assert!(router.active_reservation().is_some());

            // Duplicate confirmation fails closed without charging budget or releasing WAL.
            let duplicate_plan = router.on_first_leg_terminal(
                NOW_MS + 11,
                res_id,
                terminal,
                Some(&hl_book),
                &mut b0,
            );
            assert_eq!(duplicate_plan, FollowupPlan::RejectUnverifiedTerminalState);
            assert_eq!(b0.used_weight(NOW_MS + 11), weight_after_first);
            assert!(router.active_reservation().is_some());

            // Contradictory zero-fill terminal outcomes also fail closed and keep WAL active.
            let contradictory_cancel = router.on_first_leg_terminal(
                NOW_MS + 12,
                res_id,
                SlowLegTerminalOutcome::RejectedOrCancelledZeroFill,
                Some(&hl_book),
                &mut b0,
            );
            assert_eq!(
                contradictory_cancel,
                FollowupPlan::RejectUnverifiedTerminalState
            );
            assert_eq!(b0.used_weight(NOW_MS + 12), weight_after_first);
            assert!(router.active_reservation().is_some());

            let contradictory_zero_confirm = router.on_first_leg_terminal(
                NOW_MS + 13,
                res_id,
                SlowLegTerminalOutcome::Confirmed {
                    filled_base: Decimal::ZERO,
                    avg_fill_price: None,
                },
                Some(&hl_book),
                &mut b0,
            );
            assert_eq!(
                contradictory_zero_confirm,
                FollowupPlan::RejectUnverifiedTerminalState
            );
            assert_eq!(b0.used_weight(NOW_MS + 13), weight_after_first);
            assert!(router.active_reservation().is_some());

            // Authoritative hedge settlement succeeds cleanly and clears active_reservation().
            let settled = router.confirm_hedge_settled(res_id).unwrap();
            assert!(!settled.is_active());
            assert_eq!(settled.reservation_id, res_id);
            assert!(router.active_reservation().is_none());
        }
    }

    #[test]
    fn peer_first_positive_fill_preserves_wal_until_valid_same_quantity_retry() {
        let registry = VenueRegistry::default();
        let first = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let second = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();
        let book0 = book_with_levels(
            "SYNTH-0",
            vec![("100.18", "10")],
            vec![("100.20", "10")],
            "5000",
        );
        let book1 = book_with_levels(
            "SYNTH-1",
            vec![("99.98", "10")],
            vec![("100.00", "10")],
            "5000",
        );
        let invalid = book_with_levels(
            "SYNTH-0",
            vec![("100.18", "0")],
            vec![("100.20", "0.1")],
            "10",
        );

        for dual_taker_required in [false, true] {
            for zero_confirmed in [false, true] {
                let [mut b0, mut b1] = default_budgets();
                let mut router = CrossVenueExecutionRouter::new(CrossVenueRouterConfig {
                    dual_taker_required,
                    ..default_cfg()
                })
                .unwrap();
                let params = RouteEntryParams {
                    now_ms: NOW_MS,
                    direction: RouteDirection::SellLeg0BuyLeg1,
                    legs: [&first, &second],
                    books: [&book0, &book1],
                    base_step: dec("0.1"),
                    base_per_contract: [Decimal::ONE, Decimal::ONE],
                    price_ticks: [dec("0.01"), dec("0.01")],
                    portfolio_leg_cap_usd: None,
                    tail_risk: RouterTailRiskInput::default(),
                };
                let reservation = match router.plan_entry(params, [&mut b0, &mut b1]) {
                    RouterEntryPlan::MakerFirst { reservation, .. } if !dual_taker_required => {
                        reservation
                    }
                    RouterEntryPlan::SlowLegFirst { reservation, .. } if dual_taker_required => {
                        reservation
                    }
                    result => panic!("expected a sequential test plan: {result:?}"),
                };
                let id = reservation.reservation_id;
                let positive = SlowLegTerminalOutcome::Confirmed {
                    filled_base: dec("2.0"),
                    avg_fill_price: Some(dec("100")),
                };
                assert!(reservation.reserved_base_qty >= dec("2.0"));
                let before = b0.used_weight(NOW_MS + 10);
                assert_eq!(
                    router.on_first_leg_terminal(
                        NOW_MS + 10,
                        id,
                        positive.clone(),
                        Some(&invalid),
                        &mut b0,
                    ),
                    FollowupPlan::RejectUnverifiedTerminalState
                );
                assert_eq!(b0.used_weight(NOW_MS + 10), before);
                assert!(router.active_reservation().is_some());
                assert!(router.confirm_hedge_settled(id).is_err());

                for conflicting in [
                    SlowLegTerminalOutcome::RejectedOrCancelledZeroFill,
                    SlowLegTerminalOutcome::Confirmed {
                        filled_base: Decimal::ZERO,
                        avg_fill_price: None,
                    },
                    SlowLegTerminalOutcome::Confirmed {
                        filled_base: dec("1.5"),
                        avg_fill_price: Some(dec("100")),
                    },
                    SlowLegTerminalOutcome::Confirmed {
                        filled_base: dec("2.1"),
                        avg_fill_price: Some(dec("100")),
                    },
                ] {
                    assert_eq!(
                        router.on_first_leg_terminal(
                            NOW_MS + 11,
                            id,
                            conflicting,
                            Some(&book0),
                            &mut b0
                        ),
                        FollowupPlan::RejectUnverifiedTerminalState,
                    );
                    assert_eq!(router.active_reservation().unwrap().reservation_id, id);
                    assert_eq!(b0.used_weight(NOW_MS + 11), before);
                }

                assert!(matches!(
                    router.on_first_leg_terminal(NOW_MS + 12, id, positive.clone(), Some(&book0), &mut b0),
                    FollowupPlan::DispatchFastHedgeIoc { hedge_order, .. } if hedge_order.size_base == dec("2.0")
                ));
                assert_eq!(router.active_reservation().unwrap().reservation_id, id);
                assert_eq!(b0.used_weight(NOW_MS + 12), before + 1);
                assert_eq!(
                    router.on_first_leg_terminal(NOW_MS + 13, id, positive, Some(&book0), &mut b0),
                    FollowupPlan::RejectUnverifiedTerminalState,
                );
                assert_eq!(b0.used_weight(NOW_MS + 13), before + 1);
                assert!(router.confirm_hedge_settled(id + 99).is_err());
                assert!(router.active_reservation().is_some());
                assert!(!router.confirm_hedge_settled(id).unwrap().is_active());

                let clean_id = match router.plan_entry(params, [&mut b0, &mut b1]) {
                    RouterEntryPlan::MakerFirst { reservation, .. }
                    | RouterEntryPlan::SlowLegFirst { reservation, .. } => {
                        reservation.reservation_id
                    }
                    other => panic!("expected clean next route: {other:?}"),
                };
                let before_clean = b0.used_weight(NOW_MS + 14);
                let clean = if zero_confirmed {
                    SlowLegTerminalOutcome::Confirmed {
                        filled_base: Decimal::ZERO,
                        avg_fill_price: None,
                    }
                } else {
                    SlowLegTerminalOutcome::RejectedOrCancelledZeroFill
                };
                assert_eq!(
                    router.on_first_leg_terminal(
                        NOW_MS + 14,
                        clean_id,
                        clean,
                        Some(&book0),
                        &mut b0
                    ),
                    FollowupPlan::NoFillReleaseWal {
                        reservation_id: clean_id
                    },
                );
                assert!(router.active_reservation().is_none());
                assert_eq!(b0.used_weight(NOW_MS + 14), before_clean);
            }
        }
    }

    #[test]
    fn followup_rejects_wrong_market_future_regressed_and_stale_books_while_preserving_wal_and_planned_fallback()
     {
        let registry = VenueRegistry::default();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let lighter = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();

        let cfg = CrossVenueRouterConfig {
            max_followup_book_age_ms: 500,
            ..default_cfg()
        };
        let mut router = CrossVenueExecutionRouter::new(cfg).unwrap();
        let [mut b0, mut b1] = default_budgets();

        let planned_ts = NOW_MS + 10;
        let mut hl_book = book_with_levels(
            "ETH-HL",
            vec![("100.18", "10")],
            vec![("100.20", "10")],
            "5000",
        );
        hl_book.timestamp_ms = planned_ts;
        let mut lt_book = book_with_levels(
            "ETH-LT",
            vec![("99.98", "10")],
            vec![("100.00", "10")],
            "5000",
        );
        lt_book.timestamp_ms = planned_ts;

        let params = RouteEntryParams {
            now_ms: planned_ts,
            direction: RouteDirection::SellLeg0BuyLeg1,
            legs: [&hl, &lighter],
            books: [&hl_book, &lt_book],
            base_step: dec("0.1"),
            base_per_contract: [Decimal::ONE, Decimal::ONE],
            price_ticks: [dec("0.01"), dec("0.01")],
            portfolio_leg_cap_usd: None,
            tail_risk: RouterTailRiskInput::default(),
        };

        let (res_id, shifted_hl) = match router.plan_entry(params, [&mut b0, &mut b1]) {
            RouterEntryPlan::MakerFirst { reservation, .. } => {
                let mut moved_hl = hl_book.clone();
                moved_hl.bids = vec![Level {
                    price: dec("99.00"),
                    contracts: dec("10"),
                }];
                (reservation.reservation_id, moved_hl)
            }
            other => panic!("expected MakerFirst: {other:?}"),
        };

        let positive = SlowLegTerminalOutcome::Confirmed {
            filled_base: dec("2.0"),
            avg_fill_price: Some(dec("100")),
        };
        let weight_before = b0.used_weight(planned_ts);

        // Call before reservation creation rejects fail-closed.
        assert_eq!(
            router.on_first_leg_terminal(
                planned_ts - 1,
                res_id,
                positive.clone(),
                Some(&hl_book),
                &mut b0,
            ),
            FollowupPlan::RejectUnverifiedTerminalState
        );

        let wrong_market = Book {
            market: "BTC-HL".to_string(),
            timestamp_ms: NOW_MS + 600,
            ..shifted_hl.clone()
        };
        let zero_ts = Book {
            timestamp_ms: 0,
            ..shifted_hl.clone()
        };
        let regressed = Book {
            timestamp_ms: planned_ts - 1,
            ..shifted_hl.clone()
        };
        let future = Book {
            timestamp_ms: NOW_MS + 601,
            ..shifted_hl.clone()
        };
        let stale = Book {
            timestamp_ms: NOW_MS + 99,
            ..shifted_hl.clone()
        };

        for bad_book in [&wrong_market, &zero_ts, &regressed, &future, &stale] {
            assert_eq!(
                router.on_first_leg_terminal(
                    NOW_MS + 600,
                    res_id,
                    positive.clone(),
                    Some(bad_book),
                    &mut b0,
                ),
                FollowupPlan::RejectUnverifiedTerminalState
            );
            assert_eq!(b0.used_weight(NOW_MS + 600), weight_before);
            assert!(router.active_reservation().is_some());
            assert!(router.confirm_hedge_settled(res_id).is_err());
        }

        // Exact boundary age (now_ms - timestamp_ms == max_followup_book_age_ms == 500) succeeds and reprices.
        let boundary_fresh = Book {
            timestamp_ms: NOW_MS + 100,
            ..shifted_hl
        };
        let FollowupPlan::DispatchFastHedgeIoc { hedge_order, .. } = router.on_first_leg_terminal(
            NOW_MS + 600,
            res_id,
            positive.clone(),
            Some(&boundary_fresh),
            &mut b0,
        ) else {
            panic!("expected DispatchFastHedgeIoc at exact boundary age");
        };
        assert_eq!(hedge_order.size_base, dec("2.0"));
        assert!(!hedge_order.is_buy);
        assert!(hedge_order.limit_price < hl_book.bids[0].price);
        assert_eq!(b0.used_weight(NOW_MS + 600), weight_before + 1);
        assert!(router.active_reservation().is_some());
        assert!(!router.confirm_hedge_settled(res_id).unwrap().is_active());

        // Second cycle: reject wrong market first, then fallback to planned limit with None while preserving WAL.
        let RouterEntryPlan::MakerFirst {
            reservation: r2, ..
        } = router.plan_entry(params, [&mut b0, &mut b1])
        else {
            panic!("expected second MakerFirst");
        };
        let planned_expected =
            apply_ioc_protection(false, hl_book.bids[0].price, dec("5"), dec("0.01")).unwrap();
        let w2_before = b0.used_weight(planned_ts + 20);
        assert_eq!(
            router.on_first_leg_terminal(
                planned_ts + 20,
                r2.reservation_id,
                positive.clone(),
                Some(&wrong_market),
                &mut b0,
            ),
            FollowupPlan::RejectUnverifiedTerminalState
        );
        assert_eq!(b0.used_weight(planned_ts + 20), w2_before);
        let FollowupPlan::DispatchFastHedgeIoc {
            hedge_order: fallback_order,
            ..
        } = router.on_first_leg_terminal(
            planned_ts + 25,
            r2.reservation_id,
            positive,
            None,
            &mut b0,
        )
        else {
            panic!("expected planned limit fallback on None");
        };
        assert_eq!(fallback_order.limit_price, planned_expected);
        assert_eq!(b0.used_weight(planned_ts + 25), w2_before + 1);
        assert!(router.active_reservation().is_some());
        assert!(
            !router
                .confirm_hedge_settled(r2.reservation_id)
                .unwrap()
                .is_active()
        );
    }

    #[test]
    fn entry_rejects_future_stale_and_zero_clock_books_before_wal_or_budget_use() {
        let registry = VenueRegistry::default();
        let hl = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();
        let lighter = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();

        let cfg = CrossVenueRouterConfig {
            max_entry_book_age_ms: 400,
            max_followup_book_age_ms: 500,
            ..default_cfg()
        };
        let mut router = CrossVenueExecutionRouter::new(cfg).unwrap();
        let [mut b0, mut b1] = default_budgets();

        let hl_book = book_with_levels(
            "ETH-HL",
            vec![("100.18", "10")],
            vec![("100.20", "10")],
            "5000",
        );
        let lt_book = book_with_levels(
            "ETH-LT",
            vec![("99.98", "10")],
            vec![("100.00", "10")],
            "5000",
        );

        let base_params = RouteEntryParams {
            now_ms: NOW_MS,
            direction: RouteDirection::SellLeg0BuyLeg1,
            legs: [&hl, &lighter],
            books: [&hl_book, &lt_book],
            base_step: dec("0.1"),
            base_per_contract: [Decimal::ONE, Decimal::ONE],
            price_ticks: [dec("0.01"), dec("0.01")],
            portfolio_leg_cap_usd: None,
            tail_risk: RouterTailRiskInput::default(),
        };

        // 1) Zero entry decision clock rejects fail-closed before touching WAL or budgets.
        let zero_plan = router.plan_entry(
            RouteEntryParams {
                now_ms: 0,
                ..base_params
            },
            [&mut b0, &mut b1],
        );
        assert!(matches!(
            zero_plan,
            RouterEntryPlan::Blocked {
                reason: RouterBlockReason::InvalidConfiguration(_)
            }
        ));
        assert!(router.active_reservation().is_none());
        assert_eq!(b0.used_weight(NOW_MS), 0);
        assert_eq!(b1.used_weight(NOW_MS), 0);

        // 2) Future entry books (leg 0 +1ms, or leg 1 #665 case +500ms) and stale entry books block cleanly.
        let future_leg0 = Book {
            timestamp_ms: NOW_MS + 1,
            ..hl_book.clone()
        };
        let future_leg1 = Book {
            timestamp_ms: NOW_MS + 500,
            ..lt_book.clone()
        };
        let stale_leg0 = Book {
            timestamp_ms: NOW_MS - 401,
            ..hl_book.clone()
        };
        for pair in [
            [&future_leg0, &lt_book],
            [&hl_book, &future_leg1],
            [&stale_leg0, &lt_book],
        ] {
            let blocked = router.plan_entry(
                RouteEntryParams {
                    books: pair,
                    ..base_params
                },
                [&mut b0, &mut b1],
            );
            assert_eq!(
                blocked,
                RouterEntryPlan::Blocked {
                    reason: RouterBlockReason::InsufficientCrossableDepthOrEdge
                }
            );
            assert!(router.active_reservation().is_none());
            assert_eq!(b0.used_weight(NOW_MS), 0);
            assert_eq!(b1.used_weight(NOW_MS), 0);
        }

        // 3) Exact boundary entry age succeeds, and contemporaneous follow-up reprices and settles cleanly.
        let boundary_hl = Book {
            timestamp_ms: NOW_MS - 400,
            ..hl_book.clone()
        };
        let RouterEntryPlan::MakerFirst { reservation, .. } = router.plan_entry(
            RouteEntryParams {
                books: [&boundary_hl, &lt_book],
                ..base_params
            },
            [&mut b0, &mut b1],
        ) else {
            panic!("expected MakerFirst at exact boundary entry book age");
        };
        assert_eq!(b0.used_weight(NOW_MS), 0);
        assert_eq!(b1.used_weight(NOW_MS), 1);

        let mut followup_hl = hl_book;
        followup_hl.timestamp_ms = NOW_MS + 100;
        followup_hl.bids = vec![Level {
            price: dec("99.50"),
            contracts: dec("10"),
        }];
        let FollowupPlan::DispatchFastHedgeIoc { hedge_order, .. } = router.on_first_leg_terminal(
            NOW_MS + 100,
            reservation.reservation_id,
            SlowLegTerminalOutcome::Confirmed {
                filled_base: dec("1.5"),
                avg_fill_price: Some(dec("99.99")),
            },
            Some(&followup_hl),
            &mut b0,
        ) else {
            panic!("expected DispatchFastHedgeIoc with contemporaneous follow-up book");
        };
        let expected_limit =
            apply_ioc_protection(false, dec("99.50"), dec("5"), dec("0.01")).unwrap();
        assert_eq!(hedge_order.limit_price, expected_limit);
        assert_eq!(b0.used_weight(NOW_MS + 100), 1);
        assert!(
            !router
                .confirm_hedge_settled(reservation.reservation_id)
                .unwrap()
                .is_active()
        );
    }
}
