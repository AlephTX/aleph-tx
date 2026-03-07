use aleph_tx::exchanges::edgex::pedersen::PedersenHash;
use num_bigint::BigUint;
use num_traits::Num;
use std::time::Instant;

fn main() {
    println!("Initializing Pedersen hasher...");
    let start = Instant::now();
    let hasher = PedersenHash::new();
    println!("Initialization took: {:?}", start.elapsed());

    // Test case: Hash ETH-9 and USDC-6
    let synthetic = BigUint::from_str_radix("4554482d3900000000000000000000", 16).unwrap();
    let collateral = BigUint::from_str_radix("555344432d36000000000000000000", 16).unwrap();

    println!("\nComputing hash...");
    let start = Instant::now();
    let hash1 = hasher.hash(&[collateral.clone(), synthetic.clone()]);
    println!("Hash computation took: {:?}", start.elapsed());
    println!("Result: 0x{:064x}", hash1);
}
