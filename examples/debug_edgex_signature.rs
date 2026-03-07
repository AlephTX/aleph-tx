use aleph_tx::exchanges::edgex::signature::SignatureManager;
use sha3::{Digest, Keccak256};

fn main() {
    // Test data from log
    let sign_content = "REDACTED_PRIVATE_REQUEST";

    println!("Sign Content: {}", sign_content);
    println!("Sign Content Length: {}", sign_content.len());

    // Hash with Keccak256
    let mut hasher = Keccak256::new();
    hasher.update(sign_content.as_bytes());
    let hash_bytes = hasher.finalize();
    println!("Keccak256 Hash: {}", hex::encode(&hash_bytes));

    // Load private key from env
    dotenv::from_filename(".env.edgex").ok();
    let private_key = std::env::var("EDGEX_STARK_PRIVATE_KEY")
        .expect("EDGEX_STARK_PRIVATE_KEY not set");

    println!("Private Key: {}", private_key);

    // Sign
    let sig_manager = SignatureManager::new(&private_key).unwrap();
    let signature = sig_manager.sign_message(sign_content).unwrap();

    println!("Signature: {}", signature);
    println!("Expected:  REDACTED_PRIVATE_SIGNATURE");
}
