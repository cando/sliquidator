mod config;
mod odos;
mod types;
mod watch;

use std::fs;

use crate::config::load_silo_configs;
use crate::types::{ILiquidationHelper, ISiloLens};
use crate::watch::watch_loop;

use alloy::primitives::Address;
use alloy::{primitives::address, providers::ProviderBuilder, signers::local::PrivateKeySigner};
use eyre::Result;
use futures::future::join_all;
use rpassword::prompt_password;

#[tokio::main]
async fn main() -> Result<()> {
    let file_appender = tracing_appender::rolling::daily(".", "sliquidator.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_ansi(false)
        .init();

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
                tracing::warn!("No borrowers loaded for silo {}", silo_config.silo_address);
                return Ok(()) as Result<()>;
            }

            watch_loop(provider, lens, helper, borrowers, &silo_config, 30, 0).await
        })
    });

    let _ = join_all(handles).await;

    Ok(())
}

pub fn load_borrowers(file_path: &str) -> Vec<Address> {
    let content = fs::read_to_string(file_path)
        .unwrap_or_else(|e| panic!("Failed to read borrower file: {e}"));

    content
        .lines()
        .filter_map(|line| line.trim().parse::<Address>().ok())
        .collect()
}

fn read_private_key_from_console() -> eyre::Result<String> {
    let key = prompt_password("Enter your private key (0x...): ")?;
    if key.trim().is_empty() {
        eyre::bail!("Private key cannot be empty.");
    }
    Ok(key.trim().to_owned())
}
