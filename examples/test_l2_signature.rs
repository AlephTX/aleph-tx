use aleph_tx::exchanges::edgex::signature::SignatureManager;
use std::env;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Load private key from env
    let private_key = env::var("EDGEX_STARK_PRIVATE_KEY").expect("EDGEX_STARK_PRIVATE_KEY not set");

    let sig_mgr = SignatureManager::new(&private_key)?;

    // Test parameters (same as before)
    let synthetic_asset_id = "0x4554482d3900000000000000000000";
    let collateral_asset_id = "0x555344432d36000000000000000000";
    let fee_asset_id = "0x555344432d36000000000000000000";
    let is_buy = true;
    let amount_synthetic = 10000000u64; // 0.01 ETH
    let amount_collateral = 15000000u64; // 15 USDC
    let amount_fee = 5700u64; // 0.0057 USDC
    let nonce = 1234567890u64;
    let account_id = 573736952784748604u64;
    let expire_time_hours = 493911u64;

    println!("=== L2 Order Signature Test ===\n");
    println!("Parameters:");
    println!("  synthetic_asset: {}", synthetic_asset_id);
    println!("  collateral_asset: {}", collateral_asset_id);
    println!("  is_buy: {}", is_buy);
    println!("  amount_synthetic: {}", amount_synthetic);
    println!("  amount_collateral: {}", amount_collateral);
    println!("  amount_fee: {}", amount_fee);
    println!("  nonce: {}", nonce);
    println!("  account_id: {}", account_id);
    println!("  expire_time_hours: {}\n", expire_time_hours);

    // Calculate hash
    let hash = sig_mgr.calc_limit_order_hash(
        synthetic_asset_id,
        collateral_asset_id,
        fee_asset_id,
        is_buy,
        amount_synthetic,
        amount_collateral,
        amount_fee,
        nonce,
        account_id,
        expire_time_hours,
    )?;

    println!("L2 Hash: 0x{:064x}\n", hash);

    // Sign
    let signature = sig_mgr.sign_l2_action(hash)?;

    println!("L2 Signature: {}", signature);
    println!("Signature length: {} chars", signature.len());

    Ok(())
}
