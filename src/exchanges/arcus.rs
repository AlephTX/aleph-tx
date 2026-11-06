//! Public perpetual-market discovery and offline Arcus REST order signing.
//! The caller owns transmission, durable intent IDs and reconciliation.
use anyhow::{Context, Result, ensure};
use ed25519_dalek::{Signer, SigningKey};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub enum Network {
    Mainnet,
    Testnet,
}

impl Network {
    pub fn base_url(self) -> &'static str {
        match self {
            Self::Mainnet => "https://api.arcus.xyz",
            Self::Testnet => "https://api.testnet.arcus.xyz",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Market {
    pub market_id: u16,
    pub market_display_name: String,
    pub status: String,
    #[serde(rename = "type")]
    pub contract_type: String,
    pub base_asset: String,
    pub quote_asset: String,
    #[serde(with = "rust_decimal::serde::str")]
    pub tick_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub step_size: Decimal,
    #[serde(default)]
    pub tick_tiers: Vec<TickTier>,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_order_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub max_order_size: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub min_order_notional: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TickTier {
    #[serde(with = "rust_decimal::serde::str")]
    pub tick: Decimal,
    #[serde(default, with = "rust_decimal::serde::str_option")]
    pub up_to_price: Option<Decimal>,
}

pub struct ArcusInfo {
    client: reqwest::Client,
    network: Network,
}

impl ArcusInfo {
    pub fn new(network: Network) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .user_agent("AlephTX/Arcus")
                .build()?,
            network,
        })
    }

    pub async fn bbo(&self, market: &str) -> Result<Book> {
        ensure!(
            !market.is_empty()
                && market
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
            "Invalid Arcus market name"
        );
        let book: Book = self
            .client
            .get(format!("{}/v1/bbo/{market}", self.network.base_url()))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        book.validate()?;
        Ok(book)
    }

    pub async fn markets(&self) -> Result<Vec<Market>> {
        #[derive(Deserialize)]
        struct Response {
            markets: Vec<Market>,
        }
        Ok(self
            .client
            .get(format!("{}/v1/markets", self.network.base_url()))
            .send()
            .await?
            .error_for_status()?
            .json::<Response>()
            .await?
            .markets)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Book {
    pub best_bid: Option<BookLevel>,
    pub best_ask: Option<BookLevel>,
    pub last_sequence_id: u64,
    pub global_sequence_id: u64,
    /// Exchange snapshot time in Unix microseconds.
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookLevel {
    #[serde(with = "rust_decimal::serde::str")]
    pub price: Decimal,
    #[serde(with = "rust_decimal::serde::str")]
    pub size: Decimal,
}

impl Book {
    fn validate(&self) -> Result<()> {
        ensure!(self.timestamp > 0, "Missing Arcus source timestamp");
        for level in [&self.best_bid, &self.best_ask].into_iter().flatten() {
            ensure!(
                level.price > Decimal::ZERO && level.size > Decimal::ZERO,
                "Invalid Arcus book level"
            );
        }
        if let (Some(bid), Some(ask)) = (&self.best_bid, &self.best_ask) {
            ensure!(bid.price < ask.price, "Crossed Arcus book");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Side {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum TimeInForce {
    Gtt,
    Fok,
    Ioc,
    Alo,
}

pub struct LimitOrder<'a> {
    pub side: Side,
    pub quantity: Decimal,
    pub price: Decimal,
    pub time_in_force: TimeInForce,
    pub reduce_only: bool,
    pub client_id: &'a str,
}

/// Timestamp and signature headers must accompany this exact body.
pub struct SignedRequest {
    pub path: &'static str,
    pub api_key: String,
    pub timestamp_ns: u64,
    pub signature: String,
    pub body: serde_json::Value,
}

pub struct ArcusSigner {
    key: SigningKey,
    address: String,
    account_index: u8,
}

impl ArcusSigner {
    pub fn new(key: SigningKey, address: &str, account_index: u8) -> Result<Self> {
        super::hyperliquid::validate_hex(address, 20)?;
        ensure!(
            account_index < 10,
            "Arcus account index must be 0 through 9"
        );
        Ok(Self {
            key,
            address: address.to_ascii_lowercase(),
            account_index,
        })
    }

    pub fn place_limit(
        &self,
        market: &Market,
        order: LimitOrder<'_>,
        timestamp_ns: u64,
    ) -> Result<SignedRequest> {
        ensure!(
            market.status == "ONLINE" && market.contract_type == "PERPETUAL",
            "Arcus market is not an online perpetual"
        );
        validate_id(order.client_id)?;
        let p = units(order.price, market.tick_size)?;
        let q = units(order.quantity, market.step_size)?;
        market.validate_tier(order.price)?;
        ensure!(
            order.quantity >= market.min_order_size
                && order.quantity <= market.max_order_size
                && order
                    .quantity
                    .checked_mul(order.price)
                    .context("Arcus notional overflow")?
                    >= market.min_order_notional,
            "Arcus size/notional outside market limits"
        );
        // Arcus requires this field even on IOC and FOK. Use the documented
        // 40-day offset, with the signed value in ns and body value in µs.
        const FORTY_DAYS_US: u64 = 40 * 86_400 * 1_000_000;
        let expiry_us = (timestamp_ns / 1_000)
            .checked_add(FORTY_DAYS_US)
            .context("Arcus expiry overflow")?;
        let expiry_ns = expiry_us
            .checked_mul(1_000)
            .context("Arcus expiry overflow")?;
        // A struct fixes field order even when serde_json's preserve_order feature is enabled.
        #[derive(Serialize)]
        struct Payload<'a> {
            ad: &'a str,
            ai: u8,
            c: &'a str,
            ct: u64,
            g: u64,
            m: u16,
            op: u8,
            p: u64,
            q: u64,
            r: u8,
            s: u8,
            t: u8,
            v: u8,
        }
        let payload = Payload {
            ad: &self.address,
            ai: self.account_index,
            c: order.client_id,
            ct: timestamp_ns,
            g: expiry_ns,
            m: market.market_id,
            op: 1,
            p,
            q,
            r: u8::from(order.reduce_only),
            s: order.side as u8,
            t: order.time_in_force as u8,
            v: 1,
        };
        self.sign("/v1/placeOrder", timestamp_ns, &payload, serde_json::json!({
            "address": self.address, "accountIndex": self.account_index, "clientId": order.client_id,
            "marketId": market.market_id, "orderSide": order.side, "orderType": "LIMIT",
            "price": order.price.normalize().to_string(), "quantity": order.quantity.normalize().to_string(),
            "timeInForce": order.time_in_force, "reduceOnly": order.reduce_only,
            "goodTilTime": expiry_us.to_string(), "timestamp": timestamp_ns,
        }))
    }

    pub fn cancel_by_order_id(
        &self,
        market_id: u16,
        order_id: &str,
        timestamp_ns: u64,
    ) -> Result<SignedRequest> {
        validate_id(order_id)?;
        #[derive(Serialize)]
        struct Payload<'a> {
            ad: &'a str,
            ai: u8,
            ct: u64,
            id: &'a str,
            m: u16,
            op: u8,
            v: u8,
        }
        let payload = Payload {
            ad: &self.address,
            ai: self.account_index,
            ct: timestamp_ns,
            id: order_id,
            m: market_id,
            op: 2,
            v: 1,
        };
        self.sign(
            "/v1/cancelOrder",
            timestamp_ns,
            &payload,
            serde_json::json!({
                "address": self.address, "accountIndex": self.account_index, "marketId": market_id,
                "kind": "orderId", "orderId": order_id, "timestamp": timestamp_ns,
            }),
        )
    }

    fn sign(
        &self,
        path: &'static str,
        timestamp_ns: u64,
        payload: &impl Serialize,
        body: serde_json::Value,
    ) -> Result<SignedRequest> {
        ensure!(timestamp_ns > 0, "Arcus timestamp must be positive");
        Ok(SignedRequest {
            path,
            timestamp_ns,
            body,
            api_key: hex::encode(self.key.verifying_key().to_bytes()),
            signature: hex::encode(self.key.sign(&serde_json::to_vec(payload)?).to_bytes()),
        })
    }
}

fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.trim().is_empty() && !id.chars().any(char::is_control),
        "Invalid Arcus order ID"
    );
    Ok(())
}

fn units(value: Decimal, step: Decimal) -> Result<u64> {
    ensure!(
        value > Decimal::ZERO
            && step > Decimal::ZERO
            && value.checked_rem(step) == Some(Decimal::ZERO),
        "Arcus price/size must align exactly to its market increment"
    );
    value
        .checked_div(step)
        .and_then(|q| q.to_u64())
        .context("Arcus tick or quantity overflow")
}

impl Market {
    fn validate_tier(&self, price: Decimal) -> Result<()> {
        let mut last = Decimal::ZERO;
        for tier in &self.tick_tiers {
            ensure!(
                tier.up_to_price.is_none_or(|upper| upper > last),
                "Invalid Arcus tick ladder"
            );
            if tier.up_to_price.is_none_or(|upper| price <= upper) {
                units(price, tier.tick)?;
                return Ok(());
            }
            last = tier.up_to_price.context("Invalid Arcus tick ladder")?;
        }
        ensure!(
            self.tick_tiers.is_empty(),
            "Price outside Arcus tick ladder"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier};

    const ADDRESS: &str = "0x11111111111111111111111111111111111111Ab";
    const TS: u64 = 1_800_000_000_000_000_000;
    fn market() -> Market {
        serde_json::from_value(serde_json::json!({
            "marketId": 1, "marketDisplayName": "BTC-USD", "status":"ONLINE",
            "type":"PERPETUAL", "baseAsset":"BTC","quoteAsset":"USD","tickSize":"0.1",
            "stepSize":"0.001","minOrderSize":"0.001","maxOrderSize":"10",
            "minOrderNotional":"5",
            "tickTiers": [{"upToPrice":"50000","tick":"0.1"},{"tick":"0.5"}]
        }))
        .unwrap()
    }

    #[test]
    fn typed_place_and_cancel_sign_the_exact_arcus_wire_values() {
        let signer = ArcusSigner::new(SigningKey::from_bytes(&[7; 32]), ADDRESS, 2).unwrap();
        let request = signer
            .place_limit(
                &market(),
                LimitOrder {
                    side: Side::Sell,
                    quantity: Decimal::new(2, 3),
                    price: Decimal::new(600005, 1),
                    time_in_force: TimeInForce::Ioc,
                    reduce_only: true,
                    client_id: "MixedCase",
                },
                TS,
            )
            .unwrap();
        assert_eq!(request.body["goodTilTime"], "1803456000000000");
        assert_eq!(request.body["timestamp"], TS);
        assert_eq!(request.body["clientId"], "MixedCase");
        assert_eq!(request.body["reduceOnly"], true);
        let canonical = br#"{"ad":"0x11111111111111111111111111111111111111ab","ai":2,"c":"MixedCase","ct":1800000000000000000,"g":1803456000000000000,"m":1,"op":1,"p":600005,"q":2,"r":1,"s":1,"t":2,"v":1}"#;
        verify(&signer, &request, canonical);
        let cancel = signer.cancel_by_order_id(1, "OrderCase", TS).unwrap();
        assert_eq!(cancel.body["kind"], "orderId");
        verify(&signer, &cancel, br#"{"ad":"0x11111111111111111111111111111111111111ab","ai":2,"ct":1800000000000000000,"id":"OrderCase","m":1,"op":2,"v":1}"#);
    }

    fn verify(signer: &ArcusSigner, request: &SignedRequest, bytes: &[u8]) {
        let signature = Signature::from_slice(&hex::decode(&request.signature).unwrap()).unwrap();
        signer
            .key
            .verifying_key()
            .verify(bytes, &signature)
            .unwrap();
    }

    #[test]
    fn rejects_bad_tick_tier_quantity_step_or_market_status() {
        let signer = ArcusSigner::new(SigningKey::from_bytes(&[7; 32]), ADDRESS, 0).unwrap();
        let order = |p, q| LimitOrder {
            side: Side::Buy,
            quantity: Decimal::new(q, 4),
            price: Decimal::new(p, 1),
            time_in_force: TimeInForce::Ioc,
            reduce_only: false,
            client_id: "test",
        };
        let mut m = market();
        assert!(signer.place_limit(&m, order(600001, 20), TS).is_err());
        assert!(signer.place_limit(&m, order(600005, 21), TS).is_err());
        m.status = "OFFLINE".into();
        assert!(signer.place_limit(&m, order(600005, 20), TS).is_err());
    }

    #[test]
    fn empty_book_does_not_invent_prices() {
        let book: Book = serde_json::from_value(serde_json::json!({
            "bestBid":null,"bestAsk":null,"lastSequenceId":1,
            "globalSequenceId":5,"timestamp":1800000000000000_u64
        }))
        .unwrap();
        book.validate().unwrap();
        assert!(book.best_bid.is_none() && book.best_ask.is_none());
    }
}
