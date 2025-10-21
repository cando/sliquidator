mod common;
pub(crate) mod same_token_strategy;
pub(crate) mod swap_strategy;

use std::{collections::HashSet, sync::Arc};

use alloy::{
    eips::BlockNumberOrTag,
    network::Ethereum,
    primitives::{address, Address, U256},
    providers::{PendingTransactionBuilder, Provider},
    rpc::types::Filter,
};

use ::common::oracles::chainlink::{chainlink_forward_listener_task, SharedChainlinkPrice};
use chrono::Timelike;
use futures::StreamExt;
use tokio::{
    sync::{mpsc, RwLock},
    task::JoinHandle,
    time::{sleep, Duration},
};
use tracing::{error, info};

use crate::{
    config::SiloConfig,
    oracles::hermes::{hermes_price_feed_task, SharedPythPrice},
    solvency::{same_token_strategy::same_token_solvency, swap_strategy::swap_solvency},
    types::{
        Borrower, EoaFanout,
        IHookReceiver::{self},
        ISilo::{self},
        ISiloConfig::{self},
        ISiloLens::ISiloLensInstance,
        LiquidationContext, OraclePriceData, SharedAddressList, IERC20,
    },
    utils::{load_borrowers, percent_to_wei},
};

const GET_USERS_HEALTH_BATCH_SIZE: usize = 50;

const S_USDC_CHAINLINK_AGGREGATOR_ADDRESS: Address =
    address!("0x0BdbFF19543B20d0bc2d1eA08deE2be4C0b76743");

pub async fn run_solvency_monitor<PWatch, PSend>(
    provider: PWatch,
    lens: ISiloLensInstance<PWatch>,
    helpers: Vec<EoaFanout<PSend>>,
    config: SiloConfig,
    full_refresh_secs: u64,
    near_insolvency_check_millis: u64,
    fast_check_solvency_threshold: f64,
    fast_check_solvency_threshold_same_token_percentage: f64,
    fast_check_solvency_threshold_stable_market_percentage: f64,
) -> Vec<JoinHandle<()>>
where
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
{
    let hook = IHookReceiver::new(config.hook_address, provider.clone());
    let silo = ISilo::new(config.silo_address, provider.clone());
    let silo_config = ISiloConfig::new(config.config_address, provider.clone());
    let fast_check_threshold = percent_to_wei(fast_check_solvency_threshold);
    let fast_check_threshold_same_token =
        percent_to_wei(fast_check_solvency_threshold_same_token_percentage);
    let fast_check_threshold_stable_market =
        percent_to_wei(fast_check_solvency_threshold_stable_market_percentage);
    let watchlist = Arc::new(RwLock::new(Vec::new()));
    let blacklist = Arc::new(RwLock::new(HashSet::from([])));
    let shared_pyth_price: SharedPythPrice = Arc::new(RwLock::new(None));
    let shared_chainlink_price: SharedChainlinkPrice = Arc::new(RwLock::new(None));

    let initial_borrowers = load_borrowers(&config.borrowers_file);
    if initial_borrowers.is_empty() {
        error!("No borrowers loaded for silo {}", config.silo_address);
        return vec![];
    } else {
        info!(
            "Loaded {} initial borrowers for silo {}",
            initial_borrowers.len(),
            config.silo_address
        );
    }

    let borrowers: SharedAddressList =
        Arc::new(RwLock::new(initial_borrowers.into_iter().collect()));

    let oracle_data = match &config.oracle {
        crate::config::Oracle::Chainlink => OraclePriceData::Chainlink {
            forwarded_price: shared_chainlink_price.clone(),
        },
        crate::config::Oracle::Pyth { price_id, slot_0 } => OraclePriceData::Pyth {
            price_id: price_id.clone(),
            price_feed: shared_pyth_price,
            slot_0: *slot_0,
        },
        crate::config::Oracle::Redstone => OraclePriceData::Redstone,
    };

    let liquidation_threshold = lens
        .getLt(config.silo_address)
        .call()
        .await
        .unwrap_or_default();

    let (tx_sender, tx_receiver) =
        mpsc::unbounded_channel::<(Address, PendingTransactionBuilder<Ethereum>)>();
    let ctx = LiquidationContext {
        provider,
        lens,
        helpers,
        silo_config,
        config: config.clone(),
        hook,
        silo,
        oracle_price_data: oracle_data.clone(),
        watchlist: watchlist.clone(),
        blacklist: blacklist.clone(),
        borrowers,
        liquidation_tx_sender: tx_sender,
        fast_check_threshold,
        fast_check_threshold_same_token,
        fast_check_threshold_stable_market,
        liquidation_threshold,
    };

    let mut handles = vec![
        tokio::spawn(refresh_watchlist_task(ctx.clone(), full_refresh_secs)),
        tokio::spawn(borrower_listener_task(ctx.clone())),
        tokio::spawn(solvency_monitor_task(
            ctx.clone(),
            near_insolvency_check_millis,
        )),
        tokio::spawn(receipt_monitor_task(ctx.clone(), tx_receiver)),
    ];

    match oracle_data {
        OraclePriceData::Pyth {
            price_id,
            price_feed,
            ..
        } => {
            handles.push(tokio::spawn(hermes_price_feed_task(
                vec![price_id],
                price_feed.clone(),
            )));
        }
        OraclePriceData::Chainlink { forwarded_price } => {
            handles.push(tokio::spawn(chainlink_forward_listener_task(
                ctx.provider.clone(),
                forwarded_price.clone(),
                S_USDC_CHAINLINK_AGGREGATOR_ADDRESS,
            )));
        }
        OraclePriceData::Redstone => {}
    }

    handles
}

async fn refresh_watchlist_task<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: LiquidationContext<PWatch, PSend>,
    full_refresh_secs: u64,
) {
    loop {
        info!(silo = ?ctx.config.silo_address, "⏳ Refreshing borrower list...");

        let borrowers_guard = ctx.borrowers.read().await;
        let blacklist_guard = ctx.blacklist.read().await;

        let borrower_inputs: Vec<Borrower> = borrowers_guard
            .iter()
            .filter(|b| !blacklist_guard.contains(*b))
            .map(|&wallet| Borrower {
                silo: ctx.config.silo_address,
                wallet,
            })
            .collect();
        drop(borrowers_guard);
        drop(blacklist_guard);

        let mut new_watchlist = Vec::new();
        for batch in borrower_inputs.chunks(GET_USERS_HEALTH_BATCH_SIZE) {
            if let Ok(healths) = ctx.lens.getUsersHealth(batch.to_vec()).call().await {
                for (i, bh) in healths.iter().enumerate() {
                    let wallet = batch[i].wallet;
                    let gap = bh.lt.saturating_sub(bh.ltv);
                    if gap <= ctx.fast_check_threshold {
                        if let Ok(config_data) =
                            ctx.silo_config.getConfigsForSolvency(wallet).call().await
                        {
                            if !bh.lt.is_zero() && !bh.ltv.is_zero() {
                                if (config_data.collateralConfig.token
                                    == config_data.debtConfig.token
                                    && gap > ctx.fast_check_threshold_same_token)
                                    || (ctx.config.is_stable_market
                                        && gap > ctx.fast_check_threshold_stable_market)
                                {
                                    // Solvency check for same token borrow/lend is way slower or for stable market, so
                                    // we check if the gap is small enough to warrant it
                                    info!(borrower = %wallet, gap = %gap, threshold = ?ctx.fast_check_threshold_stable_market, ltv=%bh.ltv, "Skipping same token/stable market check");
                                    continue;
                                }

                                info!(borrower = %wallet, "Borrower near insolvency: LTV = {}, LT = {}, gap = {}", bh.ltv, bh.lt, gap);
                                new_watchlist.push((
                                    wallet,
                                    config_data.collateralConfig.token,
                                    config_data.debtConfig.token,
                                ));
                            }
                        }
                    }
                }
            }
        }

        {
            let mut lock = ctx.watchlist.write().await;
            *lock = new_watchlist;

            if chrono::Local::now().hour() == 1 && chrono::Local::now().minute() == 42 {
                // I know this is a bit of a hack, but it works for now.
                // Just clear the blacklist at 01:42 local time, every day, so we don't have stale addresses
                info!("Clearing blacklist at 01:42 local time");
                let mut blacklist_lock = ctx.blacklist.write().await;
                blacklist_lock.clear();
            }
        }

        sleep(Duration::from_secs(full_refresh_secs)).await;
    }
}

fn write_borrowers_to_file(path: &str, borrowers: &HashSet<Address>) {
    let contents = borrowers
        .iter()
        .map(|addr| format!("{addr:#x}"))
        .collect::<Vec<_>>()
        .join("\n");

    std::fs::write(path, contents)
        .unwrap_or_else(|e| error!("Failed to write to borrowers file for {}: {e}", &path));
}

async fn borrower_listener_task<PWatch, PSend>(ctx: LiquidationContext<PWatch, PSend>)
where
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
{
    let filter = Filter::new()
        .address(ctx.config.debt_token_address)
        .event("Transfer(address,address,uint256)")
        .from_block(BlockNumberOrTag::Latest);

    let sub = match ctx.provider.subscribe_logs(&filter).await {
        Ok(sub) => sub,
        Err(e) => {
            error!("Failed to subscribe to logs: {e}");
            return;
        }
    };

    let mut stream = sub.into_stream();

    let erc20 = IERC20::new(ctx.config.debt_token_address, ctx.provider.clone());

    while let Some(log) = stream.next().await {
        if let Ok(decoded) = log.log_decode::<IERC20::Transfer>() {
            let from = decoded.data().from;
            let to = decoded.data().to;

            for addr in [from, to] {
                if addr == Address::ZERO {
                    continue;
                }

                match erc20.balanceOf(addr).call().await {
                    Ok(balance) if balance > U256::ZERO => {
                        let mut list = ctx.borrowers.write().await;
                        if list.insert(addr) {
                            info!(
                                "New borrower detected: {addr:#x}. Borrowers count: {}",
                                list.len()
                            );

                            write_borrowers_to_file(&ctx.config.borrowers_file, &list);
                        } else {
                            info!("Address {addr:#x} already in borrower list");
                        }
                    }
                    Ok(_) => {
                        info!("Address {addr:#x} has zero balance, removing from borrowers");
                        let mut list = ctx.borrowers.write().await;
                        list.remove(&addr);

                        write_borrowers_to_file(&ctx.config.borrowers_file, &list);
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn solvency_monitor_task<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: LiquidationContext<PWatch, PSend>,
    near_insolvency_check_millis: u64,
) {
    loop {
        sleep(Duration::from_millis(near_insolvency_check_millis)).await;

        let mut to_watch = {
            let lock = ctx.watchlist.read().await;
            lock.clone()
        };

        if to_watch.is_empty() {
            continue;
        }

        for (borrower, collateral_token, debt_token) in to_watch.drain(..) {
            if ctx.blacklist.read().await.contains(&borrower) {
                continue;
            }

            if collateral_token == debt_token {
                same_token_solvency(ctx.clone(), borrower, debt_token).await;
            } else {
                swap_solvency(ctx.clone(), borrower, debt_token, collateral_token).await;
            }
        }
    }
}

async fn receipt_monitor_task<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: LiquidationContext<PWatch, PSend>,
    mut tx_receiver: mpsc::UnboundedReceiver<(Address, PendingTransactionBuilder<Ethereum>)>,
) {
    while let Some((borrower, pending_tx)) = tx_receiver.recv().await {
        match pending_tx.get_receipt().await {
            Ok(receipt) => {
                if !receipt.status() {
                    error!(
                        ?borrower,
                        "❌ Liquidation tx {} failed", receipt.transaction_hash,
                    );
                    ctx.blacklist.write().await.insert(borrower);
                } else {
                    info!(
                        ?borrower,
                        "✅ Liquidation tx {} succeeded", receipt.transaction_hash,
                    );
                }
            }
            Err(e) => {
                error!(?borrower, "❌ Failed to get receipt for tx {e}");
                continue;
            }
        };
    }
}
