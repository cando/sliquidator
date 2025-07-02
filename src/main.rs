mod config;
mod hermes;
mod oracle_simulator;
mod solvency;
mod types;
mod utils;

use crate::config::{load_silo_configs, SiloConfig};
use crate::solvency::run_solvency_monitor;
use crate::types::ILiquidationHelper::ILiquidationHelperInstance;
use crate::types::ISiloLens::ISiloLensInstance;
use crate::types::{ILiquidationHelper, ISiloLens};
use crate::utils::{read_private_key_from_console, read_private_key_from_file};

use alloy::primitives::address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;

use clap::Parser;
use eyre::eyre;
use eyre::Result;
use futures::future::join_all;

use tokio::task::JoinHandle;
use tracing::level_filters::LevelFilter;
use tracing::{error, info};
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

    #[arg(long, value_name = "ADDRESS")]
    withdraw_token: Option<String>,
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

    let private_key = if let Some(file_path) = args.private_key_file {
        if !file_path.is_empty() {
            tracing::debug!("Reading private key from file: {}", file_path);
            read_private_key_from_file(&file_path)?
        } else {
            read_private_key_from_console()?
        }
    } else {
        read_private_key_from_console()?
    };

    let signer = private_key.parse::<PrivateKeySigner>()?;

    let provider = match (args.ipc, args.rpc) {
        (Some(ipc_path), _) => {
            ProviderBuilder::new()
                .wallet(signer)
                .with_simple_nonce_management()
                .connect(&ipc_path)
                .await?
        }
        (_, Some(rpc_url)) => {
            ProviderBuilder::new()
                .wallet(signer)
                .with_simple_nonce_management()
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
        address!("dead000000000000000000000000000000000007"),
        provider.clone(),
    );

    if let Some(withdraw_token) = args.withdraw_token {
        return handle_withdraw(helper, withdraw_token).await;
    }

    let mut handles = vec![];
    for silo in &silo_configs {
        info!("Starting solvency monitor for silo: {}", silo.silo_address);
        handles.push(
            spawn_tasks_for_silo(
                silo.clone(),
                provider.clone(),
                lens.clone(),
                helper.clone(),
                args.fast_check_solvency_threshold,
            )
            .await,
        );
    }

    join_all(handles.into_iter().flatten().collect::<Vec<_>>()).await;

    Ok(())
}

async fn handle_withdraw(
    helper: ILiquidationHelperInstance<impl Provider + Clone>,
    token: String,
) -> Result<()> {
    use alloy::primitives::Address;
    use tracing::{error, info};

    let token_address = token.trim().parse::<Address>()?;
    info!("Withdrawing token: {}", token_address);

    let tx = helper.withdraw(token_address);
    let result = async {
        let pending_tx = tx.send().await?;
        let receipt = pending_tx.get_receipt().await?;

        if !receipt.status() {
            return Err(eyre!("Transaction reverted"));
        }

        Ok(receipt)
    }
    .await;

    match result {
        Ok(receipt) => {
            info!(
                "Withdrawn Tx succeeded in block {:?}, hash: {:?}",
                receipt.block_number, receipt.transaction_hash
            );
        }
        Err(e) => {
            error!("Withdraw tx failed: {e}");
        }
    }

    Ok(())
}

async fn spawn_tasks_for_silo<P>(
    silo_config: SiloConfig,
    provider: P,
    lens: ISiloLensInstance<P>,
    helper: ILiquidationHelperInstance<P>,
    threshold: f64,
) -> Vec<JoinHandle<()>>
where
    P: Provider + Clone + 'static,
{
    let provider_for_watch = provider.clone();
    let silo_config_for_watch = silo_config.clone();

    run_solvency_monitor(
        provider_for_watch,
        lens,
        helper,
        silo_config_for_watch,
        30,
        20,
        threshold,
    )
    .await
}
