//! Venue discovery and validation outside the fixed-size SHM matrix.
use crate::strategy::cross_venue::{QuoteKind, VenueSnapshot};
use anyhow::{Context, Result, bail, ensure};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, str::FromStr};

/// Versioned snapshot identifier for audited venue latency, fee, and quota semantics.
pub const VENUE_CAPABILITY_SNAPSHOT_VERSION: &str = "2026-11-22.v2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityAssetScope {
    All,
    Crypto,
    Equity,
}

impl CapabilityAssetScope {
    pub fn matches(self, asset_class: &str) -> bool {
        match self {
            Self::All => matches!(asset_class, "crypto" | "equity"),
            Self::Crypto => asset_class == "crypto",
            Self::Equity => asset_class == "equity",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchQuotaSemantics {
    /// Each transaction inside a batch consumes action quota independently (`SendTxBatch` != 1 unit).
    PerTransaction,
    /// Standard venue API rate-limit bucket without per-volume tx-unit replenishment.
    RateLimitedApi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupedOrdersSemantics {
    /// `CreateGroupedOrders` is restricted to conditional OTO/OCO/OTOCO structures, NOT independent two-sided quotes.
    ConditionalAttachedOnly,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduledCancelSupport {
    /// Documented in upstream SDK, requires per-network/FFI probe before production reliance.
    SdkDocumentedProbeRequired,
    Supported,
    Unsupported,
}

/// Versioned venue execution capability and fee profile (`OPT-01`).
/// Captures deterministic venue-enforced speed-bumps separately from network RTT.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VenueExecutionCapability {
    pub snapshot_version: &'static str,
    pub venue_id: &'static str,
    pub liquidity_domain: &'static str,
    pub settlement_currency: &'static str,
    pub tier: &'static str,
    pub asset_scope: CapabilityAssetScope,
    pub maker_post_only_delay_ms: u64,
    pub non_post_only_limit_delay_ms: u64,
    pub cancel_delay_ms: u64,
    pub modify_delay_ms: u64,
    pub taker_delay_ms: u64,
    #[serde(with = "rust_decimal::serde::str")]
    pub maker_fee_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub taker_fee_bps: Decimal,
    pub batch_quota_semantics: BatchQuotaSemantics,
    pub grouped_orders_semantics: GroupedOrdersSemantics,
    pub scheduled_cancel_support: ScheduledCancelSupport,
    pub primary_source: &'static str,
}

fn dec(s: &'static str) -> Decimal {
    Decimal::from_str(s).expect("valid static capability decimal")
}

// HIP-3 domains deliberately have no generic audited fee row. Per-asset
// deployerFeeScale/growthMode, collateral and account terms are required; the
// native Hyperliquid base fee is not a verified all-in Entropy/TradeXYZ fee.
// See official fee formula and docs/RESEARCH_DATA_SOURCES.md.
pub fn audited_execution_capabilities() -> Vec<VenueExecutionCapability> {
    use BatchQuotaSemantics::{PerTransaction, RateLimitedApi};
    use CapabilityAssetScope::{All, Crypto, Equity};
    use GroupedOrdersSemantics::ConditionalAttachedOnly;
    use ScheduledCancelSupport::{SdkDocumentedProbeRequired, Supported};
    vec![
        VenueExecutionCapability {
            snapshot_version: VENUE_CAPABILITY_SNAPSHOT_VERSION,
            venue_id: "lighter",
            liquidity_domain: "lighter",
            settlement_currency: "USDC",
            tier: "standard",
            asset_scope: All,
            maker_post_only_delay_ms: 0,
            non_post_only_limit_delay_ms: 300,
            cancel_delay_ms: 300,
            modify_delay_ms: 300,
            taker_delay_ms: 300,
            maker_fee_bps: dec("0"),
            taker_fee_bps: dec("0"),
            batch_quota_semantics: PerTransaction,
            grouped_orders_semantics: ConditionalAttachedOnly,
            scheduled_cancel_support: SdkDocumentedProbeRequired,
            primary_source: "https://docs.lighter.xyz/developers/reference/account-types",
        },
        VenueExecutionCapability {
            snapshot_version: VENUE_CAPABILITY_SNAPSHOT_VERSION,
            venue_id: "lighter",
            liquidity_domain: "lighter",
            settlement_currency: "USDC",
            tier: "plus",
            asset_scope: All,
            maker_post_only_delay_ms: 0,
            non_post_only_limit_delay_ms: 300,
            cancel_delay_ms: 300,
            modify_delay_ms: 300,
            taker_delay_ms: 300,
            maker_fee_bps: dec("0.50"),
            taker_fee_bps: dec("0.50"),
            batch_quota_semantics: PerTransaction,
            grouped_orders_semantics: ConditionalAttachedOnly,
            scheduled_cancel_support: SdkDocumentedProbeRequired,
            primary_source: "https://docs.lighter.xyz/developers/reference/account-types",
        },
        VenueExecutionCapability {
            snapshot_version: VENUE_CAPABILITY_SNAPSHOT_VERSION,
            venue_id: "lighter",
            liquidity_domain: "lighter",
            settlement_currency: "USDC",
            tier: "premium",
            asset_scope: All,
            maker_post_only_delay_ms: 0,
            non_post_only_limit_delay_ms: 140,
            cancel_delay_ms: 0,
            modify_delay_ms: 0,
            taker_delay_ms: 140,
            maker_fee_bps: dec("0.4"),
            taker_fee_bps: dec("2.8"),
            batch_quota_semantics: PerTransaction,
            grouped_orders_semantics: ConditionalAttachedOnly,
            scheduled_cancel_support: SdkDocumentedProbeRequired,
            primary_source: "https://docs.lighter.xyz/developers/reference/account-types",
        },
        VenueExecutionCapability {
            snapshot_version: VENUE_CAPABILITY_SNAPSHOT_VERSION,
            venue_id: "lighter_robinhood",
            liquidity_domain: "lighter:robinhood",
            settlement_currency: "USDG",
            tier: "standard",
            asset_scope: All,
            maker_post_only_delay_ms: 200,
            non_post_only_limit_delay_ms: 300,
            cancel_delay_ms: 200,
            modify_delay_ms: 200,
            taker_delay_ms: 300,
            maker_fee_bps: dec("0"),
            taker_fee_bps: dec("0"),
            batch_quota_semantics: PerTransaction,
            grouped_orders_semantics: ConditionalAttachedOnly,
            scheduled_cancel_support: SdkDocumentedProbeRequired,
            primary_source: "https://docs.lighter.xyz/developers/robinhood-chain/overview",
        },
        VenueExecutionCapability {
            snapshot_version: VENUE_CAPABILITY_SNAPSHOT_VERSION,
            venue_id: "lighter_robinhood",
            liquidity_domain: "lighter:robinhood",
            settlement_currency: "USDG",
            tier: "plus",
            asset_scope: All,
            maker_post_only_delay_ms: 200,
            non_post_only_limit_delay_ms: 300,
            cancel_delay_ms: 200,
            modify_delay_ms: 200,
            taker_delay_ms: 300,
            maker_fee_bps: dec("1.50"),
            taker_fee_bps: dec("1.50"),
            batch_quota_semantics: PerTransaction,
            grouped_orders_semantics: ConditionalAttachedOnly,
            scheduled_cancel_support: SdkDocumentedProbeRequired,
            primary_source: "https://docs.lighter.xyz/developers/robinhood-chain/overview",
        },
        VenueExecutionCapability {
            snapshot_version: VENUE_CAPABILITY_SNAPSHOT_VERSION,
            venue_id: "lighter_robinhood",
            liquidity_domain: "lighter:robinhood",
            settlement_currency: "USDG",
            tier: "premium",
            asset_scope: All,
            maker_post_only_delay_ms: 0,
            non_post_only_limit_delay_ms: 200,
            cancel_delay_ms: 0,
            modify_delay_ms: 0,
            // Base RH Premium (<$1M 14d volume): 1.20/3.50 bps and 200ms taker delay,
            // scaling to 0.84/2.45 bps and 150ms above $500M (never mainnet 0.4/2.8 bps).
            taker_delay_ms: 200,
            maker_fee_bps: dec("1.20"),
            taker_fee_bps: dec("3.50"),
            batch_quota_semantics: PerTransaction,
            grouped_orders_semantics: ConditionalAttachedOnly,
            scheduled_cancel_support: SdkDocumentedProbeRequired,
            primary_source: "https://docs.lighter.xyz/developers/robinhood-chain/overview",
        },
        VenueExecutionCapability {
            snapshot_version: VENUE_CAPABILITY_SNAPSHOT_VERSION,
            venue_id: "arcus",
            liquidity_domain: "arcus",
            settlement_currency: "USDC",
            tier: "level_0",
            asset_scope: Crypto,
            maker_post_only_delay_ms: 0,
            non_post_only_limit_delay_ms: 0,
            cancel_delay_ms: 0,
            modify_delay_ms: 0,
            taker_delay_ms: 0,
            maker_fee_bps: dec("1.50"),
            taker_fee_bps: dec("4.50"),
            batch_quota_semantics: RateLimitedApi,
            grouped_orders_semantics: GroupedOrdersSemantics::Unsupported,
            scheduled_cancel_support: ScheduledCancelSupport::Unsupported,
            primary_source: "arcus_v1_feetiers_20261117.json#sha256=0d608aa1",
        },
        // Default Arcus RWA schedule (0.00 / 2.25 bps); markets with hasCustomFeeSchedule=true
        // (e.g. ANT-USD at 0.00 / 1.12 bps) require per-market lookup via /v1/markets/feeSchedules.
        VenueExecutionCapability {
            snapshot_version: VENUE_CAPABILITY_SNAPSHOT_VERSION,
            venue_id: "arcus",
            liquidity_domain: "arcus",
            settlement_currency: "USDC",
            tier: "level_0",
            asset_scope: Equity,
            maker_post_only_delay_ms: 0,
            non_post_only_limit_delay_ms: 0,
            cancel_delay_ms: 0,
            modify_delay_ms: 0,
            taker_delay_ms: 0,
            maker_fee_bps: dec("0.00"),
            taker_fee_bps: dec("2.25"),
            batch_quota_semantics: RateLimitedApi,
            grouped_orders_semantics: GroupedOrdersSemantics::Unsupported,
            scheduled_cancel_support: ScheduledCancelSupport::Unsupported,
            primary_source: "arcus_v1_feetiers_20261117.json#sha256=0d608aa1",
        },
        VenueExecutionCapability {
            snapshot_version: VENUE_CAPABILITY_SNAPSHOT_VERSION,
            venue_id: "arcus",
            liquidity_domain: "arcus",
            settlement_currency: "USDC",
            tier: "level_4",
            asset_scope: Crypto,
            maker_post_only_delay_ms: 0,
            non_post_only_limit_delay_ms: 0,
            cancel_delay_ms: 0,
            modify_delay_ms: 0,
            taker_delay_ms: 0,
            maker_fee_bps: dec("0.00"),
            taker_fee_bps: dec("2.30"),
            batch_quota_semantics: RateLimitedApi,
            grouped_orders_semantics: GroupedOrdersSemantics::Unsupported,
            scheduled_cancel_support: ScheduledCancelSupport::Unsupported,
            primary_source: "arcus_v1_feetiers_20261117.json#sha256=0d608aa1",
        },
        VenueExecutionCapability {
            snapshot_version: VENUE_CAPABILITY_SNAPSHOT_VERSION,
            venue_id: "arcus",
            liquidity_domain: "arcus",
            settlement_currency: "USDC",
            tier: "level_4",
            asset_scope: Equity,
            maker_post_only_delay_ms: 0,
            non_post_only_limit_delay_ms: 0,
            cancel_delay_ms: 0,
            modify_delay_ms: 0,
            taker_delay_ms: 0,
            maker_fee_bps: dec("0.00"),
            taker_fee_bps: dec("1.15"),
            batch_quota_semantics: RateLimitedApi,
            grouped_orders_semantics: GroupedOrdersSemantics::Unsupported,
            scheduled_cancel_support: ScheduledCancelSupport::Unsupported,
            primary_source: "arcus_v1_feetiers_20261117.json#sha256=0d608aa1",
        },
        VenueExecutionCapability {
            snapshot_version: VENUE_CAPABILITY_SNAPSHOT_VERSION,
            venue_id: "hyperliquid",
            liquidity_domain: "hyperliquid:core",
            settlement_currency: "USDC",
            tier: "base",
            asset_scope: All,
            maker_post_only_delay_ms: 0,
            non_post_only_limit_delay_ms: 0,
            cancel_delay_ms: 0,
            modify_delay_ms: 0,
            taker_delay_ms: 0,
            maker_fee_bps: dec("1.50"),
            taker_fee_bps: dec("4.50"),
            batch_quota_semantics: RateLimitedApi,
            grouped_orders_semantics: ConditionalAttachedOnly,
            scheduled_cancel_support: Supported,
            primary_source: "https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees",
        },
    ]
}

const ASYMMETRIC_TAKER_DELAY_MS: u64 = 100;
const CONCURRENT_MAX_TAKER_DELAY_MS: u64 = 20;

/// Latency-driven execution topology for a two-leg cross-venue strategy (`ARB-IMP-07`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "topology", rename_all = "snake_case")]
pub enum ExecutionTopology {
    MakerFirst { maker_leg: usize, hedge_leg: usize },
    SlowLegFirst { first_leg: usize, second_leg: usize },
    ConcurrentProtectedIoc,
}

/// Selects a causal execution topology from venue speed-bump and settlement contracts.
/// Rejects identical liquidity domains, mismatched settlement assets (`USDC` vs `USDG`),
/// and dual-slow pairs with no fast IOC hedge leg.
pub fn select_execution_topology(
    leg_a: &VenueExecutionCapability,
    leg_b: &VenueExecutionCapability,
    dual_taker_required: bool,
) -> Result<ExecutionTopology> {
    ensure!(
        leg_a.settlement_currency == leg_b.settlement_currency,
        "settlement currency mismatch ({} != {}): cross-stablecoin hedge requires explicit FX ledger",
        leg_a.settlement_currency,
        leg_b.settlement_currency
    );
    ensure!(
        leg_a.liquidity_domain != leg_b.liquidity_domain,
        "legs share liquidity domain {}",
        leg_a.liquidity_domain
    );

    let a_delay = leg_a.taker_delay_ms;
    let b_delay = leg_b.taker_delay_ms;
    let (slower_idx, faster_idx, slower_delay, faster_delay) = if a_delay >= b_delay {
        (0, 1, a_delay, b_delay)
    } else {
        (1, 0, b_delay, a_delay)
    };

    if dual_taker_required {
        if slower_delay <= CONCURRENT_MAX_TAKER_DELAY_MS {
            return Ok(ExecutionTopology::ConcurrentProtectedIoc);
        }
        if slower_delay > faster_delay && faster_delay <= CONCURRENT_MAX_TAKER_DELAY_MS {
            return Ok(ExecutionTopology::SlowLegFirst {
                first_leg: slower_idx,
                second_leg: faster_idx,
            });
        }
        bail!(
            "no fast hedge leg available for dual-taker execution ({} taker_delay={}ms, {} taker_delay={}ms)",
            leg_a.venue_id,
            a_delay,
            leg_b.venue_id,
            b_delay
        );
    }

    if slower_delay >= ASYMMETRIC_TAKER_DELAY_MS {
        if faster_delay < ASYMMETRIC_TAKER_DELAY_MS {
            return Ok(ExecutionTopology::MakerFirst {
                maker_leg: slower_idx,
                hedge_leg: faster_idx,
            });
        }
        bail!(
            "no fast hedge leg available ({} taker_delay={}ms, {} taker_delay={}ms)",
            leg_a.venue_id,
            a_delay,
            leg_b.venue_id,
            b_delay
        );
    }

    if slower_delay <= CONCURRENT_MAX_TAKER_DELAY_MS {
        if leg_a.maker_fee_bps < leg_b.maker_fee_bps {
            return Ok(ExecutionTopology::MakerFirst {
                maker_leg: 0,
                hedge_leg: 1,
            });
        }
        if leg_b.maker_fee_bps < leg_a.maker_fee_bps {
            return Ok(ExecutionTopology::MakerFirst {
                maker_leg: 1,
                hedge_leg: 0,
            });
        }
        return Ok(ExecutionTopology::ConcurrentProtectedIoc);
    }

    Ok(ExecutionTopology::MakerFirst {
        maker_leg: slower_idx,
        hedge_leg: faster_idx,
    })
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSupport {
    ExistingGateway,
    RestClient,
    SignedRequests,
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketDataSupport {
    LimitOrderBook,
    Indicative,
}

#[derive(Debug, Clone, Serialize)]
pub struct Venue {
    pub id: String,
    pub liquidity_domain: String,
    pub risk_domain: String,
    pub hyperliquid_dex: Option<String>,
    pub order_support: OrderSupport,
    pub market_data: MarketDataSupport,
}

impl Venue {
    pub fn has_order_book(&self) -> bool {
        self.market_data == MarketDataSupport::LimitOrderBook
    }
}

#[derive(Debug)]
pub struct VenueRegistry(BTreeMap<String, Venue>);

impl Default for VenueRegistry {
    fn default() -> Self {
        use MarketDataSupport::*;
        use OrderSupport::*;
        Self(
            [
                (
                    "lighter",
                    "lighter",
                    "lighter",
                    None,
                    ExistingGateway,
                    LimitOrderBook,
                ),
                (
                    "lighter_robinhood",
                    "lighter:robinhood",
                    "lighter",
                    None,
                    ReadOnly,
                    LimitOrderBook,
                ),
                (
                    "hyperliquid",
                    "hyperliquid:core",
                    "hyperliquid",
                    Some(""),
                    RestClient,
                    LimitOrderBook,
                ),
                (
                    "entropy",
                    "hyperliquid:io",
                    "hyperliquid",
                    Some("io"),
                    RestClient,
                    LimitOrderBook,
                ),
                (
                    "trade_xyz",
                    "hyperliquid:xyz",
                    "hyperliquid",
                    Some("xyz"),
                    RestClient,
                    LimitOrderBook,
                ),
                ("arcus", "arcus", "arcus", None, RestClient, LimitOrderBook),
                ("bulk", "bulk", "bulk", None, ReadOnly, LimitOrderBook),
                (
                    "extended",
                    "extended",
                    "extended",
                    None,
                    ReadOnly,
                    LimitOrderBook,
                ),
                (
                    "variational",
                    "variational",
                    "variational",
                    None,
                    ReadOnly,
                    Indicative,
                ),
            ]
            .into_iter()
            .map(|(id, liquidity, risk, dex, order_support, market_data)| {
                (
                    id.into(),
                    Venue {
                        id: id.into(),
                        liquidity_domain: liquidity.into(),
                        risk_domain: risk.into(),
                        hyperliquid_dex: dex.map(str::to_owned),
                        order_support,
                        market_data,
                    },
                )
            })
            .collect(),
        )
    }
}

impl VenueRegistry {
    pub fn get(&self, id: &str) -> Result<&Venue> {
        self.0
            .get(id)
            .with_context(|| format!("Unregistered venue {id}"))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Venue> {
        self.0.values()
    }

    /// Register another verified HIP-3 namespace without allocating a SHM slot.
    /// Aliases resolve to the same liquidity domain as the original book.
    pub fn register_hip3(&mut self, id: &str, dex: &str) -> Result<()> {
        let valid = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        };
        ensure!(
            valid(id) && valid(dex) && dex != "core",
            "Invalid venue or deployer name"
        );
        ensure!(!self.0.contains_key(id), "Venue {id} is already registered");
        self.0.insert(
            id.into(),
            Venue {
                id: id.into(),
                liquidity_domain: format!("hyperliquid:{dex}"),
                risk_domain: "hyperliquid".into(),
                hyperliquid_dex: Some(dex.into()),
                order_support: OrderSupport::RestClient,
                market_data: MarketDataSupport::LimitOrderBook,
            },
        );
        Ok(())
    }

    pub fn validate_snapshot(&self, snapshot: &VenueSnapshot) -> Result<&Venue> {
        let venue = self.get(&snapshot.venue)?;
        ensure!(
            snapshot.liquidity_domain == venue.liquidity_domain,
            "Liquidity domain for {} must be {}",
            venue.id,
            venue.liquidity_domain
        );
        if !venue.has_order_book() {
            ensure!(
                snapshot.quote_kind == QuoteKind::Indicative,
                "{} only publishes indicative quotes",
                venue.id
            );
        }
        Ok(venue)
    }

    /// Resolve the versioned execution capability (`OPT-01`) for `(venue_id, tier, asset_class)`.
    /// Rejects unregistered or unverified venues/tiers rather than defaulting unknown delays or fees to zero.
    pub fn resolve_execution_capability(
        &self,
        venue_id: &str,
        tier: &str,
        asset_class: &str,
    ) -> Result<VenueExecutionCapability> {
        let venue = self.get(venue_id)?;
        ensure!(
            matches!(asset_class, "crypto" | "equity"),
            "Unsupported capability asset_class {asset_class:?} for venue {venue_id}"
        );
        for cap in audited_execution_capabilities() {
            if cap.venue_id == venue.id && cap.tier == tier && cap.asset_scope.matches(asset_class)
            {
                return Ok(cap);
            }
        }
        bail!(
            "Unverified execution capability for venue={venue_id} tier={tier} asset_class={asset_class} (snapshot={VENUE_CAPABILITY_SNAPSHOT_VERSION})"
        )
    }
}

/// Effective per-asset HIP-3 fee and volume credit terms (`OPT-01`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Hip3AssetFeeSchedule {
    #[serde(with = "rust_decimal::serde::str")]
    pub effective_maker_fee_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub effective_taker_fee_bps: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub tier_volume_credit_weight: Decimal,
    pub growth_mode_enabled: bool,
}

/// Compute effective HIP-3 fee schedule from native Hyperliquid base fees,
/// deployer fee scale, growth-mode status, and asset disjointness.
///
/// Fails closed if `growth_mode_enabled` is requested for a verified core
/// crypto asset or if `deployer_fee_scale` falls outside `[0, 5]`.
pub fn compute_hip3_execution_fees(
    base_maker_bps: Decimal,
    base_taker_bps: Decimal,
    deployer_fee_scale: Decimal,
    growth_mode_enabled: bool,
    is_verified_core_crypto_asset: bool,
) -> Result<Hip3AssetFeeSchedule> {
    ensure!(
        base_maker_bps >= Decimal::ZERO && base_taker_bps >= Decimal::ZERO,
        "HIP-3 base fees must be nonnegative"
    );
    ensure!(
        deployer_fee_scale >= Decimal::ZERO && deployer_fee_scale <= Decimal::from(5),
        "HIP-3 deployer_fee_scale must lie in [0, 5]"
    );
    ensure!(
        !(growth_mode_enabled && is_verified_core_crypto_asset),
        "HIP-3 growth mode excludes verified core crypto assets"
    );
    // Above a 100% deployer fee scale, the protocol share rises to match the deployer share.
    let scale_factor = if deployer_fee_scale < Decimal::ONE {
        Decimal::ONE + deployer_fee_scale
    } else {
        Decimal::TWO * deployer_fee_scale
    };
    let growth_factor = if growth_mode_enabled {
        dec("0.10")
    } else {
        Decimal::ONE
    };
    Ok(Hip3AssetFeeSchedule {
        effective_maker_fee_bps: (base_maker_bps * scale_factor * growth_factor).normalize(),
        effective_taker_fee_bps: (base_taker_bps * scale_factor * growth_factor).normalize(),
        tier_volume_credit_weight: growth_factor.normalize(),
        growth_mode_enabled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::cross_venue::tests::market;

    #[test]
    fn hip3_aliases_share_books_but_core_has_a_distinct_book() {
        let mut registry = VenueRegistry::default();
        registry.register_hip3("entropy-alias", "io").unwrap();
        let alias = registry.get("entropy-alias").unwrap();
        let entropy = registry.get("entropy").unwrap();
        let core = registry.get("hyperliquid").unwrap();
        assert_eq!(alias.liquidity_domain, entropy.liquidity_domain);
        assert_ne!(core.liquidity_domain, entropy.liquidity_domain);
        assert_eq!(core.risk_domain, entropy.risk_domain);
        assert!(registry.register_hip3("lighter", "io").is_err());
    }

    #[test]
    fn readonly_and_liquidity_domain_cannot_be_overridden_by_replay() {
        let registry = VenueRegistry::default();
        let mut snapshot = market("variational", 99.0, 100.0);
        snapshot.liquidity_domain = "variational".into();
        assert!(registry.validate_snapshot(&snapshot).is_err());
        snapshot.quote_kind = QuoteKind::Indicative;
        assert!(registry.validate_snapshot(&snapshot).is_ok());
        snapshot.liquidity_domain = "independent".into();
        assert!(registry.validate_snapshot(&snapshot).is_err());
    }

    #[test]
    fn execution_capability_separates_lighter_core_and_robinhood_and_rejects_unverified() {
        let registry = VenueRegistry::default();
        let core_std = registry
            .resolve_execution_capability("lighter", "standard", "crypto")
            .unwrap();
        let rh_std = registry
            .resolve_execution_capability("lighter_robinhood", "standard", "equity")
            .unwrap();
        assert_eq!(core_std.settlement_currency, "USDC");
        assert_eq!(rh_std.settlement_currency, "USDG");
        assert_eq!(core_std.maker_post_only_delay_ms, 0);
        assert_eq!(rh_std.maker_post_only_delay_ms, 200);
        assert_eq!(core_std.cancel_delay_ms, 300);
        assert_eq!(rh_std.cancel_delay_ms, 200);

        let core_prem = registry
            .resolve_execution_capability("lighter", "premium", "crypto")
            .unwrap();
        let rh_prem = registry
            .resolve_execution_capability("lighter_robinhood", "premium", "equity")
            .unwrap();
        assert_eq!(core_prem.taker_delay_ms, 140);
        assert_eq!(core_prem.maker_fee_bps, dec("0.4"));
        assert_eq!(core_prem.taker_fee_bps, dec("2.8"));
        assert_eq!(rh_prem.taker_delay_ms, 200);
        assert_eq!(rh_prem.maker_fee_bps, dec("1.20"));
        assert_eq!(rh_prem.taker_fee_bps, dec("3.50"));
        assert_ne!(rh_prem.maker_fee_bps, core_prem.maker_fee_bps);
        assert_ne!(rh_prem.taker_fee_bps, core_prem.taker_fee_bps);

        let core_plus = registry
            .resolve_execution_capability("lighter", "plus", "crypto")
            .unwrap();
        let rh_plus = registry
            .resolve_execution_capability("lighter_robinhood", "plus", "equity")
            .unwrap();
        assert_eq!(core_plus.maker_post_only_delay_ms, 0);
        assert_eq!(core_plus.cancel_delay_ms, 300);
        assert_eq!(core_plus.taker_delay_ms, 300);
        assert_eq!(core_plus.maker_fee_bps, dec("0.50"));
        assert_eq!(core_plus.taker_fee_bps, dec("0.50"));
        assert_eq!(rh_plus.maker_post_only_delay_ms, 200);
        assert_eq!(rh_plus.cancel_delay_ms, 200);
        assert_eq!(rh_plus.taker_delay_ms, 300);
        assert_eq!(rh_plus.maker_fee_bps, dec("1.50"));
        assert_eq!(rh_plus.taker_fee_bps, dec("1.50"));
        assert_eq!(
            core_prem.batch_quota_semantics,
            BatchQuotaSemantics::PerTransaction
        );
        assert_eq!(
            core_prem.grouped_orders_semantics,
            GroupedOrdersSemantics::ConditionalAttachedOnly
        );

        let arcus_crypto = registry
            .resolve_execution_capability("arcus", "level_0", "crypto")
            .unwrap();
        let arcus_rwa = registry
            .resolve_execution_capability("arcus", "level_0", "equity")
            .unwrap();
        assert_eq!(arcus_crypto.maker_fee_bps, dec("1.50"));
        assert_eq!(arcus_crypto.taker_fee_bps, dec("4.50"));
        assert_eq!(arcus_rwa.maker_fee_bps, dec("0.00"));
        assert_eq!(arcus_rwa.taker_fee_bps, dec("2.25"));

        // Level 4 (Platinum) per the public /v1/feetiers (230 ppm) and per-market
        // /v1/markets (RWA custom schedule 115 ppm) snapshots, sha256=0d608aa1.
        let arcus_crypto_l4 = registry
            .resolve_execution_capability("arcus", "level_4", "crypto")
            .unwrap();
        let arcus_rwa_l4 = registry
            .resolve_execution_capability("arcus", "level_4", "equity")
            .unwrap();
        assert_eq!(arcus_crypto_l4.maker_fee_bps, dec("0.00"));
        assert_eq!(arcus_crypto_l4.taker_fee_bps, dec("2.30"));
        assert_eq!(arcus_rwa_l4.maker_fee_bps, dec("0.00"));
        assert_eq!(arcus_rwa_l4.taker_fee_bps, dec("1.15"));

        // Data discovery remains supported, but HIP-3 execution economics need
        // asset-specific evidence instead of inheriting native perp base fees.
        for venue in ["entropy", "trade_xyz"] {
            assert!(registry.get(venue).is_ok());
            for asset_class in ["crypto", "equity"] {
                assert!(
                    registry
                        .resolve_execution_capability(venue, "base", asset_class)
                        .is_err()
                );
            }
        }

        // Unverified or read-only venues fail closed rather than defaulting to zero delay/fee.
        assert!(
            registry
                .resolve_execution_capability("edgex", "standard", "crypto")
                .is_err()
        );
        assert!(
            registry
                .resolve_execution_capability("bulk", "base", "crypto")
                .is_err()
        );
        assert!(
            registry
                .resolve_execution_capability("lighter", "unknown_tier", "crypto")
                .is_err()
        );
    }

    #[test]
    fn hip3_execution_fees_enforce_growth_mode_cuts_and_core_crypto_disjointness() {
        let base_maker = dec("1.50");
        let base_taker = dec("4.50");
        let regular =
            compute_hip3_execution_fees(base_maker, base_taker, Decimal::ONE, false, false)
                .unwrap();
        assert_eq!(regular.effective_maker_fee_bps, dec("3"));
        assert_eq!(regular.effective_taker_fee_bps, dec("9"));
        assert_eq!(regular.tier_volume_credit_weight, Decimal::ONE);

        let high_scale =
            compute_hip3_execution_fees(base_maker, base_taker, dec("2"), false, false).unwrap();
        assert_eq!(high_scale.effective_maker_fee_bps, dec("6"));
        assert_eq!(high_scale.effective_taker_fee_bps, dec("18"));

        let growth =
            compute_hip3_execution_fees(base_maker, base_taker, Decimal::ONE, true, false).unwrap();
        assert_eq!(growth.effective_maker_fee_bps, dec("0.3"));
        assert_eq!(growth.effective_taker_fee_bps, dec("0.9"));
        assert_eq!(growth.tier_volume_credit_weight, dec("0.1"));

        // Core crypto asset disjointness fails closed under HIP-3 growth mode.
        assert!(
            compute_hip3_execution_fees(base_maker, base_taker, Decimal::ONE, true, true).is_err()
        );
        assert!(
            compute_hip3_execution_fees(base_maker, base_taker, dec("-0.1"), false, false).is_err()
        );
    }

    #[test]
    fn execution_topology_router_respects_speed_bumps_fees_and_settlement_domains() {
        let registry = VenueRegistry::default();
        let core_std = registry
            .resolve_execution_capability("lighter", "standard", "equity")
            .unwrap();
        let rh_std = registry
            .resolve_execution_capability("lighter_robinhood", "standard", "equity")
            .unwrap();
        let arcus_eq = registry
            .resolve_execution_capability("arcus", "level_0", "equity")
            .unwrap();
        let hl_base = registry
            .resolve_execution_capability("hyperliquid", "base", "crypto")
            .unwrap();

        // Core Standard (300ms taker, 0ms PostOnly) <-> Arcus Equity (0ms)
        assert_eq!(
            select_execution_topology(&core_std, &arcus_eq, false).unwrap(),
            ExecutionTopology::MakerFirst {
                maker_leg: 0,
                hedge_leg: 1
            }
        );
        // Dual taker across Core Standard (300ms) vs Arcus Equity (0ms) enforces SlowLegFirst
        assert_eq!(
            select_execution_topology(&core_std, &arcus_eq, true).unwrap(),
            ExecutionTopology::SlowLegFirst {
                first_leg: 0,
                second_leg: 1
            }
        );
        // Fast <-> Fast routes MakerFirst onto lower maker fee leg (Arcus 0bp vs HL 1.50bp)
        assert_eq!(
            select_execution_topology(&arcus_eq, &hl_base, false).unwrap(),
            ExecutionTopology::MakerFirst {
                maker_leg: 0,
                hedge_leg: 1
            }
        );
        assert_eq!(
            select_execution_topology(&arcus_eq, &hl_base, true).unwrap(),
            ExecutionTopology::ConcurrentProtectedIoc
        );
        // Cross-settlement (USDC vs USDG) fails closed
        assert!(select_execution_topology(&core_std, &rh_std, false).is_err());
        // Dual-slow fails closed without a fast hedge leg
        let core_slow = VenueExecutionCapability {
            venue_id: "lighter_b",
            liquidity_domain: "lighter_b",
            settlement_currency: "USDG",
            ..core_std
        };
        assert!(select_execution_topology(&core_slow, &rh_std, false).is_err());
        // Same liquidity domain fails closed
        assert!(select_execution_topology(&core_std, &core_std, false).is_err());
    }
}
