use crate::edgex_api::model::CreateOrderRequest;
use crate::edgex_api::signature::SignatureManager;
use reqwest::header::{HeaderMap, HeaderValue, CONTENT_TYPE};
use reqwest::Client;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const BASE_URL: &str = "https://pro.edgex.exchange";

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("Request error: {0}")]
    RequestError(#[from] reqwest::Error),
    #[error("Signature error: {0}")]
    SignatureError(#[from] crate::edgex_api::signature::SignatureError),
    #[error("API error: {0}")]
    ApiError(String),
}

pub struct EdgeXClient {
    client: Client,
    pub signature_manager: SignatureManager,
    base_url: String,
}

impl EdgeXClient {
    pub fn new(private_key: &str, base_url: Option<String>) -> Result<Self, ClientError> {
        let signature_manager = SignatureManager::new(private_key)?;
        let client = Client::builder().build()?;
        let base_url = base_url.unwrap_or_else(|| BASE_URL.to_string());

        Ok(Self {
            client,
            signature_manager,
            base_url,
        })
    }

    fn build_sign_content(timestamp: &str, method: &str, path: &str, body_val: &Value) -> String {
        fn get_value(val: &Value) -> String {
            match val {
                Value::Null => "".to_string(),
                Value::Bool(b) => if *b { "true".to_string() } else { "false".to_string() },
                Value::Number(n) => n.to_string(),
                Value::String(s) => s.clone(),
                Value::Array(arr) => {
                    let values: Vec<String> = arr.iter().map(get_value).collect();
                    values.join("&")
                }
                Value::Object(obj) => {
                    let mut keys: Vec<&String> = obj.keys().collect();
                    keys.sort();
                    let pairs: Vec<String> = keys.iter().map(|k| format!("{}={}", k, get_value(&obj[*k]))).collect();
                    pairs.join("&")
                }
            }
        }
        
        // According to EdgeX Python SDK:
        // sign_content = f"{timestamp}{method}{path}{body_str}"
        let body_str = get_value(body_val);
        format!("{}{}{}{}", timestamp, method, path, body_str)
    }

    pub async fn create_order(&self, req: &CreateOrderRequest) -> Result<Value, ClientError> {
        let url = format!("{}/api/v1/private/order/createOrder", self.base_url);
        
        // TODO: The request object 'req' should already have l2Signature populated, 
        // OR we should sign it here.
        // For now, assuming caller or a builder helper handles signing before passing here, 
        // or we clone and sign here.
        
        // Let's assume we implement a helper to sign and create the request.
        // But for this raw method, we take the request as is.
        
        let body = serde_json::to_string(req).map_err(|e| ClientError::ApiError(e.to_string()))?;
        let body_val: Value = serde_json::to_value(req).unwrap();
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().to_string();
        
        let path = "/api/v1/private/order/createOrder";
        let sign_payload = Self::build_sign_content(&timestamp, "POST", path, &body_val);
        tracing::debug!("CreateOrder Sign Payload: {}", sign_payload);
        
        let header_signature = self.signature_manager.sign_message(&sign_payload)?;

        let mut headers = HeaderMap::new();
        headers.insert("X-edgeX-Api-Timestamp", HeaderValue::from_str(&timestamp).unwrap());
        headers.insert("X-edgeX-Api-Signature", HeaderValue::from_str(header_signature.trim_start_matches("0x")).unwrap());
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let res = self.client.post(&url)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        let status = res.status();
        if !status.is_success() {
            let text = res.text().await?;
            return Err(ClientError::ApiError(format!("Status: {}, Body: {}", status, text)));
        }

        let json: Value = res.json().await?;
        Ok(json)
    }

    pub async fn cancel_order(&self, req: &crate::edgex_api::model::CancelOrderRequest) -> Result<Value, ClientError> {
        let url = format!("{}/api/v1/private/order/cancelOrderById", self.base_url);
        // Uses same Header auth mechanism
        
        let body = serde_json::to_string(req).map_err(|e| ClientError::ApiError(e.to_string()))?;
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().to_string();
        let path = "/api/v1/private/order/cancelOrderById";
        
        let sign_payload = format!("{}{}{}{}", timestamp, "POST", path, body);
        let header_signature = self.signature_manager.sign_message(&sign_payload)?;

        let mut headers = HeaderMap::new();
        headers.insert("X-edgeX-Api-Timestamp", HeaderValue::from_str(&timestamp).unwrap());
        headers.insert("X-edgeX-Api-Signature", HeaderValue::from_str(header_signature.trim_start_matches("0x")).unwrap());
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let res = self.client.post(&url)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        let status = res.status();
        if !status.is_success() {
            let text = res.text().await?;
            return Err(ClientError::ApiError(format!("Status: {}, Body: {}", status, text)));
        }

        let json: Value = res.json().await?;
        Ok(json)
    }

    pub async fn cancel_all_orders(&self, req: &crate::edgex_api::model::CancelAllOrderRequest) -> Result<Value, ClientError> {
        let url = format!("{}/api/v1/private/order/cancelAllOrder", self.base_url);
        
        // EdgeX cancelAllOrder does not require l2_signature in the body, just the HTTP header signature.
        let body = serde_json::to_string(req).map_err(|e| ClientError::ApiError(e.to_string()))?;
        let body_val: Value = serde_json::to_value(req).unwrap();
        let timestamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis().to_string();
        let path = "/api/v1/private/order/cancelAllOrder";
        
        let sign_payload = Self::build_sign_content(&timestamp, "POST", path, &body_val);
        tracing::debug!("CancelAllOrder Sign Payload: {}", sign_payload);
        let header_signature = self.signature_manager.sign_message(&sign_payload)?;

        let mut headers = HeaderMap::new();
        headers.insert("X-edgeX-Api-Timestamp", HeaderValue::from_str(&timestamp).unwrap());
        headers.insert("X-edgeX-Api-Signature", HeaderValue::from_str(header_signature.trim_start_matches("0x")).unwrap());
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let res = self.client.post(&url)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        let status = res.status();
        if !status.is_success() {
            let text = res.text().await?;
            return Err(ClientError::ApiError(format!("Status: {}, Body: {}", status, text)));
        }

        let json: Value = res.json().await?;
        Ok(json)
    }

    pub async fn get_open_orders(&self, account_id: u64) -> Result<Vec<crate::edgex_api::model::OpenOrder>, ClientError> {
        let url = format!("{}/api/v1/private/order/getOpenOrders", self.base_url);
        let params = [("accountId", account_id.to_string())];
        
        // GET request with query params
        // Header signature usually requires Path + QueryString? 
        // Or strictly Request Body?
        // Docs usually specify. For now assuming timestamp+method+path+query OR just path.
        // If GET, body is empty.
        
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().to_string();
        let header_signature = "0x0000000000000000000000000000000000000000".to_string(); // Temporary

        let mut headers = HeaderMap::new();
        headers.insert("X-edgeX-Api-Timestamp", HeaderValue::from_str(&timestamp).unwrap());
        headers.insert("X-edgeX-Api-Signature", HeaderValue::from_str(header_signature.trim_start_matches("0x")).unwrap());

        let res = self.client.get(&url)
            .headers(headers)
            .query(&params)
            .send()
            .await?;

        let status = res.status();
        if !status.is_success() {
            let text = res.text().await?;
            return Err(ClientError::ApiError(format!("Status: {}, Body: {}", status, text)));
        }

        // Response structure might be { "code": "...", "data": [...] }
        // We'll parse Value first then generic.
        let json: Value = res.json().await?;
        // Assuming "data" field contains list, or root is list.
        // Need to check docs for response format.
        // Usually "data": [ ... ]
        if let Some(data) = json.get("data") {
             let orders: Vec<crate::edgex_api::model::OpenOrder> = serde_json::from_value(data.clone()).map_err(|e| ClientError::ApiError(e.to_string()))?;
             Ok(orders)
        } else {
             // Fallback if root is array
             let orders: Vec<crate::edgex_api::model::OpenOrder> = serde_json::from_value(json).map_err(|e| ClientError::ApiError(e.to_string()))?;
             Ok(orders)
        }
    }

    pub async fn get_fills(&self, account_id: u64) -> Result<Vec<crate::edgex_api::model::Fill>, ClientError> {
        let url = format!("{}/api/v1/private/order/getFills", self.base_url);
        let params = [("accountId", account_id.to_string())];
        
        // Similar GET auth pattern
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis().to_string();
        let header_signature = "0x0000000000000000000000000000000000000000".to_string(); // Temporary

        let mut headers = HeaderMap::new();
        headers.insert("X-edgeX-Api-Timestamp", HeaderValue::from_str(&timestamp).unwrap());
        headers.insert("X-edgeX-Api-Signature", HeaderValue::from_str(header_signature.trim_start_matches("0x")).unwrap());

        let res = self.client.get(&url)
            .headers(headers)
            .query(&params)
            .send()
            .await?;

        let status = res.status();
        if !status.is_success() {
            let text = res.text().await?;
            return Err(ClientError::ApiError(format!("Status: {}, Body: {}", status, text)));
        }

        let json: Value = res.json().await?;
        if let Some(data) = json.get("data") {
             let fills: Vec<crate::edgex_api::model::Fill> = serde_json::from_value(data.clone()).map_err(|e| ClientError::ApiError(e.to_string()))?;
             Ok(fills)
        } else {
             let fills: Vec<crate::edgex_api::model::Fill> = serde_json::from_value(json).map_err(|e| ClientError::ApiError(e.to_string()))?;
             Ok(fills)
        }
    }
}
