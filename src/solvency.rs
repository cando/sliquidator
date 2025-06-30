use std::{
    collections::HashSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
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
    liquidation_threshold: U256,
}

const GET_USERS_HEALTH_BATCH_SIZE: usize = 50;
const SAME_TOKEN_MULTIPLIER: u64 = 10;

pub async fn run_solvency_monitor<P>(
    provider: P,
    lens: ISiloLensInstance<P>,
    helper: ILiquidationHelperInstance<P>,
    config: SiloConfig,
    full_refresh_secs: u64,
    near_insolvency_check_millis: u64,
    fast_check_solvency_threshold: f64,
) -> Vec<JoinHandle<()>>
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
        error!("No borrowers loaded for silo {}", config.silo_address);
        return vec![];
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

    let liquidation_threshold = lens
        .getLt(config.silo_address)
        .call()
        .await
        .unwrap_or_default();

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
        liquidation_threshold,
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

    handles
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
                                if config_data.collateralConfig.token
                                    == config_data.debtConfig.token
                                    && gap.saturating_mul(U256::from(SAME_TOKEN_MULTIPLIER))
                                        > ctx.fast_check_threshold
                                // this means we are checking for 1/SAME_TOKEN_MULTIPLIER of the threshold (e.g. 0.05% for 0.5% threshold)
                                {
                                    // Solvency check for same token borrow/lend is way slower, so
                                    // we check if the gap is small enough to warrant it
                                    info!(borrower = %wallet, gap = %gap, ltv=%bh.ltv, "Skipping same token check");
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

            match ctx.mode {
                SolvencyMode::Standard => {
                    if collateral_token == debt_token {
                        same_token_solvency(ctx.clone(), borrower, debt_token).await;
                    } else {
                        standard_solvency(ctx.clone(), borrower, debt_token, collateral_token)
                            .await;
                    }
                }
                SolvencyMode::SimulatedPyth {
                    ref price_id,
                    ref price_feed,
                } => pyth_solvency(ctx.clone(), &price_id, &price_feed, borrower).await,
            }
        }
    }
}

async fn standard_solvency(
    ctx: LiquidationContext<impl Provider + Clone + 'static>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
) {
    let multicall = ctx
        .provider
        .multicall()
        .add(ctx.hook.maxLiquidation(borrower))
        .add(ctx.silo.isSolvent(borrower));

    let (liquidation, is_solvent) = match multicall.aggregate().await {
        Ok(result) => result,
        Err(e) => {
            error!(?borrower, "Multicall failed: {e}");
            return;
        }
    };

    if is_solvent {
        debug!(?borrower, "Standard check: solvent. Skipping.");
        return;
    }

    if !is_solvent
        && liquidation.maxDebt < U256::from(ctx.config.min_amounts_for_liquidation[&debt_token])
    {
        warn!(
            ?borrower,
            "Insolvent but below min debt threshold. Skipping."
        );
        ctx.watchlist
            .write()
            .await
            .retain(|(w, _, _)| *w != borrower);
        return;
    }

    info!(?borrower, max_debt = %liquidation.maxDebt, "🚨 Borrower is insolvent → preparing liquidation");

    liquidate_and_wait_for_receipt(
        ctx.clone(),
        borrower,
        debt_token,
        collateral_token,
        get_swap_data(
            &ctx,
            collateral_token,
            debt_token,
            liquidation.collateralToLiquidate,
        )
        .await,
        liquidation.maxDebt,
    )
    .await;
}

async fn same_token_solvency(
    ctx: LiquidationContext<impl Provider + Clone + 'static>,
    borrower: Address,
    debt_token: Address,
) {
    let multicall = ctx
        .provider
        .multicall()
        .add(ctx.hook.maxLiquidation(borrower))
        .add(ctx.silo.isSolvent(borrower));

    let (liquidation, is_solvent) = match multicall.aggregate().await {
        Ok(result) => result,
        Err(e) => {
            error!(?borrower, "Multicall failed: {e}");
            return;
        }
    };

    if is_solvent {
        // Get the current block number
        let block_number = match ctx.provider.get_block_number().await {
            Ok(num) => num,
            Err(_) => {
                error!(?borrower, "Failed to get current block number");
                return;
            }
        };

        // Get the current LTV
        let current_ltv = match ctx
            .lens
            .getLtv(ctx.config.silo_address, borrower)
            .call()
            .await
        {
            Ok(ltv) => ltv,
            Err(_) => {
                error!(?borrower, "Failed to get current LTV");
                return;
            }
        };

        let n_blocks_ago = 50;
        let n_blocks_to_check = 5;
        let delta_threshold = U256::from(50000000000 as u64);

        // Get the past LTV (block number - 100)
        let past_ltv = match ctx
            .lens
            .getLtv(ctx.config.silo_address, borrower)
            .block(BlockId::Number(BlockNumberOrTag::Number(
                block_number - n_blocks_ago,
            )))
            .call()
            .await
        {
            Ok(ltv) => ltv,
            Err(_) => {
                error!(?borrower, "Failed to get past LTV");
                return;
            }
        };

        // Calculate the delta and check if liquidation is necessary
        let delta_per_last_n_blocks = current_ltv
            .saturating_sub(past_ltv)
            .div_ceil(U256::from(n_blocks_ago));
        if delta_per_last_n_blocks > delta_threshold {
            warn!(
                %borrower,
                %ctx.config.silo_address,
                %delta_per_last_n_blocks,
                "LTV delta per last {} blocks is too high, skipping",
                n_blocks_ago,
            );
            return;
        }

        let delta_in_n_blocks =
            delta_per_last_n_blocks.saturating_mul(U256::from(n_blocks_to_check));
        if delta_in_n_blocks + current_ltv > ctx.liquidation_threshold {
            info!(%borrower, %ctx.config.silo_address, %current_ltv, %past_ltv, %delta_per_last_n_blocks, %delta_in_n_blocks,
                "🚨 Borrower is solvent, but LTV delta is high enough to start trying to liquidate. Simulated LTV: {}",
                delta_in_n_blocks + current_ltv
            );

            let mut liquidation_tentatives = 0;
            while liquidation_tentatives < 10 {
                liquidate_no_debt_and_no_wait(
                    ctx.clone(),
                    borrower,
                    debt_token,
                    debt_token,
                    Bytes::new(),
                )
                .await;

                sleep(Duration::from_millis(300)).await;
                liquidation_tentatives += 1;
                // current block time in sonic is around 500-600ms.
                // so trying 15 times with 300ms sleep it measn we cover ~ 5 seconds, which is more than
                // the 5 blocks with simulated LTV
            }

            return;
        }

        debug!(%borrower, %ctx.config.silo_address, %current_ltv, %past_ltv, %delta_per_last_n_blocks, %delta_in_n_blocks,
        "Borrower is solvent, LTV is too low to liquidate. Simulated LTV: {}", delta_in_n_blocks + current_ltv);
    } else {
        if liquidation.maxDebt < U256::from(ctx.config.min_amounts_for_liquidation[&debt_token]) {
            warn!(
                ?borrower,
                "Same token borrower insolvent but below min debt threshold. Skipping."
            );
            ctx.watchlist
                .write()
                .await
                .retain(|(w, _, _)| *w != borrower);
            return;
        }

        info!(
            ?borrower,
            "🚨 Same token Borrower is (already) insolvent → preparing liquidation"
        );

        let swap_data = Bytes::new(); // No swap needed for same token liquidation

        liquidate_and_wait_for_receipt(
            ctx.clone(),
            borrower,
            debt_token,
            debt_token,
            swap_data,
            liquidation.maxDebt,
        )
        .await;
    }
}

async fn pyth_solvency(
    ctx: LiquidationContext<impl Provider + Clone + 'static>,
    _price_id: &String,
    price_feed: &SharedPythPrice,
    borrower: Address,
) {
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
                if solvent {
                    info!(?borrower, "Still solvent after Pyth simulation.");
                    return;
                }
            }
            Err(e) => {
                error!(?borrower, "Pyth simulation failed: {e}");
                return;
            }
        }
    } else {
        warn!(?borrower, "No Pyth price, skipping simulation.");
        sleep(Duration::from_secs(5)).await;
        return;
    }
}

async fn get_swap_data(
    ctx: &LiquidationContext<impl Provider + Clone + 'static>,
    collateral_token: Address,
    debt_token: Address,
    collateral_to_liquidate: U256,
) -> Bytes {
    if collateral_token != debt_token {
        let amount_for_swap = collateral_to_liquidate * U256::from(995) / U256::from(1000);

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
    }
}

async fn liquidate_and_wait_for_receipt<P>(
    ctx: LiquidationContext<P>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    swap_data: Bytes,
    max_debt: U256,
) -> bool
where
    P: Provider + Clone + 'static,
{
    info!(collateral_token = ?collateral_token, debt_token = ?debt_token, ?borrower, "🚀 Sending liquidation tx...");
    let tx: alloy::contract::CallBuilder<
        &P,
        std::marker::PhantomData<crate::types::ILiquidationHelper::executeLiquidationCall>,
    > = ctx
        .helper
        .executeLiquidation(
            ctx.config.silo_address,
            debt_token,
            max_debt,
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
            info!(?receipt.block_number, %borrower, "✅ Tx succeeded");
            ctx.watchlist
                .write()
                .await
                .retain(|(w, _, _)| *w != borrower);
            return true;
        }
        Err(e) => {
            error!(?borrower, "❌ Liquidation tx failed: {e}");
            ctx.blacklist.write().await.insert(borrower);

            return false;
        }
    }
}

async fn liquidate_no_debt_and_no_wait(
    ctx: LiquidationContext<impl Provider + Clone + 'static>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    swap_data: Bytes,
) {
    match ctx
        .helper
        .executeLiquidationBlind(
            ctx.config.silo_address,
            ctx.config.hook_address,
            debt_token,
            LiquidationData {
                hook: ctx.config.hook_address,
                collateralAsset: collateral_token,
                borrower,
            },
            swap_data,
        )
        .gas(5_000_000u64)
        .gas_price(65_000_000_000u128)
        .send()
        .await
    {
        Ok(pending_tx) => {
            info!(
                ?borrower,
                "Liquidation (no wait) tx sent: {:?}",
                pending_tx.tx_hash()
            );
        }
        Err(e) => {
            error!(?borrower, "Failed to send liquidation (no wait) tx: {e}");
        }
    }
}
