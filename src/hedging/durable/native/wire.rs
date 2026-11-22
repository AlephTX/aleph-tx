//! Minimal fields consumed from public API schemas; extra venue fields retained
//! by the caller's private raw receipt, not printed by deserialization errors.
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
fn exact<'de, D: Deserializer<'de>>(d: D) -> Result<Decimal, D::Error> {
    let raw = String::deserialize(d)?;
    Decimal::from_str_exact(&raw).map_err(|_| serde::de::Error::custom("Inexact native decimal"))
}
fn optional_exact<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Decimal>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|raw| {
            Decimal::from_str_exact(&raw)
                .map_err(|_| serde::de::Error::custom("Inexact native decimal"))
        })
        .transpose()
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ArcusOrder {
    pub order_id: String,
    pub client_id: Option<String>,
    pub address: String,
    #[serde(default)]
    pub account_index: u8,
    pub market_id: u16,
    pub market_display_name: String,
    pub side: String,
    pub status: String,
    #[serde(deserialize_with = "exact")]
    pub price: Decimal,
    #[serde(deserialize_with = "exact")]
    pub original_size: Decimal,
    #[serde(deserialize_with = "exact")]
    pub remaining_size: Decimal,
    #[serde(default, deserialize_with = "optional_exact")]
    pub filled_size: Option<Decimal>,
    pub updated_at: u64,
}
#[derive(Deserialize)]
pub(super) struct ArcusFills {
    pub fills: Vec<ArcusFill>,
    pub total: Option<usize>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ArcusFill {
    pub trade_id: String,
    pub order_id: String,
    pub client_id: Option<String>,
    pub address: String,
    pub account_index: u8,
    pub market_id: u16,
    pub market_display_name: String,
    pub side: String,
    pub role: String,
    #[serde(deserialize_with = "exact")]
    pub original_size: Decimal,
    #[serde(deserialize_with = "exact")]
    pub size: Decimal,
    #[serde(deserialize_with = "exact")]
    pub price: Decimal,
    #[serde(deserialize_with = "exact")]
    pub fee: Decimal,
    pub created_at: u64,
}
#[derive(Deserialize)]
pub(super) struct ArcusPositions {
    #[serde(deserialize_with = "unique_positions")]
    pub positions: BTreeMap<String, ArcusPosition>,
    pub total: Option<usize>,
}

fn unique_positions<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, ArcusPosition>, D::Error> {
    struct Unique;
    impl<'de> serde::de::Visitor<'de> for Unique {
        type Value = BTreeMap<String, ArcusPosition>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("unique native position keys")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut result = BTreeMap::new();
            while let Some((key, value)) = map.next_entry::<String, ArcusPosition>()? {
                if result.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("Repeated position key"));
                }
            }
            Ok(result)
        }
    }
    d.deserialize_map(Unique)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ArcusPosition {
    pub address: String,
    pub account_index: u8,
    pub market_id: u16,
    pub market_display_name: String,
    pub side: String,
    #[serde(deserialize_with = "exact")]
    pub size: Decimal,
}
#[derive(Deserialize)]
pub(super) struct HlOrderEnvelope {
    pub status: String,
    pub order: Option<HlLocatedOrder>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HlLocatedOrder {
    pub order: HlOrder,
    pub status: String,
    pub status_timestamp: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HlOrder {
    pub coin: String,
    pub side: String,
    pub oid: u64,
    pub cloid: Option<String>,
    #[serde(deserialize_with = "exact")]
    pub orig_sz: Decimal,
    #[serde(deserialize_with = "exact")]
    pub sz: Decimal,
    #[serde(deserialize_with = "exact")]
    pub limit_px: Decimal,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HlFill {
    pub coin: String,
    pub side: String,
    pub oid: u64,
    pub tid: u64,
    pub time: u64,
    pub fee_token: String,
    #[serde(deserialize_with = "exact")]
    pub px: Decimal,
    #[serde(deserialize_with = "exact")]
    pub sz: Decimal,
    #[serde(deserialize_with = "exact")]
    pub fee: Decimal,
    #[serde(default, deserialize_with = "optional_exact")]
    pub builder_fee: Option<Decimal>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct HlPositions {
    pub asset_positions: Vec<HlAssetPosition>,
    pub time: u64,
}
#[derive(Deserialize)]
pub(super) struct HlAssetPosition {
    pub position: HlPosition,
}
#[derive(Deserialize)]
pub(super) struct HlPosition {
    pub coin: String,
    #[serde(deserialize_with = "exact")]
    pub szi: Decimal,
}
