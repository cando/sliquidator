use std::{collections::HashSet, time::Instant};

use alloy::{
    primitives::{address, Address, U256},
    providers::Provider,
};
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info};

use crate::types::{
    Borrower, IHookReceiver, ILiquidationHelper::ILiquidationHelperInstance, ISilo, ISiloConfig,
    ISiloLens::ISiloLensInstance, LiquidationData,
};
use crate::{config::SiloConfig, odos::get_odos_swap_data};

const USDC_ADDRESS: Address = address!("0x29219dd400f2bf60e5a23d13be72b486d4038894");
const GET_USERS_HEALTH_BATCH_SIZE: usize = 50;

const ONLY_SAME_TOKEN: bool = false;

pub async fn watch_loop<P>(
    provider: P,
    lens: ISiloLensInstance<P>,
    helper: ILiquidationHelperInstance<P>,
    borrowers: Vec<Address>,
    silo_config: &SiloConfig,
    full_refresh_time_secs: u64,
    near_insolvent_threshold: U256,
    near_insolvency_refresh_time_millis: u64,
) -> eyre::Result<()>
where
    P: Provider + Clone,
{
    let hook = IHookReceiver::new(silo_config.hook_address, provider.clone());
    let config = ISiloConfig::new(silo_config.config_address, provider.clone());
    let silo = ISilo::new(silo_config.silo_address, provider.clone());

    let mut watchlist = Vec::new();
    let mut blacklist: HashSet<Address> = HashSet::new();
    let mut last_refresh: Option<Instant> = None;

    loop {
        let should_refresh = match last_refresh {
            None => true,
            Some(instant) => instant.elapsed().as_secs() >= full_refresh_time_secs,
        };

        if should_refresh {
            last_refresh = Some(Instant::now());
            info!("Refreshing borrower list...");
            watchlist.clear();

            let borrower_inputs: Vec<Borrower> = borrowers
                .iter()
                .filter(|b| !blacklist.contains(*b))
                .map(|&wallet| Borrower {
                    silo: silo_config.silo_address,
                    wallet,
                })
                .collect();

            for batch in borrower_inputs.chunks(GET_USERS_HEALTH_BATCH_SIZE) {
                match lens.getUsersHealth(batch.to_vec()).call().await {
                    Ok(healths) => {
                        for (i, bh) in healths.iter().enumerate() {
                            let wallet = batch[i].wallet;
                            let gap = bh.lt.saturating_sub(bh.ltv);
                            if gap <= near_insolvent_threshold {
                                match config.getConfigsForSolvency(wallet).call().await {
                                    Ok(config_data) => {
                                        if !ONLY_SAME_TOKEN
                                            || config_data.collateralConfig.token
                                                == config_data.debtConfig.token
                                        {
                                            info!(
                                                "Borrower {} near insolvency: lt={}, ltv={}, gap={}",
                                                wallet, bh.lt, bh.ltv, gap
                                            );
                                            watchlist.push((
                                                wallet,
                                                config_data.collateralConfig.token,
                                                config_data.debtConfig.token,
                                            ));
                                        } else {
                                            debug!("Skipping near insolvency borrower {}: collateral and debt tokens are different", wallet);
                                        }
                                    }
                                    Err(e) => {
                                        error!("Failed to get config for borrower {wallet:?}: {e}");
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Failed to get user health for batch: {e}");
                        continue;
                    }
                }
            }
        }

        if watchlist.is_empty() {
            info!("No borrowers near solvency to monitor.");
            sleep(Duration::from_secs(30)).await;
            continue;
        } else {
            sleep(Duration::from_millis(near_insolvency_refresh_time_millis)).await;
        }

        let mut next_watchlist = Vec::new();

        for (borrower, collateral_token, debt_token) in watchlist.drain(..) {
            if blacklist.contains(&borrower) {
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

            if is_solvent
                || liquidation.collateralToLiquidate.is_zero()
                || liquidation.maxDebt.is_zero()
            {
                next_watchlist.push((borrower, collateral_token, debt_token));
                continue;
            }

            info!("🚨 Borrower {borrower:?} is insolvent → preparing liquidation");

            let amount_for_quote =
                liquidation.collateralToLiquidate * U256::from(99) / U256::from(100);

            let adjusted_amount = if collateral_token == USDC_ADDRESS {
                amount_for_quote * U256::from(10u64).pow(U256::from(12))
            } else {
                amount_for_quote
            };
            let swap_data = if collateral_token != debt_token {
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
                        blacklist.insert(borrower);
                    }
                }
                Err(e) => {
                    error!("Failed to confirm tx for borrower {borrower:?}: {e}");
                    blacklist.insert(borrower);
                }
            }
        }

        watchlist = next_watchlist;
    }
}
