mod config;
mod odos;
mod types;
mod watch;

use std::fs;

use crate::config::load_silo_configs;
use crate::types::{ILiquidationHelper, ISiloLens};
use crate::watch::watch_loop;

use alloy::primitives::{address, Address};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;

use clap::Parser;
use eyre::Result;
use futures::future::join_all;
use rpassword::prompt_password;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;

/// Sonic Silo Liquidator
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct CliArgs {
    /// Path to the IPC socket (e.g., ~/.sonic/sonic.ipc)
    #[arg(long, conflicts_with = "rpc")]
    ipc: Option<String>,

    /// HTTP RPC URL (e.g., http://localhost:8545)
    #[arg(long, conflicts_with = "ipc")]
    rpc: Option<String>,

    /// Path to silo config JSON file
    #[arg(long, default_value = "silos.json")]
    config: String,

    // Fast check solvency threshold in percentage (default: 0.5%)
    #[arg(long, default_value = "0.5")]
    fast_check_solvency_threshold: f64,

    /// Log to console instead of file
    #[arg(long)]
    log_console: bool,

    /// Path to private key file (default: read from console)
    #[arg(long)]
    private_key_file: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();

    let env_filter = EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .from_env_lossy();

    if args.log_console {
        tracing_subscriber::fmt()
            .with_ansi(true)
            .with_level(true)
            .with_env_filter(env_filter)
            .init();
    } else {
        let file_appender = tracing_appender::rolling::daily(".", "sliquidator.log");
        let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::fmt()
            .with_writer(non_blocking)
            .with_ansi(false)
            .with_env_filter(env_filter)
            .init();
    }

    let key_hex = if let Some(file_path) = args.private_key_file {
        if !file_path.is_empty() {
            tracing::debug!("Reading private key from file: {}", file_path);
            read_private_key_from_file(&file_path)?
        } else {
            read_private_key_from_console()?
        }
    } else {
        read_private_key_from_console()?
    };

    let signer = key_hex.parse::<PrivateKeySigner>()?;

    let provider = match (args.ipc, args.rpc) {
        (Some(ipc_path), _) => {
            ProviderBuilder::new()
                .wallet(signer)
                .connect(&ipc_path)
                .await?
        }
        (_, Some(rpc_url)) => {
            ProviderBuilder::new()
                .wallet(signer)
                .connect(&rpc_url)
                .await?
        }
        (None, None) => eyre::bail!("You must specify either --ipc or --rpc"),
    };

    let silo_configs = load_silo_configs(&args.config)?;

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

            watch_loop(
                provider,
                lens,
                helper,
                borrowers,
                &silo_config,
                30,
                args.fast_check_solvency_threshold,
                0,
            )
            .await
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

fn read_private_key_from_file(path: &str) -> eyre::Result<String> {
    let key = fs::read_to_string(path)?.trim().to_owned();
    if key.is_empty() {
        eyre::bail!("Private key is empty");
    }
    Ok(key.trim().to_owned())
}
