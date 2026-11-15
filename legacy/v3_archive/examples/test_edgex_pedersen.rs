use aleph_tx::exchanges::edgex::pedersen::PedersenHash;
use num_bigint::BigUint;
use num_traits::Num;

fn main() {
    let hasher = PedersenHash::new();

    // Test case: Hash ETH-9 and USDC-6
    let synthetic = BigUint::from_str_radix("4554482d3900000000000000000000", 16).unwrap();
    let collateral = BigUint::from_str_radix("555344432d36000000000000000000", 16).unwrap();

    // For buy order: sell collateral, buy synthetic
    let hash1 = hasher.hash(&[collateral.clone(), synthetic.clone()]);
    println!("Pedersen hash (collateral, synthetic): 0x{:064x}", hash1);
    println!("Hash (decimal): {}", hash1);

    // Expected from Python SDK:
    // 0x04d55362f72cd6560c053d8d39ffd5f0e7776c1f5fecfdcbf1d6027020acf7b9
    let expected = BigUint::from_str_radix(
        "04d55362f72cd6560c053d8d39ffd5f0e7776c1f5fecfdcbf1d6027020acf7b9",
        16,
    )
    .unwrap();

    if hash1 == expected {
        println!("✅ Hash matches Python SDK!");
    } else {
        println!("❌ Hash does NOT match Python SDK");
        println!("Expected: 0x{:064x}", expected);
    }
}
