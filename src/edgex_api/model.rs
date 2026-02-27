use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TimeInForce {
    GoodTilCancel,
    ImmediateOrCancel,
    FillOrKill,
    PostOnly,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CreateOrderRequest {
    pub price: String,
    pub size: String,
    pub r#type: OrderType,
    pub time_in_force: TimeInForce,
    pub account_id: u64,
    pub contract_id: u64,
    pub side: OrderSide,
    #[serde(rename = "clientOrderId")]
    pub client_order_id: String,
    #[serde(rename = "expireTime")]
    pub expire_time: u64,
    // L2 Auth fields
    pub l2_nonce: u64,
    pub l2_value: String,
    pub l2_size: String,
    pub l2_limit_fee: String,
    pub l2_expire_time: u64,
    pub l2_signature: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CancelOrderRequest {
    pub account_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
    pub contract_id: u64,
    // L2 Auth fields
    pub l2_nonce: u64,
    pub l2_signature: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CancelAllOrderRequest {
    pub account_id: u64,
    pub filter_contract_id_list: Vec<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OrderResponse {
    pub order_id: u64,
    pub client_order_id: Option<String>,
    pub status: String,
    // Add other fields as discovered from API responses
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OpenOrder {
    pub order_id: u64,
    pub contract_id: u64,
    pub price: String,
    pub size: String,
    pub side: OrderSide,
    pub status: String,
    pub filled_size: String,
    pub remaining_size: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Fill {
    pub id: u64,
    pub order_id: u64,
    pub contract_id: u64,
    pub price: String,
    pub size: String,
    pub side: OrderSide,
    pub time: u64,
    pub fee: String,
    pub fee_asset_id: u64,
}
