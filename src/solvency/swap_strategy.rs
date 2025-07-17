use std::time::Duration;

use alloy::{
    primitives::{Address, Bytes, FixedBytes, U256},
    providers::Provider,
};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::{
    oracles::{
        chainlink::SharedChainlinkPrice,
        hermes::SharedPythPrice,
        simulator::{chainlink_is_borrower_solvent_simulated, pyth_is_borrower_solvent_simulated},
    },
    solvency::common::{
        get_swap_data, liquidate_and_wait_for_receipt, liquidate_no_wait,
        update_pyth_price_then_liquidate_and_wait_for_receipt,
        MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX,
        MIN_AMOUNT_FOR_LIQUIDATION_SIMULATED_STRATEGY_INDEX,
    },
    types::{IHookReceiver::maxLiquidationReturn, LiquidationContext, OraclePriceData},
};

pub(crate) async fn swap_solvency(
    ctx: LiquidationContext<impl Provider + Clone + 'static>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
) {
    let (liquidation, is_solvent) = match ctx
        .provider
        .multicall()
        .add(ctx.hook.maxLiquidation(borrower))
        .add(ctx.silo.isSolvent(borrower))
        .block(alloy::eips::BlockId::Number(
            alloy::eips::BlockNumberOrTag::Latest,
        ))
        .aggregate()
        .await
    {
        Ok(res) => res,
        Err(e) => {
            error!(?borrower, "<swap_solvency> multicall failed: {e}");
            return;
        }
    };

    if is_solvent {
        match ctx.oracle_price_data {
            OraclePriceData::Chainlink {
                ref forwarded_price,
            } => {
                try_chainlink_simulated_liquidation(
                    &ctx,
                    borrower,
                    debt_token,
                    collateral_token,
                    forwarded_price,
                )
                .await;
            }
            OraclePriceData::Pyth {
                ref price_feed,
                ref slot_0,
                ..
            } => {
                try_pyth_simulated_liquidation(
                    ctx.clone(),
                    debt_token,
                    collateral_token,
                    &price_feed,
                    borrower,
                    slot_0,
                )
                .await
            }
        }
    } else {
        try_direct_liquidation(&ctx, borrower, debt_token, collateral_token, liquidation).await;
    }
}

async fn try_chainlink_simulated_liquidation<P: Provider + Clone + 'static>(
    ctx: &LiquidationContext<P>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    forwarded_price: &SharedChainlinkPrice,
) {
    let Some(price_data) = *forwarded_price.read().await else {
        debug!(?borrower, "Solvent now, but no forward price available.");
        return;
    };

    match chainlink_is_borrower_solvent_simulated(ctx, borrower, price_data.price).await {
        Ok((true, _, _)) => {
            debug!(?borrower, "Still solvent in simulation. Skipping.");
        }
        Ok((false, max_debt, collateral_to_liquidate)) => {
            debug!(?borrower, max_debt = %max_debt, "Insolvent in simulation.");

            let min_threshold = U256::from(
                ctx.config.min_amounts_for_liquidation
                    [MIN_AMOUNT_FOR_LIQUIDATION_SIMULATED_STRATEGY_INDEX],
            );

            if max_debt >= min_threshold {
                info!(?borrower, max_debt = %max_debt, next_price = %price_data.price,
                      "🚨 Simulated insolvency above threshold. Attempting proactive liquidation.");

                attempt_multiple_liquidations(
                    ctx,
                    borrower,
                    debt_token,
                    collateral_token,
                    max_debt,
                    collateral_to_liquidate,
                    price_data.max_priority_fee_per_gas,
                    price_data.max_fee_per_gas,
                )
                .await;
            } else {
                debug!(?borrower, max_debt = %max_debt, "Simulated insolvency but below threshold. Skipping.");
            }
        }
        Err(e) => {
            error!(?borrower, "Simulation failed: {e}");
        }
    }
}

pub(crate) async fn try_pyth_simulated_liquidation(
    ctx: LiquidationContext<impl Provider + Clone + 'static>,
    debt_token: Address,
    collateral_token: Address,
    price_feed: &SharedPythPrice,
    borrower: Address,
    slot_0: &FixedBytes<32>,
) {
    if let Some(p) = &*price_feed.read().await {
        match pyth_is_borrower_solvent_simulated(
            &ctx,
            borrower,
            p.0.publish_time,
            p.0.expo,
            p.0.price,
            p.0.conf,
            slot_0,
        )
        .await
        {
            Ok((solvent, max_debt, collateral_to_liquidate)) => {
                if solvent {
                    debug!(?borrower, "Still solvent after Pyth simulation.");
                    return;
                } else {
                    let base_threshold = U256::from(
                        ctx.config.min_amounts_for_liquidation
                            [MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX],
                    );

                    if max_debt < base_threshold {
                        warn!(?borrower, %ctx.config.silo_address,  max_debt = %max_debt, "Insolvent but below base threshold. Skipping.");
                        ctx.watchlist
                            .write()
                            .await
                            .retain(|(w, _, _)| *w != borrower);
                        return;
                    }

                    info!(
                        ?borrower,
                        "🚨 Detected insolvency after Pyth simulation - executing liquidation"
                    );

                    let swap_data = get_swap_data(
                        &ctx.helpers[0].clone(),
                        collateral_token,
                        debt_token,
                        collateral_to_liquidate,
                    )
                    .await;

                    update_pyth_price_then_liquidate_and_wait_for_receipt(
                        ctx.clone(),
                        ctx.helpers[0].clone(),
                        borrower,
                        debt_token,
                        collateral_token,
                        swap_data,
                        p.1.clone()
                            .into_iter()
                            .map(|b| Bytes::copy_from_slice(b.as_bytes()))
                            .collect::<Vec<_>>(),
                    )
                    .await;
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

async fn try_direct_liquidation<P: Provider + Clone + 'static>(
    ctx: &LiquidationContext<P>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    liquidation: maxLiquidationReturn,
) {
    let max_debt = liquidation.maxDebt;
    let base_threshold = U256::from(
        ctx.config.min_amounts_for_liquidation[MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX],
    );

    if max_debt < base_threshold {
        warn!(?borrower, %ctx.config.silo_address, max_debt = %max_debt, "Insolvent but below base threshold. Skipping.");
        ctx.watchlist
            .write()
            .await
            .retain(|(w, _, _)| *w != borrower);
        return;
    }

    info!(?borrower, max_debt = %max_debt, "🚨 Borrower is insolvent → proceeding with direct liquidation.");

    let swap_data = get_swap_data(
        &ctx.helpers[0].clone(),
        collateral_token,
        debt_token,
        liquidation.collateralToLiquidate,
    )
    .await;

    liquidate_and_wait_for_receipt(
        ctx.clone(),
        ctx.helpers[0].clone(),
        borrower,
        debt_token,
        collateral_token,
        swap_data,
        max_debt,
    )
    .await;
}

async fn attempt_multiple_liquidations<P: Provider + Clone + 'static>(
    ctx: &LiquidationContext<P>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    max_debt_to_cover: U256,
    collateral_to_liquidate: U256,
    max_priority_fee_per_gas: Option<u128>,
    max_fee_per_gas: u128,
) {
    let num_helpers = ctx.helpers.len();

    for i in 0..5 {
        let helper = ctx.helpers[i % num_helpers].clone();

        let swap_data = get_swap_data(
            &helper,
            collateral_token,
            debt_token,
            collateral_to_liquidate,
        )
        .await;

        liquidate_no_wait(
            ctx.clone(),
            helper,
            borrower,
            debt_token,
            collateral_token,
            Some(max_debt_to_cover),
            swap_data.clone(),
            max_priority_fee_per_gas,
            max_fee_per_gas,
        )
        .await;

        sleep(Duration::from_millis(20)).await;
    }

    // Remove the borrower from the watchlist after attempting liquidations
    // in order to avoid repeated attempts
    ctx.watchlist
        .write()
        .await
        .retain(|(w, _, _)| *w != borrower);
}
