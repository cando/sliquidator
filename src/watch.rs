use std::{collections::HashSet, sync::Arc};

use alloy::{
    primitives::{address, Address, U256},
    providers::Provider,
};
use tokio::{
    sync::RwLock,
    time::{sleep, Duration},
};
use tracing::{error, info, warn};

use crate::types::{
    Borrower, IHookReceiver, ILiquidationHelper::ILiquidationHelperInstance, ISilo, ISiloConfig,
    ISiloLens::ISiloLensInstance, LiquidationData,
};
use crate::{config::SiloConfig, odos::get_odos_swap_data};

const GET_USERS_HEALTH_BATCH_SIZE: usize = 50;

const ONLY_SAME_TOKEN: bool = false;

pub async fn watch_loop<P>(
    provider: P,
    lens: ISiloLensInstance<P>,
    helper: ILiquidationHelperInstance<P>,
    borrowers: Vec<Address>,
    silo_config: &SiloConfig,
    full_refresh_time_secs: u64,
    near_insolvency_refresh_time_millis: u64,
) -> eyre::Result<()>
where
    P: Provider + Clone + 'static,
{
    let hook = IHookReceiver::new(silo_config.hook_address, provider.clone());
    let config = ISiloConfig::new(silo_config.config_address, provider.clone());
    let silo = ISilo::new(silo_config.silo_address, provider.clone());
    let silo_address = silo_config.silo_address.clone();

    let blacklist: Arc<RwLock<HashSet<Address>>> = Arc::new(RwLock::new(HashSet::new()));
    let blacklist_clone = Arc::clone(&blacklist);

    let watchlist: Arc<RwLock<Vec<(Address, Address, Address)>>> =
        Arc::new(RwLock::new(Vec::new()));
    let watchlist_clone = Arc::clone(&watchlist);

    // TODO undestardn 0xdec0000000000000000000000000000000000001
    blacklist
        .write()
        .await
        .insert(address!("0xdec0000000000000000000000000000000000001"));

    tokio::spawn(async move {
        loop {
            info!("⏳ Refreshing borrower list for silo {silo_address}...");
            let mut new_watchlist = Vec::new();
            let blacklist = blacklist_clone.read().await;

            let borrower_inputs: Vec<Borrower> = borrowers
                .iter()
                .filter(|b| !blacklist.contains(*b)) // might need Arc<Mutex<>> for this
                .map(|&wallet| Borrower {
                    silo: silo_address,
                    wallet,
                })
                .collect();

            drop(blacklist);

            for batch in borrower_inputs.chunks(GET_USERS_HEALTH_BATCH_SIZE) {
                if let Ok(healths) = lens.getUsersHealth(batch.to_vec()).call().await {
                    for (i, bh) in healths.iter().enumerate() {
                        let wallet = batch[i].wallet;
                        let gap = bh.lt.saturating_sub(bh.ltv);
                        if gap <= U256::from(1500000) {
                            // TODO: make this configurable
                            if let Ok(config_data) =
                                config.getConfigsForSolvency(wallet).call().await
                            {
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
                let mut lock = watchlist_clone.write().await;
                *lock = new_watchlist;
            }

            sleep(Duration::from_secs(full_refresh_time_secs)).await;
        }
    });

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
                    sleep(Duration::from_millis(
                        near_insolvency_refresh_time_millis * 10,
                    ))
                    .await;
                    continue;
                }
            };

            if is_solvent {
                continue;
            }

            if !is_solvent
                && liquidation.maxDebt < U256::from(silo_config.min_amount_for_liquidation)
            {
                warn!(
                    "Borrower {borrower:?} is insolvent but max debt ({}) is below the threshold ({}). Skipping.",
                    liquidation.maxDebt, silo_config.min_amount_for_liquidation
                );
                continue;
            }

            info!("🚨 Borrower {borrower:?} is insolvent → preparing liquidation");

            let amount_for_quote =
                liquidation.collateralToLiquidate * U256::from(99) / U256::from(100);

            let swap_data = if collateral_token != debt_token {
                let adjusted_amount = scale_to_wei(
                    amount_for_quote,
                    silo_config.token_decimals[&collateral_token],
                );
                info!(
                    "Fetching swap data for collateral token {} and debt token {} with amount {}",
                    collateral_token, debt_token, adjusted_amount
                );

                let result = get_odos_swap_data(
                    helper.address(),
                    &collateral_token,
                    &debt_token,
                    adjusted_amount,
                )
                .await?;

                let swap_bytes = result.unwrap_or_default();
                if swap_bytes.is_empty() {
                    continue;
                }

                swap_bytes.into()
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
                .gas(2_500_000u64)
                .gas_price(55_000_000_000u128); // 55 gwei

            match tx.send().await?.get_receipt().await {
                Ok(receipt) => {
                    if receipt.status() {
                        info!("✅ Tx succeeded in block {:?}", receipt.block_number);
                    } else {
                        error!("❌ Tx reverted");
                        blacklist.write().await.insert(borrower);
                    }
                }
                Err(e) => {
                    error!("Failed to confirm tx for borrower {borrower:?}: {e}");
                    blacklist.write().await.insert(borrower);
                }
            }
        }
    }
}

fn scale_to_wei(amount: U256, token_decimals: u8) -> U256 {
    match token_decimals {
        18 => amount,
        d if d < 18 => {
            let scale = 10u128.pow((18 - d) as u32);
            amount * U256::from(scale)
        }
        d => {
            let scale = 10u128.pow((d - 18) as u32);
            amount / U256::from(scale)
        }
    }
}
