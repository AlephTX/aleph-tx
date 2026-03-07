use aleph_tx::exchanges::edgex::signature::SignatureManager;
use std::time::Instant;

fn main() {
    let private_key = "023421824d933e7e9ed0159ec5902b183eee87fd1ea2dd32807a2d69e247ef57";

    println!("Creating SignatureManager...");
    let start = Instant::now();
    let sig_mgr = SignatureManager::new(private_key).unwrap();
    println!("Creation took: {:?}", start.elapsed());

    // Test parameters
    let synthetic_asset_id = "0x4554482d3900000000000000000000";
    let collateral_asset_id = "0x555344432d36000000000000000000";
    let fee_asset_id = collateral_asset_id;
    let is_buy = true;
    let amount_synthetic = 10_000_000u64;
    let amount_collateral = 15_000_000u64;
    let amount_fee = 5700u64;
    let nonce = 1234567890u64;
    let account_id = 573736952784748604u64;
    let expire_time_hours = 493911u64;

    println!("\nCalculating order hash...");
    let start = Instant::now();
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
    ).unwrap();
    println!("Hash calculation took: {:?}", start.elapsed());
    println!("Hash: 0x{:064x}", hash);

    println!("\nSigning hash...");
    let start = Instant::now();
    let signature = sig_mgr.sign_l2_action(hash).unwrap();
    println!("Signing took: {:?}", start.elapsed());
    println!("Signature: {}", signature);
}
