use starknet_crypto::{sign, Felt};
use sha3::{Digest, Keccak256};

fn main() {
    let private_key_hex = "023421824d933e7e9ed0159ec5902b183eee87fd1ea2dd32807a2d69e247ef57";
    let private_key = Felt::from_hex(private_key_hex).expect("Invalid private key");

    // Test message
    let message = "1772894985474GET/api/v1/private/account/getAccountAssetaccountId=573736952784748604";

    // Hash with Keccak256
    let mut hasher = Keccak256::new();
    hasher.update(message.as_bytes());
    let hash_bytes = hasher.finalize();

    let mut bytes_32 = [0u8; 32];
    bytes_32.copy_from_slice(&hash_bytes);
    let hash_felt = Felt::from_bytes_be(&bytes_32);

    println!("Message: {}", message);
    println!("Keccak256 Hash (hex): {}", hex::encode(&bytes_32));
    println!("Keccak256 Hash (bytes): {:?}", &bytes_32[..8]);
    println!("Hash as Felt: 0x{:064x}", hash_felt);
    println!("Felt as bytes: {:?}", &hash_felt.to_bytes_be()[..8]);

    // Sign
    let signature = sign(&private_key, &hash_felt, &Felt::from_hex("0x1").unwrap()).unwrap();

    println!("\nSignature r: 0x{:064x}", signature.r);
    println!("Signature s: 0x{:064x}", signature.s);
    println!("\nCombined (r+s): {:064x}{:064x}", signature.r, signature.s);
}
