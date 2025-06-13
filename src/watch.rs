use std::time::Instant;

use alloy::{
    primitives::{address, Address, U256},
    providers::Provider,
};
use chrono::Local;
use tokio::time::{sleep, Duration};
use tracing::{debug, error, info};

use crate::types::{
    Borrower, IHookReceiver, ILiquidationHelper::ILiquidationHelperInstance, ISilo, ISiloConfig,
    ISiloLens::ISiloLensInstance, LiquidationData,
};
use crate::{config::SiloConfig, odos::get_odos_swap_data};

const USDC_ADDRESS: Address = address!("0x29219dd400f2bf60e5a23d13be72b486d4038894");

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
                .map(|&wallet| Borrower {
                    silo: silo_config.silo_address,
                    wallet,
                })
                .collect();

            match lens.getUsersHealth(borrower_inputs.clone()).call().await {
                Ok(healths) => {
                    for (i, bh) in healths.iter().enumerate() {
                        let gap = bh.lt.saturating_sub(bh.ltv);
                        if gap <= near_insolvent_threshold {
                            info!(
                                "Borrower {} near insolvency: lt={}, ltv={}, gap={}",
                                borrower_inputs[i].wallet, bh.lt, bh.ltv, gap
                            );
                            watchlist.push((borrower_inputs[i].wallet, bh.lt, bh.ltv, gap));
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to get borrower health: {e}");
                    continue;
                }
            }
        }

        if watchlist.is_empty() {
            info!("No borrowers near solvency to monitor.");
            sleep(Duration::from_secs(5)).await;
            continue;
        }

        for (borrower, _, _, _) in &watchlist {
            let multicall = provider
                .multicall()
                .add(hook.maxLiquidation(*borrower))
                .add(config.getConfigsForSolvency(*borrower))
                .add(silo.isSolvent(*borrower));

            let (liquidation, config_data, is_solvent) = match multicall.aggregate().await {
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
                continue;
            }

            info!("🚨 Borrower {borrower:?} is insolvent → preparing liquidation");

            let collateral_token = config_data.collateralConfig.token;
            let debt_token = config_data.debtConfig.token;

            let amount_for_quote =
                liquidation.collateralToLiquidate * U256::from(99) / U256::from(100);

            let adjusted_amount = if debt_token == USDC_ADDRESS {
                amount_for_quote * U256::from(10u64).pow(U256::from(12))
            } else {
                amount_for_quote
            };

            let swap_data = if collateral_token != debt_token {
                get_odos_swap_data(
                    helper.address(),
                    &collateral_token,
                    &debt_token,
                    adjusted_amount,
                )
                .await?
                .unwrap_or_default()
                .into()
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
                        borrower: *borrower,
                    },
                    swap_data,
                )
                .gas(2_500_000u64)
                .gas_price(55_000_000_000u128); // 55 gwei

            match tx.send().await?.get_receipt().await {
                Ok(receipt) => {
                    info!(
                        "✅ Executed liquidation at block {:?}",
                        receipt.block_number
                    );
                }
                Err(e) => {
                    error!("Failed to confirm tx for borrower {borrower:?}: {e}");
                }
            }
        }

        debug!(
            "{} ⏱️  Monitoring {} borrowers",
            Local::now().format("[%H:%M:%S]"),
            watchlist.len()
        );

        sleep(Duration::from_millis(near_insolvency_refresh_time_millis)).await;
    }
}
