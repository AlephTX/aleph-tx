use num_bigint::BigUint;
use starknet_crypto::{Felt, pedersen_hash};

fn main() {
    // Test case from Go SDK
    // Hash of two simple values
    let a = Felt::from_hex("0x4554482d3900000000000000000000").unwrap(); // ETH-9
    let b = Felt::from_hex("0x555344432d36000000000000000000").unwrap(); // USDC-6

    let hash = pedersen_hash(&a, &b);
    println!("Pedersen hash result: 0x{:064x}", hash);

    // Also test with the values from our order
    let synthetic = Felt::from_hex("0x4554482d3900000000000000000000").unwrap();
    let collateral = Felt::from_hex("0x555344432d36000000000000000000").unwrap();

    let hash1 = pedersen_hash(&synthetic, &collateral);
    println!("Hash 1 (sell=collateral, buy=synthetic): 0x{:064x}", hash1);

    // Convert to BigUint to see the decimal value
    let hash_bytes = hash1.to_bytes_be();
    let hash_int = BigUint::from_bytes_be(&hash_bytes);
    println!("Hash 1 (decimal): {}", hash_int);
}
