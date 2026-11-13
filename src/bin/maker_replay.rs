//! Execute the production Lighter maker against a public recorded venue.
use aleph_tx::strategy::inventory_neutral_mm::run_recorded_maker;
use anyhow::{Context, Result};
use std::{fs::File, io::BufReader};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let [config, events] = args.as_slice() else {
        anyhow::bail!("Usage: maker_replay CONFIG.json EVENTS.ndjson");
    };
    let config =
        serde_json::from_reader(File::open(config)?).context("invalid maker configuration")?;
    let report = run_recorded_maker(config, BufReader::new(File::open(events)?))?;
    serde_json::to_writer(std::io::stdout().lock(), &report)?;
    println!();
    Ok(())
}
