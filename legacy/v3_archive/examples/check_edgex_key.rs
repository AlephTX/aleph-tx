use anyhow::{Result, anyhow, ensure};
use starknet_crypto::{Felt, get_public_key};

fn main() -> Result<()> {
    let input = std::env::var("EDGEX_STARK_PRIVATE_KEY")
        .map_err(|_| anyhow!("Set EDGEX_STARK_PRIVATE_KEY in the local environment"))?;
    let private_key = Felt::from_hex(&input).map_err(|_| anyhow!("Invalid EdgeX key format"))?;
    ensure!(private_key != Felt::ZERO, "The EdgeX key must be nonzero");

    println!("Public Key (x): 0x{:064x}", get_public_key(&private_key));
    Ok(())
}
