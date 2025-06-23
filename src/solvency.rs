use std::{
    collections::HashSet,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy::{
    primitives::{address, Address, Bytes, U256},
    providers::Provider,
    sol_types::SolCall,
};
use eyre::eyre;
use tokio::{
    sync::RwLock,
    time::{sleep, Duration},
};
use tracing::{debug, error, info, warn};

use crate::{
    config::SiloConfig,
    types::{
        IRouter::{swapExactTokensForTokensCall, IRouterInstance},
        ISiloConfig::ISiloConfigInstance,
        Route,
    },
    utils::scale_to_wei,
};
use crate::{
    types::{
        Borrower, IHookReceiver, ILiquidationHelper::ILiquidationHelperInstance, ISilo,
        ISiloConfig, ISiloLens::ISiloLensInstance, LiquidationData, SharedAddressList,
    },
    utils::percent_to_wei,
};

const GET_USERS_HEALTH_BATCH_SIZE: usize = 50;

const ONLY_SAME_TOKEN: bool = false;

pub async fn solvency_loop<P>(
    provider: P,
    lens: ISiloLensInstance<P>,
    router: IRouterInstance<P>,
    helper: ILiquidationHelperInstance<P>,
    borrowers: SharedAddressList,
    silo_config: &SiloConfig,
    full_refresh_time_secs: u64,
    fast_check_solvency_threshold: f64,
    near_insolvency_refresh_time_millis: u64,
) -> eyre::Result<()>
where
    P: Provider + Clone + 'static,
{
    let hook = IHookReceiver::new(silo_config.hook_address, provider.clone());
    let config = ISiloConfig::new(silo_config.config_address, provider.clone());
    let silo = ISilo::new(silo_config.silo_address, provider.clone());

    let blacklist: Arc<RwLock<HashSet<Address>>> = Arc::new(RwLock::new(HashSet::new()));
    let blacklist_clone = Arc::clone(&blacklist);

    let watchlist: Arc<RwLock<Vec<(Address, Address, Address)>>> =
        Arc::new(RwLock::new(Vec::new()));
    let watchlist_clone = Arc::clone(&watchlist);

    let wei_fast_check_threshold = percent_to_wei(fast_check_solvency_threshold);

    // TODO understand 0xdec0000000000000000000000000000000000001
    blacklist
        .write()
        .await
        .insert(address!("0xdec0000000000000000000000000000000000001"));

    tokio::spawn(refresh_borrowers_health(
        silo_config.silo_address,
        lens,
        config,
        borrowers,
        watchlist_clone,
        blacklist_clone,
        full_refresh_time_secs,
        wei_fast_check_threshold,
    ));

    loop {
        sleep(Duration::from_millis(near_insolvency_refresh_time_millis)).await;

        let mut to_watch = {
            let lock = watchlist.read().await;
            lock.clone()
        };

        if to_watch.is_empty() {
            continue;
        }

        for (borrower, collateral_token, debt_token) in to_watch.drain(..) {
            let skip = {
                let guard = blacklist.read().await;
                guard.contains(&borrower)
            };
            if skip {
                continue;
            }

            let multicall = provider
                .multicall()
                .add(hook.maxLiquidation(borrower))
                .add(silo.isSolvent(borrower));

            let (liquidation, is_solvent) = match multicall.aggregate().await {
                Ok(result) => result,
                Err(e) => {
                    error!("Multicall failed for borrower {borrower:?}: {e}");
                    continue;
                }
            };

            if is_solvent {
                debug!("Borrower {borrower:?} is solvent, skipping liquidation check");
                continue;
            }

            if !is_solvent
                && liquidation.maxDebt
                    < U256::from(silo_config.min_amounts_for_liquidation[&debt_token])
            {
                warn!(
                    "Borrower {borrower:?} is insolvent but max debt ({}) is below the threshold ({}). Skipping.",
                    liquidation.maxDebt, silo_config.min_amounts_for_liquidation[&debt_token]
                );
                watchlist.write().await.retain(|(w, _, _)| *w != borrower);
                continue;
            }

            info!("🚨 Borrower {borrower:?} is insolvent → preparing liquidation");

            let swap_data = if collateral_token != debt_token {
                let amount_for_swap =
                    liquidation.collateralToLiquidate * U256::from(99) / U256::from(100);

                let adjusted_amount = scale_to_wei(
                    amount_for_swap,
                    silo_config.token_decimals[&collateral_token],
                );

                let (amount_out, stable) = match router
                    .getAmountOut(adjusted_amount, collateral_token, debt_token)
                    .call()
                    .await
                {
                    Ok(res) => (res.amount, res.stable),
                    Err(e) => {
                        error!("Router getAmountOut failed for {collateral_token:?} to {debt_token:?} with amount {adjusted_amount:?}: {e}");
                        continue;
                    }
                };

                let call = swapExactTokensForTokensCall {
                    amountIn: adjusted_amount,
                    amountOutMin: amount_out * U256::from(95) / U256::from(100),
                    routes: vec![Route {
                        from: collateral_token,
                        to: debt_token,
                        stable: stable,
                    }],
                    to: *helper.address(),
                    deadline: U256::from(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .expect("Time went backwards")
                            .as_secs()
                            + 300,
                    ),
                };

                Bytes::from(call.abi_encode())
            } else {
                Vec::new().into()
            };

            let tx = helper
                .executeLiquidation(
                    silo_config.silo_address,
                    debt_token,
                    liquidation.maxDebt,
                    LiquidationData {
                        hook: silo_config.hook_address,
                        collateralAsset: collateral_token,
                        borrower: borrower,
                    },
                    swap_data,
                )
                .gas(5_000_000u64)
                .gas_price(65_000_000_000u128); // 65 gwei

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
                    info!("✅ Tx succeeded in block {:?}", receipt.block_number);
                }
                Err(e) => {
                    error!("❌ Liquidation tx failed for borrower {borrower:?}: {e}");
                    blacklist.write().await.insert(borrower);
                }
            }
        }
    }
}

async fn refresh_borrowers_health<P>(
    silo_address: Address,
    lens: ISiloLensInstance<P>,
    config: ISiloConfigInstance<P>,
    borrowers: SharedAddressList,
    watchlist: Arc<RwLock<Vec<(Address, Address, Address)>>>,
    blacklist: Arc<RwLock<HashSet<Address>>>,
    full_refresh_time_secs: u64,
    wei_fast_check_threshold: U256,
) -> eyre::Result<()>
where
    P: Provider + Clone + 'static,
{
    loop {
        info!("⏳ Refreshing borrower list for silo {silo_address}...");
        let mut new_watchlist = Vec::new();
        let blacklist_guard = blacklist.read().await;

        let borrowers_guard = borrowers.read().await;
        let borrower_inputs: Vec<Borrower> = borrowers_guard
            .iter()
            .filter(|b| !blacklist_guard.contains(*b)) // might need Arc<Mutex<>> for this
            .map(|&wallet| Borrower {
                silo: silo_address,
                wallet,
            })
            .collect();

        drop(borrowers_guard);
        drop(blacklist_guard);

        for batch in borrower_inputs.chunks(GET_USERS_HEALTH_BATCH_SIZE) {
            if let Ok(healths) = lens.getUsersHealth(batch.to_vec()).call().await {
                for (i, bh) in healths.iter().enumerate() {
                    let wallet = batch[i].wallet;
                    let gap = bh.lt.saturating_sub(bh.ltv);
                    if gap <= wei_fast_check_threshold {
                        if let Ok(config_data) = config.getConfigsForSolvency(wallet).call().await {
                            if (!ONLY_SAME_TOKEN
                                || config_data.collateralConfig.token
                                    == config_data.debtConfig.token)
                                && !bh.lt.is_zero()
                                && !bh.ltv.is_zero()
                            {
                                info!(
                                    "Silo {silo_address}: borrower {wallet:?} is near insolvency: LTV = {}, LT = {}, gap = {}",
                                    bh.ltv, bh.lt, gap
                                );
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
            let mut lock = watchlist.write().await;
            *lock = new_watchlist;
        }

        sleep(Duration::from_secs(full_refresh_time_secs)).await;
    }
}
