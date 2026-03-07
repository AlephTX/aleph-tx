use starknet_crypto::Felt;

fn main() {
    let limit_order_type = 3u64;
    let account_id = 573736952784748604u64;
    let expire_time = 493911u64;

    println!("limit_order_type: {}", limit_order_type);
    println!("account_id: {} (0x{:x})", account_id, account_id);
    println!("expire_time: {} (0x{:x})", expire_time, expire_time);

    // Helper to shift and add
    let shift_add = |acc: Felt, val: u64, shift: u32| -> Felt {
        let shift_multiplier = Felt::from(2u64).pow(shift as u128);
        (acc * shift_multiplier) + Felt::from(val)
    };

    let pm1 = Felt::from(limit_order_type);
    println!("\nStep 1: pm1 = {}", limit_order_type);
    println!("  Hex: 0x{:064x}", pm1);

    let pm1 = shift_add(pm1, account_id, 64);
    println!("\nStep 2: pm1 = (pm1 << 64) + account_id");
    println!("  Hex: 0x{:064x}", pm1);

    let pm1 = shift_add(pm1, account_id, 64);
    println!("\nStep 3: pm1 = (pm1 << 64) + account_id");
    println!("  Hex: 0x{:064x}", pm1);

    let pm1 = shift_add(pm1, account_id, 64);
    println!("\nStep 4: pm1 = (pm1 << 64) + account_id");
    println!("  Hex: 0x{:064x}", pm1);

    let pm1 = shift_add(pm1, expire_time, 32);
    println!("\nStep 5: pm1 = (pm1 << 32) + expire_time");
    println!("  Hex: 0x{:064x}", pm1);

    let shift_17 = Felt::from(2u64).pow(17u128);
    let pm1 = pm1 * shift_17;
    println!("\nStep 6: pm1 = pm1 << 17");
    println!("  Hex: 0x{:064x}", pm1);
}
