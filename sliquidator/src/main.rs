mod config;
mod oracles;
mod solvency;
mod types;
mod utils;

use crate::config::{load_app_config, AppConfig, SiloConfig, LIQUIDATOR_ADDRESS, SONIC_MULTICALL3};
use crate::solvency::run_solvency_monitor;

use crate::types::ILiquidationHelper::ILiquidationHelperInstance;
use crate::types::ISiloLens::ISiloLensInstance;
use crate::types::{EoaFanout, ILiquidationHelper, ISiloLens};
use crate::utils::read_private_key_from_file;

use alloy::primitives::{address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::signers::local::PrivateKeySigner;

use clap::Parser;
use common::soldiers::soldier_manager::SoldierManager;
use eyre::{eyre, Result};
use futures::future::join_all;
use tokio::task::JoinHandle;
use tracing::level_filters::LevelFilter;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Sonic Silo Liquidator
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct CliArgs {
    /// Base IPC path for the primary (read/observe) provider (e.g., ~/.sonic/sonic.ipc)
    #[arg(long)]
    base_ipc: Option<String>,

    /// Private key file for the base IPC provider (used for lens / maintenance ops)
    #[arg(long, value_name = "FILE")]
    base_key_file: String,

    /// Repeatable: RPC/WebSocket endpoints used for broadcasting (no wallets attached)
    /// Example: --endpoint http://localhost:8545 --endpoint https://rpc-x
    #[arg(long, value_name = "URL", num_args = 0.., action = clap::ArgAction::Append)]
    endpoint: Vec<String>,

    /// Path to silo config JSON file
    #[arg(long, default_value = "silos.json")]
    config: String,

    /// Log to console instead of file
    #[arg(long)]
    log_console: bool,

    /// Optional: withdraw a token using the base provider/key
    #[arg(long, value_name = "ADDRESS")]
    withdraw_token: Option<String>,

    /// Optional: RPC endpoint to use for withdrawn tokens (overrides base IPC)
    #[arg(long)]
    withdrawn_rpc: Option<String>,

    // >>> Soldier manager CLI
    /// Optional: BIP-39 mnemonic file for soldier EOAs (one seed -> many soldiers)
    #[arg(long, value_name = "FILE")]
    soldier_mnemonic_file: Option<String>,

    /// Number of soldiers to derive (m/44'/60'/0'/0/i)
    #[arg(long, default_value = "0")]
    soldiers: u32,

    /// Top-up if balance < min_wei (default: 0)
    #[arg(long, default_value = "0")]
    soldier_min_wei: u128,

    /// When topping up, send to reach this target balance in wei (default: 0)
    #[arg(long, default_value = "0")]
    soldier_target_wei: u128,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = CliArgs::parse();

    // ---------- logging ----------
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

    // ---------- base provider (IPC, with base signer) ----------
    let base_pk = read_private_key_from_file(&args.base_key_file)?;
    let base_signer = base_pk.parse::<PrivateKeySigner>()?;

    // For withdraws, keep it simple: use base provider/key
    // E.g. cargo run --bin sliquidator -r -- --withdrawn-rpc https://rpc.soniclabs.com --log-console --withdraw-token 0x79bbF4508B1391af3A0F4B30bb5FC4aa9ab0E07C --base-key
    // -file base_key
    if let Some(withdraw_token) = args.withdraw_token.clone() {
        if let Some(rpc_endpoint) = args.withdrawn_rpc.clone() {
            let withdraw_provider = ProviderBuilder::new()
                .wallet(base_signer.clone())
                .with_simple_nonce_management()
                .connect(&rpc_endpoint)
                .await
                .map_err(|e| eyre!("Failed to connect withdrawn RPC provider: {e}"))?;

            let base_helper =
                ILiquidationHelper::new(LIQUIDATOR_ADDRESS, withdraw_provider.clone());
            return handle_withdraw(base_helper, withdraw_token).await;
        }
    }

    let base_ipc = &args
        .base_ipc
        .expect("You must provide --base-ipc for normal operation.");
    let base_provider = ProviderBuilder::new()
        .wallet(base_signer.clone())
        .with_simple_nonce_management()
        .connect(base_ipc)
        .await
        .map_err(|e| eyre!("Failed to connect base IPC provider: {e}"))?;

    let chain_id = base_provider.get_chain_id().await?;

    info!("Base provider (IPC) connected at {}", base_ipc);

    // ---------- broadcast endpoints (NO wallet attached) ----------
    if args.endpoint.is_empty() {
        return Err(eyre!(
            "You must specify at least one --endpoint (broadcast RPC/WS URL)."
        ));
    }

    let mut broadcast_endpoints = Vec::new();
    for url in &args.endpoint {
        // No wallet here: we will send raw signed bytes
        let p = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect(url)
            .await
            .map_err(|e| eyre!("Failed to connect endpoint {}: {e}", url))?;
        info!("Broadcast endpoint connected: {}", url);
        broadcast_endpoints.push(p);
    }

    // ---------- EOA Soldiers (derived from mnemonic; optional) ----------

    let mut eoa_fanouts = Vec::new();

    if args.soldiers > 0 {
        let mnemonic_file = args
            .soldier_mnemonic_file
            .as_ref()
            .expect("--soldiers > 0 requires --soldier-mnemonic-file <FILE>");

        // Automatically loads or generates a mnemonic
        let mgr = SoldierManager::from_mnemonic_file(
            base_provider.clone(),
            base_signer.clone(),
            chain_id,
            mnemonic_file,
            SONIC_MULTICALL3,
        )?;

        let min = U256::from(args.soldier_min_wei);
        let target = U256::from(args.soldier_target_wei);
        if target < min {
            return Err(eyre!("soldier_target_wei must be >= soldier_min_wei"));
        }

        // 1) bootstrap soldiers (top-up if needed)
        let soldier_wallets = mgr.bootstrap_soldiers(args.soldiers, min, target).await?;

        // 2) attach to fanouts
        for w in soldier_wallets {
            info!("Soldier ready: {:?}", w.default_signer().address());
            eoa_fanouts.push(EoaFanout {
                signer: w,
                endpoints: broadcast_endpoints.clone(),
            });
        }

        // 3) launch periodic refunder every 2h
        let periodic_mgr = mgr.clone();
        tokio::spawn(async move {
            periodic_mgr
                .run_periodic_funding_task(
                    args.soldiers,
                    min,
                    target,
                    std::time::Duration::from_secs(7200),
                )
                .await;
        });
    }

    // ---------- config & contracts (read path uses base provider) ----------
    let app_config = load_app_config(&args.config)?;

    let lens = ISiloLens::new(
        address!("925d5466d4d5b01995e20e1245924ada6415126a"),
        base_provider.clone(), // read/watch on base IPC
    );

    // ---------- run per-silo tasks ----------
    let mut handles = vec![];
    for silo in &app_config.silos {
        info!("Starting solvency monitor for silo: {}", silo.silo_address);
        handles.push(
            spawn_tasks_for_silo(
                &app_config,
                silo.clone(),
                base_provider.clone(),
                lens.clone(),
                eoa_fanouts.clone(),
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

async fn spawn_tasks_for_silo<PWatch, PSend>(
    app_config: &AppConfig,
    silo_config: SiloConfig,
    watch_provider: PWatch,
    lens: ISiloLensInstance<PWatch>,
    eoa_fanouts: Vec<EoaFanout<PSend>>,
) -> Vec<JoinHandle<()>>
where
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
{
    let provider_for_watch = watch_provider.clone();

    run_solvency_monitor(
        provider_for_watch,
        lens,
        eoa_fanouts,
        silo_config,
        app_config.all_borrowers_full_health_refresh_secs,
        app_config.near_insolvency_check_millis,
        app_config.fast_check_solvency_threshold_percentage,
        app_config.fast_check_solvency_threshold_same_token_percentage,
        app_config.fast_check_solvency_threshold_stable_market_percentage,
    )
    .await
}
