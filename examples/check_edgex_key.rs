use starknet_crypto::{Felt, get_public_key};

fn main() {
    let private_key_hex = "REDACTED_PRIVATE_KEY";
    let private_key = Felt::from_hex(private_key_hex).expect("Invalid private key");

    let public_key = get_public_key(&private_key);

    println!("Private Key: {}", private_key_hex);
    println!("Public Key (x): 0x{:064x}", public_key);
}
