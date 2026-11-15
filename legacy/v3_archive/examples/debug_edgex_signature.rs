use aleph_tx::exchanges::edgex::signature::SignatureManager;
use sha3::{Digest, Keccak256};

fn main() -> anyhow::Result<()> {
    // Synthetic create-order payload for offline REST signature diagnostics.
    let content = format!(
        "1000POST/api/v1/private/order/createOrderaccountId=1&clientOrderId=fixture-1&contractId=10000002&l2Signature={}&price=1500.00&side=BUY&size=0.0100&timeInForce=POST_ONLY&type=LIMIT",
        "0".repeat(128)
    );
    let signature = SignatureManager::new("0x1")?.sign_message(&content)?;
    println!(
        "Keccak256 Hash: {}",
        hex::encode(Keccak256::digest(&content))
    );
    println!("Fixture signature: {signature}");
    Ok(())
}
