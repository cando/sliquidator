mod config;
mod odos;
mod types;
mod watch;

use std::fs;

use crate::config::load_silo_configs;
use crate::types::{ILiquidationHelper, ISiloLens};
use crate::watch::watch_loop;

use alloy::primitives::Address;
use alloy::{
    primitives::{address, U256},
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
};
use eyre::Result;
use futures::future::join_all;
use rpassword::prompt_password;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let key_hex = read_private_key_from_console()?;
    let signer = key_hex.parse::<PrivateKeySigner>()?;

    let provider = ProviderBuilder::new()
        .wallet(signer)
        .connect_http("https://rpc.soniclabs.com".parse()?); // "https://rpc.soniclabs.com" // "http://localhost:8545"

    let silo_configs = load_silo_configs("silos.json")?;

    // Shared lens/helper for now — can be made per-silo if needed
    let lens = ISiloLens::new(
        address!("925d5466d4d5b01995e20e1245924ada6415126a"),
        provider.clone(),
    );
    let helper = ILiquidationHelper::new(
        address!("dead000000000000000000000000000000000001"),
        provider.clone(),
    );

    let handles = silo_configs.into_iter().map(|silo_config| {
        let provider = provider.clone();
        let lens = lens.clone();
        let helper = helper.clone();
        tokio::spawn(async move {
            let borrowers = load_borrowers(&silo_config.borrowers_file);
            if borrowers.is_empty() {
                tracing::warn!(
                    "No borrowers loaded for silo at {}",
                    silo_config.silo_address
                );
                return Ok(()) as Result<()>;
            }

            watch_loop(
                provider,
                lens,
                helper,
                borrowers,
                &silo_config,
                30,
                percent_to_wei(0.1),
                20,
            )
            .await
        })
    });

    let _ = join_all(handles).await;

    Ok(())
}

// Borrower file loader
pub fn load_borrowers(file_path: &str) -> Vec<Address> {
    let content = fs::read_to_string(file_path)
        .unwrap_or_else(|e| panic!("Failed to read borrower file: {e}"));

    content
        .lines()
        .filter_map(|line| line.trim().parse::<Address>().ok())
        .collect()
}

/// Converts a percentage to a 18-decimal fixed-point number (wei-style).
///
/// # Arguments
///
/// * `percentage` - The percentage as a floating-point number (e.g., 0.01 for 0.01%)
///
/// # Returns
///
/// * `U256` - The 18-decimal fixed-point representation
fn percent_to_wei(percentage: f64) -> U256 {
    // Convert to fractional (e.g., 0.01% -> 0.0001)
    let fractional = percentage / 100.0;

    // Scale up to 18 decimal places
    let scaled = fractional * 1e18;

    U256::from(scaled as u128)
}

fn read_private_key_from_console() -> eyre::Result<String> {
    let key = prompt_password("Enter your private key (0x...): ")?;
    if key.trim().is_empty() {
        eyre::bail!("Private key cannot be empty.");
    }
    Ok(key.trim().to_owned())
}
