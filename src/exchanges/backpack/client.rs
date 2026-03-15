use super::model::*;
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

#[derive(Debug, Clone, serde::Deserialize)]
pub struct BackpackAccountStats {
    pub available_balance: f64,
    pub portfolio_value: f64,
    pub position: f64,
    pub leverage: f64,
    pub margin_usage: f64,
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

    pub async fn get_balances(&self) -> Result<std::collections::HashMap<String, BackpackBalance>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let params = serde_json::Map::new();
        let signature = self.generate_signature("balanceQuery", &params, timestamp, 5000);

        let mut headers = HeaderMap::new();
        headers.insert("X-API-Key", HeaderValue::from_str(&self.api_key)?);
        headers.insert(
            "X-Timestamp",
            HeaderValue::from_str(&timestamp.to_string())?,
        );
        headers.insert("X-Window", HeaderValue::from_static("5000"));
        headers.insert("X-Signature", HeaderValue::from_str(&signature)?);

        let url = format!("{}/api/v1/capital", self.base_url);
        let resp = self.client.get(&url).headers(headers).send().await?;

        if !resp.status().is_success() {
            let txt = resp.text().await?;
            return Err(anyhow!("Backpack get_balances error: {}", txt));
        }

        let json: Value = resp.json().await?;
        tracing::debug!("🔍 [BP] Raw balance response: {}", json);
        let mut balances = std::collections::HashMap::new();
        if let Some(obj) = json.as_object() {
            for (asset, data) in obj {
                if let Ok(b) = serde_json::from_value::<BackpackBalance>(data.clone()) {
                    balances.insert(asset.clone(), b);
                } else {
                    // Try parsing manually if nested different
                    let available = data
                        .get("available")
                        .and_then(|v| v.as_str())
                        .unwrap_or("0");
                    let locked = data.get("locked").and_then(|v| v.as_str()).unwrap_or("0");
                    balances.insert(
                        asset.clone(),
                        BackpackBalance {
                            symbol: asset.clone(),
                            available: available.to_string(),
                            locked: locked.to_string(),
                        },
                    );
                }
            }
        }
        Ok(balances)
    }

    pub async fn get_recent_fills(
        &self,
        symbol: &str,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<BackpackFill>> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let mut params = serde_json::Map::new();
        params.insert("symbol".to_string(), Value::String(symbol.to_string()));
        params.insert(
            "limit".to_string(),
            Value::Number(serde_json::Number::from(limit)),
        );
        params.insert(
            "offset".to_string(),
            Value::Number(serde_json::Number::from(offset)),
        );
        let signature = self.generate_signature("fillHistoryQueryAll", &params, timestamp, 5000);

        let mut headers = HeaderMap::new();
        headers.insert("X-API-Key", HeaderValue::from_str(&self.api_key)?);
        headers.insert(
            "X-Timestamp",
            HeaderValue::from_str(&timestamp.to_string())?,
        );
        headers.insert("X-Window", HeaderValue::from_static("5000"));
        headers.insert("X-Signature", HeaderValue::from_str(&signature)?);

        let url = format!(
            "{}/wapi/v1/history/fills?symbol={}&limit={}&offset={}",
            self.base_url, symbol, limit, offset
        );
        let resp = self.client.get(&url).headers(headers).send().await?;

        if !resp.status().is_success() {
            let txt = resp.text().await?;
            return Err(anyhow!("Backpack get_recent_fills error: {}", txt));
        }

        let json: Value = resp.json().await?;
        let fills: Vec<BackpackFill> = serde_json::from_value(json).unwrap_or_default();
        Ok(fills)
    }

    /// Get margin account collateral information (for perpetual trading)
    /// This returns the actual trading account equity, not just spot balances
    pub async fn get_collateral(&self) -> Result<f64> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
        let params = serde_json::Map::new();
        let signature = self.generate_signature("collateralQuery", &params, timestamp, 5000);

        let mut headers = HeaderMap::new();
        headers.insert("X-API-Key", HeaderValue::from_str(&self.api_key)?);
        headers.insert(
            "X-Timestamp",
            HeaderValue::from_str(&timestamp.to_string())?,
        );
        headers.insert("X-Window", HeaderValue::from_static("5000"));
        headers.insert("X-Signature", HeaderValue::from_str(&signature)?);

        let url = format!("{}/api/v1/capital/collateral", self.base_url);
        let resp = self.client.get(&url).headers(headers).send().await?;

        if !resp.status().is_success() {
            let txt = resp.text().await?;
            return Err(anyhow!("Backpack get_collateral error: {}", txt));
        }

        let json: Value = resp.json().await?;
        tracing::debug!("🔍 [BP] Collateral response: {}", json);

        // Extract netEquity from the response
        let net_equity = json
            .get("netEquity")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        Ok(net_equity)
    }

    /// Compute total account equity in USD by summing all non-zero spot balances
    /// and converting to USD using the public ticker API.
    /// Handles Backpack's unified cross-margin model where all spot assets = collateral.
    pub async fn get_total_equity(&self) -> Result<f64> {
        // First try to get collateral (margin account equity)
        if let Ok(collateral_equity) = self.get_collateral().await
            && collateral_equity > 0.0
        {
            tracing::debug!("🔍 [BP] Using collateral equity: ${:.2}", collateral_equity);
            return Ok(collateral_equity);
        }

        // Fallback to spot balances calculation
        let balances = self.get_balances().await?;
        let mut total_usd = 0.0_f64;

        for (symbol, bal) in &balances {
            let available: f64 = bal.available.parse().unwrap_or(0.0);
            let locked: f64 = bal.locked.parse().unwrap_or(0.0);
            let qty = available + locked;
            if qty < 0.001 {
                continue;
            }

            // Stablecoins are 1:1 USD
            if symbol == "USDC" || symbol == "USDT" {
                total_usd += qty;
                continue;
            }

            // Skip non-tradeable assets
            if symbol == "POINTS" {
                continue;
            }

            // Look up USD price via public ticker
            let ticker_symbol = format!("{}_USDC", symbol);
            let url = format!("{}/api/v1/ticker?symbol={}", self.base_url, ticker_symbol);
            if let Ok(resp) = self.client.get(&url).send().await
                && resp.status().is_success()
                && let Ok(json) = resp.json::<Value>().await
            {
                let last_price = json
                    .get("lastPrice")
                    .and_then(|v| v.as_str().and_then(|s| s.parse::<f64>().ok()))
                    .unwrap_or(0.0);
                if last_price > 0.0 {
                    let usd_value = qty * last_price;
                    if usd_value > 0.01 {
                        tracing::debug!(
                            "  [BP] {} {} × ${:.6} = ${:.2}",
                            qty,
                            symbol,
                            last_price,
                            usd_value
                        );
                    }
                    total_usd += usd_value;
                }
            }
        }

        tracing::debug!("🔍 [BP] Total equity: ${:.2}", total_usd);
        Ok(total_usd)
    }

    pub async fn get_account_stats(&self) -> Result<BackpackAccountStats> {
        let total_equity = self.get_total_equity().await?;
        let positions = self.get_open_positions().await?;
        
        // Sum position notional for leverage calculation
        let mut total_notional = 0.0;
        let mut main_pos = 0.0;
        for pos in positions {
            let qty: f64 = pos.quantity.parse().unwrap_or(0.0);
            total_notional += qty.abs(); // Simplistic: treats 1 unit as $1 for non-USD assets
            main_pos += qty;
        }

        Ok(BackpackAccountStats {
            available_balance: total_equity, // Backpack treats all spot as collateral
            portfolio_value: total_equity,
            position: main_pos,
            leverage: if total_equity > 0.0 { total_notional / total_equity } else { 0.0 },
            margin_usage: if total_equity > 0.0 { (total_notional / total_equity) / 20.0 } else { 0.0 }, // Assuming 20x max
        })
    }
}
