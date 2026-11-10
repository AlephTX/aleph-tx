use aleph_tx::edgex_api::client::EdgeXClient;
use anyhow::{Result, anyhow, ensure};
use std::env;

#[tokio::main]
async fn main() -> Result<()> {
    // This command performs an authenticated read against EdgeX.
    dotenv::from_filename(".env.edgex").ok();
    let account_id = env::var("EDGEX_ACCOUNT_ID")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| anyhow!("Set EDGEX_ACCOUNT_ID to the numeric account ID"))?;
    ensure!(account_id > 0, "EDGEX_ACCOUNT_ID must be positive");
    let key = env::var("EDGEX_STARK_PRIVATE_KEY")
        .map_err(|_| anyhow!("Set EDGEX_STARK_PRIVATE_KEY in the local environment"))?;
    let client = EdgeXClient::new(&key, None)?;
    client
        .get_balances(account_id)
        .await
        .map_err(|_| anyhow!("EdgeX authenticated balance request failed"))?;
    println!("EdgeX balance request completed");
    Ok(())
}
