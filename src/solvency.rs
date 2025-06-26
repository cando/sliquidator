use std::{
    collections::HashSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{
    eips::BlockNumberOrTag,
    primitives::{address, aliases::I24, Address, Bytes, U160, U256},
    providers::Provider,
    rpc::types::Filter,
    sol_types::SolCall,
};
use eyre::eyre;
use futures::StreamExt;
use tokio::{
    sync::RwLock,
    task::JoinHandle,
    time::{sleep, Duration},
};
use tracing::{debug, error, info, warn};

use crate::{
    config::SiloConfig,
    hermes::{hermes_price_feed_task, SharedPythPrice},
    pyth_simulator::is_borrower_solvent_simulated,
    types::{
        Borrower,
        IHookReceiver::{self, IHookReceiverInstance},
        ILiquidationHelper::ILiquidationHelperInstance,
        ISilo::{self, ISiloInstance},
        ISiloConfig::{self, ISiloConfigInstance},
        ISiloLens::ISiloLensInstance,
        LiquidationData, SharedAddressList, IERC20,
    },
    utils::{load_borrowers, percent_to_wei},
};

#[derive(Clone)]
enum SolvencyMode {
    Standard,
    SimulatedPyth {
        price_id: String,
        price_feed: SharedPythPrice,
    },
}

#[derive(Clone)]
struct LiquidationContext<P: Provider + Clone + 'static> {
    provider: P,
    lens: ISiloLensInstance<P>,
    helper: ILiquidationHelperInstance<P>,
    silo_config: ISiloConfigInstance<P>,
    config: SiloConfig,
    hook: IHookReceiverInstance<P>,
    silo: ISiloInstance<P>,
    mode: SolvencyMode,
    watchlist: Arc<RwLock<Vec<(Address, Address, Address)>>>,
    blacklist: Arc<RwLock<HashSet<Address>>>,
    borrowers: SharedAddressList,
    fast_check_threshold: U256,
}

const GET_USERS_HEALTH_BATCH_SIZE: usize = 50;

pub fn run_solvency_monitor<P>(
    provider: P,
    lens: ISiloLensInstance<P>,
    helper: ILiquidationHelperInstance<P>,
    config: SiloConfig,
    full_refresh_secs: u64,
    near_insolvency_check_millis: u64,
    fast_check_solvency_threshold: f64,
) -> eyre::Result<Vec<JoinHandle<()>>>
where
    P: Provider + Clone + 'static,
{
    let hook = IHookReceiver::new(config.hook_address, provider.clone());
    let silo = ISilo::new(config.silo_address, provider.clone());
    let silo_config = ISiloConfig::new(config.config_address, provider.clone());
    let fast_check_threshold = percent_to_wei(fast_check_solvency_threshold);

    let watchlist = Arc::new(RwLock::new(Vec::new()));
    let blacklist = Arc::new(RwLock::new(HashSet::from([address!(
        "0xdec0000000000000000000000000000000000001"
    )])));
    let shared_pyth_price: SharedPythPrice = Arc::new(RwLock::new(None));

    let initial_borrowers = load_borrowers(&config.borrowers_file);
    if initial_borrowers.is_empty() {
        eyre::bail!("No borrowers loaded for silo {}", config.silo_address);
    }

    let borrowers: SharedAddressList =
        Arc::new(RwLock::new(initial_borrowers.into_iter().collect()));

    let mode = match &config.simulate_solvency_price_id {
        Some(price_id) => SolvencyMode::SimulatedPyth {
            price_id: price_id.clone(),
            price_feed: shared_pyth_price,
        },
        None => SolvencyMode::Standard,
    };

    let ctx = LiquidationContext {
        provider,
        lens,
        helper,
        silo_config,
        config: config.clone(),
        hook,
        silo,
        mode: mode.clone(),
        watchlist: watchlist.clone(),
        blacklist: blacklist.clone(),
        borrowers,
        fast_check_threshold,
    };

    let mut handles = vec![
        tokio::spawn(refresh_watchlist_task(ctx.clone(), full_refresh_secs)),
        tokio::spawn(borrower_listener_task(ctx.clone())),
        tokio::spawn(solvency_monitor_task(
            ctx.clone(),
            near_insolvency_check_millis,
        )),
    ];

    match mode {
        SolvencyMode::SimulatedPyth {
            price_id,
            price_feed,
        } => {
            handles.push(tokio::spawn(hermes_price_feed_task(
                vec![price_id],
                price_feed.clone(),
            )));
        }
        _ => (),
    }

    Ok(handles)
}

async fn refresh_watchlist_task<P: Provider + Clone + 'static>(
    ctx: LiquidationContext<P>,
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
                                info!(wallet = %wallet, "Borrower near insolvency: LTV = {}, LT = {}, gap = {}", bh.ltv, bh.lt, gap);
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
        }

        sleep(Duration::from_secs(full_refresh_secs)).await;
    }
}

async fn borrower_listener_task<P>(ctx: LiquidationContext<P>)
where
    P: Provider + Clone + 'static,
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
                        } else {
                            info!("Address {addr:#x} already in borrower list");
                        }
                    }
                    Ok(_) => {
                        info!("Address {addr:#x} has zero balance, removing from borrowers");
                        let mut list = ctx.borrowers.write().await;
                        list.remove(&addr);
                    }
                    _ => {}
                }
            }
        }
    }
}

async fn solvency_monitor_task<P: Provider + Clone + 'static>(
    ctx: LiquidationContext<P>,
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

            let multicall = ctx
                .provider
                .multicall()
                .add(ctx.hook.maxLiquidation(borrower))
                .add(ctx.silo.isSolvent(borrower));

            let (liquidation, mut is_solvent) = match multicall.aggregate().await {
                Ok(result) => result,
                Err(e) => {
                    error!(?borrower, "Multicall failed: {e}");
                    continue;
                }
            };

            if is_solvent {
                match &ctx.mode {
                    SolvencyMode::SimulatedPyth { price_feed, .. } => {
                        if let Some(p) = &*price_feed.read().await {
                            match is_borrower_solvent_simulated(
                                &ctx.provider,
                                ctx.config.silo_address,
                                borrower,
                                p.publish_time,
                                p.expo,
                                p.price,
                                p.conf,
                            )
                            .await
                            {
                                Ok(solvent) => {
                                    is_solvent = solvent;
                                    if is_solvent {
                                        info!(?borrower, "Still solvent after Pyth simulation.");
                                        continue;
                                    }
                                }
                                Err(e) => {
                                    error!(?borrower, "Pyth simulation failed: {e}");
                                    continue;
                                }
                            }
                        } else {
                            warn!(?borrower, "No Pyth price, skipping simulation.");
                            continue;
                        }
                    }
                    SolvencyMode::Standard => {
                        debug!(?borrower, "Standard check: solvent. Skipping.");
                        continue;
                    }
                }
            }

            if !is_solvent
                && liquidation.maxDebt
                    < U256::from(ctx.config.min_amounts_for_liquidation[&debt_token])
            {
                warn!(
                    ?borrower,
                    "Insolvent but below min debt threshold. Skipping."
                );
                ctx.watchlist
                    .write()
                    .await
                    .retain(|(w, _, _)| *w != borrower);
                continue;
            }

            info!(?borrower, max_debt = %liquidation.maxDebt, "🚨 Borrower is insolvent → preparing liquidation");

            let swap_data = if collateral_token != debt_token {
                let amount_for_swap =
                    liquidation.collateralToLiquidate * U256::from(995) / U256::from(1000);

                let call = crate::types::IV3SwapRouter::exactInputSingleCall {
                    params: crate::types::IV3SwapRouter::ExactInputSingleParams {
                        tokenIn: collateral_token,
                        tokenOut: debt_token,
                        tickSpacing: I24::unchecked_from(50),
                        recipient: *ctx.helper.address(),
                        deadline: U256::from(
                            SystemTime::now()
                                .duration_since(UNIX_EPOCH)
                                .expect("Time went backwards")
                                .as_secs()
                                + 300,
                        ),
                        amountIn: amount_for_swap,
                        amountOutMinimum: U256::ZERO,
                        sqrtPriceLimitX96: U160::ZERO,
                    },
                };

                Bytes::from(call.abi_encode())
            } else {
                Vec::new().into()
            };

            let tx = ctx
                .helper
                .executeLiquidation(
                    ctx.config.silo_address,
                    debt_token,
                    liquidation.maxDebt,
                    LiquidationData {
                        hook: ctx.config.hook_address,
                        collateralAsset: collateral_token,
                        borrower,
                    },
                    swap_data,
                )
                .gas(5_000_000u64)
                .gas_price(65_000_000_000u128);

            match async {
                let pending_tx = tx.send().await?;
                let receipt = pending_tx.get_receipt().await?;
                if !receipt.status() {
                    return Err(eyre!("Transaction reverted"));
                }
                Ok(receipt)
            }
            .await
            {
                Ok(receipt) => {
                    info!(block = ?receipt.block_number, "✅ Tx succeeded");
                }
                Err(e) => {
                    error!(?borrower, "❌ Liquidation tx failed: {e}");
                    ctx.blacklist.write().await.insert(borrower);
                }
            }
        }
    }
}
