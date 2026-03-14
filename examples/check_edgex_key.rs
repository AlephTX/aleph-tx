use starknet_crypto::{Felt, get_public_key};

fn main() {
    let private_key_hex = "023421824d933e7e9ed0159ec5902b183eee87fd1ea2dd32807a2d69e247ef57";
    let private_key = Felt::from_hex(private_key_hex).expect("Invalid private key");

    let public_key = get_public_key(&private_key);

    println!("Private Key: {}", private_key_hex);
    println!("Public Key (x): 0x{:064x}", public_key);
}
