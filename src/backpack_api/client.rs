use crate::backpack_api::model::*;
use anyhow::{Context, Result, anyhow};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use ed25519_dalek::{Signer, SigningKey};
use reqwest::Client;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct BackpackClient {
    client: Client,
    api_key: String,
    base_url: String,
    signing_key: SigningKey,
}

impl BackpackClient {
    pub fn new(api_key: &str, api_secret_b64: &str, base_url: &str) -> Result<Self> {
        let secret_bytes = BASE64
            .decode(api_secret_b64)
            .context("Failed to decode backpack API secret from base64")?;

        let signing_key = if secret_bytes.len() == 32 {
            SigningKey::from_bytes(secret_bytes.as_slice().try_into().unwrap())
        } else if secret_bytes.len() == 64 {
            let mut arr = [0u8; 32];
            arr.copy_from_slice(&secret_bytes[..32]); // Take seed only
            SigningKey::from_bytes(&arr)
        } else {
            return Err(anyhow!("Invalid Ed25519 private key length"));
        };

        Ok(Self {
            client: Client::builder().build()?,
            api_key: api_key.to_string(),
            base_url: base_url.to_string(),
            signing_key,
        })
    }

    fn generate_signature(
        &self,
        instruction: &str,
        params: &serde_json::Map<String, Value>,
        timestamp: u128,
        window: u32,
    ) -> String {
        let mut sorted_keys: Vec<&String> = params.keys().collect();
        sorted_keys.sort();

        let mut query_parts = vec![];
        query_parts.push(format!("instruction={}", instruction));

        for k in sorted_keys {
            if let Some(v) = params.get(k) {
                let val_str = match v {
                    Value::String(s) => s.to_string(),
                    Value::Bool(b) => b.to_string().to_lowercase(),
                    Value::Number(n) => n.to_string(),
                    _ => v.to_string(),
                };
                query_parts.push(format!("{}={}", k, val_str));
            }
        }

        query_parts.push(format!("timestamp={}", timestamp));
        query_parts.push(format!("window={}", window));

        let sign_string = query_parts.join("&");
        // tracing::debug!("Backpack Sign Payload: {}", sign_string);

        let signature = self.signing_key.sign(sign_string.as_bytes());
        BASE64.encode(signature.to_bytes())
    }

    pub async fn get_open_positions(&self) -> Result<Vec<BackpackPosition>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let params = serde_json::Map::new();
        let signature = self.generate_signature("positionQuery", &params, timestamp, 5000);

        let mut headers = HeaderMap::new();
        headers.insert("X-API-Key", HeaderValue::from_str(&self.api_key)?);
        headers.insert(
            "X-Timestamp",
            HeaderValue::from_str(&timestamp.to_string())?,
        );
        headers.insert("X-Window", HeaderValue::from_static("5000"));
        headers.insert("X-Signature", HeaderValue::from_str(&signature)?);

        let url = format!("{}/api/v1/position", self.base_url);
        let resp = self.client.get(&url).headers(headers).send().await?;

        if !resp.status().is_success() {
            let txt = resp.text().await?;
            return Err(anyhow!("Backpack get_open_positions error: {}", txt));
        }

        let json: Value = resp.json().await?;
        if json.as_array().is_some() {
            let positions: Vec<BackpackPosition> = serde_json::from_value(json).unwrap_or_default();
            Ok(positions)
        } else if let Some(data) = json.get("data") {
            let positions: Vec<BackpackPosition> =
                serde_json::from_value(data.clone()).unwrap_or_default();
            Ok(positions)
        } else {
            Ok(vec![])
        }
    }

    pub async fn create_order(
        &self,
        order: &BackpackOrderRequest,
    ) -> Result<BackpackOrderResponse> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();

        let mut params_map = serde_json::Map::new();
        let body_val = serde_json::to_value(order)?;
        if let Value::Object(m) = body_val {
            params_map = m.clone();
        }

        let signature = self.generate_signature("orderExecute", &params_map, timestamp, 5000);

        let mut headers = HeaderMap::new();
        headers.insert("X-API-Key", HeaderValue::from_str(&self.api_key)?);
        headers.insert(
            "X-Timestamp",
            HeaderValue::from_str(&timestamp.to_string())?,
        );
        headers.insert("X-Window", HeaderValue::from_static("5000"));
        headers.insert("X-Signature", HeaderValue::from_str(&signature)?);
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );

        let url = format!("{}/api/v1/order", self.base_url);

        // Backpack strict req: send JSON exactly matching map
        let resp = self
            .client
            .post(&url)
            .headers(headers)
            .json(&params_map)
            .send()
            .await?;

        if !resp.status().is_success() {
            let txt = resp.text().await?;
            return Err(anyhow!("Backpack create_order error: {}", txt));
        }

        let ok_resp: BackpackOrderResponse = resp.json().await?;
        Ok(ok_resp)
    }

    pub async fn cancel_all_orders(&self, symbol: &str) -> Result<()> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();

        let mut params = serde_json::Map::new();
        params.insert("symbol".to_string(), Value::String(symbol.to_string()));

        let signature = self.generate_signature("orderCancelAll", &params, timestamp, 5000);

        let mut headers = HeaderMap::new();
        headers.insert("X-API-Key", HeaderValue::from_str(&self.api_key)?);
        headers.insert(
            "X-Timestamp",
            HeaderValue::from_str(&timestamp.to_string())?,
        );
        headers.insert("X-Window", HeaderValue::from_static("5000"));
        headers.insert("X-Signature", HeaderValue::from_str(&signature)?);
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );

        let url = format!("{}/api/v1/orders", self.base_url);
        let resp = self
            .client
            .delete(&url)
            .headers(headers)
            .json(&params)
            .send()
            .await?;

        if !resp.status().is_success() {
            let txt = resp.text().await?;
            return Err(anyhow!("Backpack cancel_all_orders error: {}", txt));
        }

        Ok(())
    }
}
