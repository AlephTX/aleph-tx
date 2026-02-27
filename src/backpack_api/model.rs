use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct BackpackOrderRequest {
    pub symbol: String,
    pub side: String,
    #[serde(rename = "orderType")]
    pub order_type: String,
    pub price: String,
    pub quantity: String,
    #[serde(rename = "clientId", skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(rename = "postOnly", skip_serializing_if = "Option::is_none")]
    pub post_only: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_in_force: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BackpackOrderResponse {
    pub id: String,
    pub symbol: String,
    pub side: String,
    pub price: Option<String>,
    pub quantity: Option<String>,
    pub status: String,
}

#[derive(Debug, Deserialize)]
pub struct BackpackPosition {
    pub symbol: String,
    pub quantity: String,
    #[serde(rename = "averageEntryPrice")]
    pub average_entry_price: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BackpackFill {
    pub symbol: String,
    pub price: String,
    pub quantity: String,
    pub side: String,
    #[serde(rename = "isMaker")]
    pub is_maker: bool,
}
