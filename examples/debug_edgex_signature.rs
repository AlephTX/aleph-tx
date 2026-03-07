use aleph_tx::exchanges::edgex::signature::SignatureManager;
use sha3::{Digest, Keccak256};

fn main() {
    // Test data from log
    let sign_content = "1772894594396POST/api/v1/private/order/createOrderaccountId=573736952784748604&clientOrderId=34562801-7bcd-4264-86fa-b7af8318497b&contractId=10000002&expireTime=1777214594392&l2ExpireTime=1778078594392&l2LimitFee=1&l2Nonce=1649419978&l2Signature=07921467034f7094edd603ff7513dca43fc9fedd475354b553f7c5f059c6618a0358e1166819f5b640bbdf1053331f15860c7744a15f5e9aff212b5dd2b9773b&l2Size=0.0100&l2Value=15.000000&price=1500.00&reduceOnly=false&side=BUY&size=0.0100&timeInForce=POST_ONLY&type=LIMIT";

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
    println!("Expected:  014cef0a08746e2ea9e347dd31cd41070e83c881f655f946977871b7f52f45d5018b2d08897654cc8957d11b1edaad6d405174e5648ee39e47a06e4e4a725970");
}
