mod config;
mod oracles;
mod solvency;
mod types;
mod utils;

use crate::config::{load_app_config, AppConfig, SiloConfig};
use crate::solvency::run_solvency_monitor;
use crate::types::ILiquidationHelper::ILiquidationHelperInstance;
use crate::types::ISiloLens::ISiloLensInstance;
use crate::types::{ILiquidationHelper, ISiloLens};
use crate::utils::read_private_key_from_file;

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

    /// Log to console instead of file
    #[arg(long)]
    log_console: bool,

    /// Path to private key file (default: read from console)
    #[arg(long, value_name = "FILES", num_args = 1.., use_value_delimiter = true)]
    private_key_files: Vec<String>,

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

    let mut provider_pool = Vec::new();

    for file_path in &args.private_key_files {
        let private_key = read_private_key_from_file(file_path)?;
        let signer = private_key.parse::<PrivateKeySigner>()?;

        let provider = match (args.ipc.clone(), args.rpc.clone()) {
            (Some(ipc), _) => {
                ProviderBuilder::new()
                    .wallet(signer.clone())
                    .with_simple_nonce_management()
                    .connect(&ipc)
                    .await?
            }
            (_, Some(rpc)) => {
                ProviderBuilder::new()
                    .wallet(signer.clone())
                    .with_simple_nonce_management()
                    .connect(&rpc)
                    .await?
            }
            _ => eyre::bail!("You must specify --ipc or --rpc"),
        };

        provider_pool.push(provider);
    }

    let app_config = load_app_config(&args.config)?;

    let lens = ISiloLens::new(
        address!("925d5466d4d5b01995e20e1245924ada6415126a"),
        provider_pool[0].clone(),
    );

    // prepare a set of helpers for each provider, for each private key
    let helpers = provider_pool
        .iter()
        .map(|provider| {
            ILiquidationHelper::new(
                address!("dead000000000000000000000000000000000003"),
                provider.clone(),
            )
        })
        .collect::<Vec<_>>();

    if let Some(withdraw_token) = args.withdraw_token {
        return handle_withdraw(helpers[0].clone(), withdraw_token).await;
    }

    let mut handles = vec![];
    for silo in &app_config.silos {
        info!("Starting solvency monitor for silo: {}", silo.silo_address);
        handles.push(
            spawn_tasks_for_silo(
                &app_config,
                silo.clone(),
                provider_pool[0].clone(),
                lens.clone(),
                helpers.clone(),
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
    app_config: &AppConfig,
    silo_config: SiloConfig,
    provider: P,
    lens: ISiloLensInstance<P>,
    helper: Vec<ILiquidationHelperInstance<P>>,
) -> Vec<JoinHandle<()>>
where
    P: Provider + Clone + 'static,
{
    let provider_for_watch = provider.clone();

    run_solvency_monitor(
        provider_for_watch,
        lens,
        helper,
        silo_config,
        app_config.all_borrowers_full_health_refresh_secs,
        app_config.near_insolvency_check_millis,
        app_config.fast_check_solvency_threshold_percentage,
        app_config.fast_check_solvency_threshold_same_token_percentage,
    )
    .await
}
