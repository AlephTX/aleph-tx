use aleph_tx::exchanges::edgex::signature::SignatureManager;
use starknet_crypto::{Felt, pedersen_hash};
use std::env;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== EdgeX L2 Signature Diagnostic Tool ===\n");

    // Load private key
    let private_key = env::var("EDGEX_STARK_PRIVATE_KEY")?;
    let sig_mgr = SignatureManager::new(&private_key)?;

    // Test parameters
    let synthetic_asset_id = "0x4554482d3900000000000000000000";
    let collateral_asset_id = "0x555344432d36000000000000000000";
    let fee_asset_id = "0x555344432d36000000000000000000";
    let is_buy = true;
    let amount_synthetic = 10000000u64;
    let amount_collateral = 15000000u64;
    let amount_fee = 5700u64;
    let nonce = 1234567890u64;
    let account_id = 573736952784748604u64;
    let expire_time_hours = 493911u64;

    println!("Input Parameters:");
    println!("  synthetic_asset_id: {}", synthetic_asset_id);
    println!("  collateral_asset_id: {}", collateral_asset_id);
    println!("  fee_asset_id: {}", fee_asset_id);
    println!("  is_buy: {}", is_buy);
    println!("  amount_synthetic: {}", amount_synthetic);
    println!("  amount_collateral: {}", amount_collateral);
    println!("  amount_fee: {}", amount_fee);
    println!("  nonce: {}", nonce);
    println!("  account_id: {}", account_id);
    println!("  expire_time_hours: {}\n", expire_time_hours);

    // Parse asset IDs
    let syn_id = Felt::from_hex(synthetic_asset_id.trim_start_matches("0x"))?;
    let col_id = Felt::from_hex(collateral_asset_id.trim_start_matches("0x"))?;
    let fee_id = Felt::from_hex(fee_asset_id.trim_start_matches("0x"))?;

    println!("Parsed Asset IDs (as Felt):");
    println!("  syn_id:  0x{:064x}", syn_id);
    println!("  col_id:  0x{:064x}", col_id);
    println!("  fee_id:  0x{:064x}\n", fee_id);

    // Determine sell/buy based on direction
    let (asset_id_sell, asset_id_buy, amount_sell, amount_buy) = if is_buy {
        (col_id, syn_id, amount_collateral, amount_synthetic)
    } else {
        (syn_id, col_id, amount_synthetic, amount_collateral)
    };

    println!("Order Direction (is_buy={}):", is_buy);
    println!("  asset_id_sell: 0x{:064x}", asset_id_sell);
    println!("  asset_id_buy:  0x{:064x}", asset_id_buy);
    println!("  amount_sell:   {}", amount_sell);
    println!("  amount_buy:    {}\n", amount_buy);

    // Step 1: hash(asset_id_sell, asset_id_buy)
    let hash1 = pedersen_hash(&asset_id_sell, &asset_id_buy);
    println!("Step 1 - Pedersen Hash:");
    println!("  hash(asset_id_sell, asset_id_buy)");
    println!("  = 0x{:064x}\n", hash1);

    // Step 2: hash(hash1, asset_id_fee)
    let hash2 = pedersen_hash(&hash1, &fee_id);
    println!("Step 2 - Pedersen Hash:");
    println!("  hash(hash1, asset_id_fee)");
    println!("  = 0x{:064x}\n", hash2);

    // Step 3: Pack message 0
    let shift_add = |acc: Felt, val: u64, shift: u32| -> Felt {
        let shift_multiplier = Felt::from(2u64).pow(shift as u128);
        (acc * shift_multiplier) + Felt::from(val)
    };

    let pm0 = Felt::from(amount_sell);
    let pm0 = shift_add(pm0, amount_buy, 64);
    let pm0 = shift_add(pm0, amount_fee, 64);
    let pm0 = shift_add(pm0, nonce, 32);

    println!("Step 3 - Pack Message 0:");
    println!("  amount_sell << 64 + amount_buy << 64 + amount_fee << 32 + nonce");
    println!("  = 0x{:064x}\n", pm0);

    // Step 4: hash(hash2, packed_message0)
    let hash3 = pedersen_hash(&hash2, &pm0);
    println!("Step 4 - Pedersen Hash:");
    println!("  hash(hash2, packed_message0)");
    println!("  = 0x{:064x}\n", hash3);

    // Step 5: Pack message 1
    let limit_order_type = 3u64;
    let pm1 = Felt::from(limit_order_type);
    let pm1 = shift_add(pm1, account_id, 64);
    let pm1 = shift_add(pm1, account_id, 64);
    let pm1 = shift_add(pm1, account_id, 64);
    let pm1 = shift_add(pm1, expire_time_hours, 32);
    let shift_17 = Felt::from(2u64).pow(17u128);
    let pm1 = pm1 * shift_17;

    println!("Step 5 - Pack Message 1:");
    println!("  (((3 << 64 + account_id) << 64 + account_id) << 64 + account_id) << 32 + expire_time) << 17");
    println!("  = 0x{:064x}\n", pm1);

    // Step 6: Final hash
    let final_hash = pedersen_hash(&hash3, &pm1);
    println!("Step 6 - Final Pedersen Hash:");
    println!("  hash(hash3, packed_message1)");
    println!("  = 0x{:064x}\n", final_hash);

    // Sign the hash
    let signature = sig_mgr.sign_l2_action(final_hash)?;
    println!("L2 Signature:");
    println!("  {}\n", signature);

    println!("=== Diagnostic Complete ===");
    println!("\nPlease compare these hash values with the Go/Python SDK output.");
    println!("If any hash differs, the Pedersen hash implementation is incompatible.");

    Ok(())
}
