use starknet_crypto::Felt;
use num_bigint::BigUint;
use num_traits::Num;

fn main() {
    // Stark prime
    let stark_prime = BigUint::from_str_radix(
        "800000000000011000000000000000000000000000000000000000000000001",
        16,
    ).unwrap();

    println!("Stark Prime: 0x{:x}", stark_prime);
    println!("Stark Prime bits: {}", stark_prime.bits());

    // Test packed_message0
    let amount_sell = 15000000u64; // 15 USDC
    let amount_buy = 10000000u64;  // 0.01 ETH
    let amount_fee = 5700u64;
    let nonce = 1234567890u64;

    // Calculate using BigUint (no modulo)
    let mut pm0 = BigUint::from(amount_sell);
    pm0 <<= 64;
    pm0 += amount_buy;
    pm0 <<= 64;
    pm0 += amount_fee;
    pm0 <<= 32;
    pm0 += nonce;

    println!("\nPacked Message 0 (BigUint): 0x{:x}", pm0);
    println!("PM0 bits: {}", pm0.bits());
    println!("PM0 < Stark Prime: {}", pm0 < stark_prime);

    // Calculate using Felt
    let shift_add = |acc: Felt, val: u64, shift: u32| -> Felt {
        let shift_multiplier = Felt::from(2u64).pow(shift as u128);
        (acc * shift_multiplier) + Felt::from(val)
    };

    let pm0_felt = Felt::from(amount_sell);
    let pm0_felt = shift_add(pm0_felt, amount_buy, 64);
    let pm0_felt = shift_add(pm0_felt, amount_fee, 64);
    let pm0_felt = shift_add(pm0_felt, nonce, 32);

    println!("Packed Message 0 (Felt): 0x{:064x}", pm0_felt);

    // Test packed_message1
    let limit_order_type = 3u64;
    let account_id = 573736952784748604u64;
    let expire_time = 493911u64;

    let mut pm1 = BigUint::from(limit_order_type);
    pm1 <<= 64;
    pm1 += account_id;
    pm1 <<= 64;
    pm1 += account_id;
    pm1 <<= 64;
    pm1 += account_id;
    pm1 <<= 32;
    pm1 += expire_time;
    pm1 <<= 17;

    println!("\nPacked Message 1 (BigUint): 0x{:x}", pm1);
    println!("PM1 bits: {}", pm1.bits());
    println!("PM1 < Stark Prime: {}", pm1 < stark_prime);

    let pm1_felt = Felt::from(limit_order_type);
    let pm1_felt = shift_add(pm1_felt, account_id, 64);
    let pm1_felt = shift_add(pm1_felt, account_id, 64);
    let pm1_felt = shift_add(pm1_felt, account_id, 64);
    let pm1_felt = shift_add(pm1_felt, expire_time, 32);
    let shift_17 = Felt::from(2u64).pow(17u128);
    let pm1_felt = pm1_felt * shift_17;

    println!("Packed Message 1 (Felt): 0x{:064x}", pm1_felt);
}
