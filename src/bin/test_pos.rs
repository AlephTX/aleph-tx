use aleph_tx::edgex_api::signature::SignatureManager;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use std::time::{SystemTime, UNIX_EPOCH};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env_str =
        std::fs::read_to_string("/home/metaverse/.openclaw/workspace/aleph-tx/.env.edgex")?;
    let mut key = String::new();
    let mut account_id = 0u64;

    for line in env_str.lines() {
        if let Some(rest) = line.strip_prefix("EDGEX_ACCOUNT_ID=") {
            account_id = rest.trim().parse().unwrap_or(0);
        }
        if let Some(rest) = line.strip_prefix("EDGEX_STARK_PRIVATE_KEY=") {
            key = rest.trim().to_string();
        }
    }

    let sig_manager = SignatureManager::new(&key).unwrap();
    let client = reqwest::Client::builder().build()?;

    let paths = vec![
        "/api/v1/private/order/getHistoryOrderFillTransactionPage",
        "/api/v1/private/order/getActiveOrderPage",
    ];

    for path in paths {
        let url = format!("https://pro.edgex.exchange{}", path);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis()
            .to_string();

        // Exact same as Python SDK: timestamp + method + path + queryString
        let query_str = format!("accountId={}", account_id);
        let sign_payload = format!("{}GET{}{}", timestamp, path, query_str);

        println!("Sign Payload: {}", sign_payload);
        let sig = sig_manager.sign_message(&sign_payload).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(
            "X-edgeX-Api-Timestamp",
            HeaderValue::from_str(&timestamp).unwrap(),
        );
        headers.insert(
            "X-edgeX-Api-Signature",
            HeaderValue::from_str(sig.trim_start_matches("0x")).unwrap(),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let res = client
            .get(&url)
            .query(&[("accountId", account_id.to_string())])
            .headers(headers)
            .send()
            .await?;
        println!("GET {} -> status: {}", path, res.status());
        println!("Body: {}", res.text().await?.get(..300).unwrap_or(""));
        println!("---");
    }

    Ok(())
}
