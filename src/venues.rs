//! Venue discovery and validation outside the fixed-size SHM matrix.
use crate::strategy::cross_venue::{QuoteKind, VenueSnapshot};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::collections::BTreeMap;

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
}
