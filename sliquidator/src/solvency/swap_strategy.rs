use std::time::{Duration, SystemTime, UNIX_EPOCH};

use alloy::{
    primitives::{aliases::I24, Address, Bytes, FixedBytes, U160, U256},
    providers::Provider,
    sol_types::SolCall,
};
use common::oracles::chainlink::SharedChainlinkPrice;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::{
    config::LIQUIDATOR_ADDRESS,
    oracles::{
        hermes::{decode_pyth_update_b64, SharedPythPrice},
        simulator::{chainlink_is_borrower_solvent_simulated, pyth_is_borrower_solvent_simulated},
    },
    solvency::common::{
        liquidate_no_wait_spam, MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX,
        MIN_AMOUNT_FOR_LIQUIDATION_SIMULATED_STRATEGY_INDEX,
    },
    types::{
        IHookReceiver::maxLiquidationReturn, LiquidationContext, LiquidationType, OraclePriceData,
    },
};

pub(crate) async fn swap_solvency<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
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
                    ctx,
                    debt_token,
                    collateral_token,
                    &price_feed,
                    borrower,
                    slot_0,
                )
                .await
            }
            OraclePriceData::Redstone => {
                debug!(
                    ?borrower,
                    "Redstone oracle not supported for simulated swap solvency checks."
                );
            }
        }
    } else {
        try_direct_liquidation(&ctx, borrower, debt_token, collateral_token, liquidation).await;
    }
}

async fn try_chainlink_simulated_liquidation<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
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

                let swap_data = get_swap_data(
                    ctx.config.swap_pool_tick_spacing,
                    collateral_token,
                    debt_token,
                    collateral_to_liquidate,
                )
                .await;

                liquidate_no_wait_spam(
                    ctx,
                    borrower,
                    debt_token,
                    collateral_token,
                    swap_data,
                    LiquidationType::Standard,
                )
                .await;

                // Remove the borrower from the watchlist after attempting liquidations
                // in order to avoid repeated attempts
                ctx.watchlist
                    .write()
                    .await
                    .retain(|(w, _, _)| *w != borrower);
            } else {
                info!(?borrower, max_debt = %max_debt, min_threshold = ?min_threshold, "Simulated insolvency but below threshold. Skipping.");
                ctx.watchlist
                    .write()
                    .await
                    .retain(|(w, _, _)| *w != borrower);
                return;
            }
        }
        Err(e) => {
            error!(?borrower, "Simulation failed: {e}");
        }
    }
}

pub(crate) async fn try_pyth_simulated_liquidation<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
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
                        ctx.config.swap_pool_tick_spacing,
                        collateral_token,
                        debt_token,
                        collateral_to_liquidate,
                    )
                    .await;

                    let pyth_price_data = match decode_pyth_update_b64(&p.1) {
                        Ok(data) => data,
                        Err(e) => {
                            error!(?borrower, "Failed to decode Pyth price data: {e}");
                            return;
                        }
                    };

                    update_pyth_price_then_liquidate_no_wait(
                        ctx,
                        borrower,
                        debt_token,
                        collateral_token,
                        swap_data,
                        vec![pyth_price_data],
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

async fn try_direct_liquidation<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
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
        ctx.config.swap_pool_tick_spacing,
        collateral_token,
        debt_token,
        liquidation.collateralToLiquidate,
    )
    .await;

    liquidate_no_wait_spam(
        ctx,
        borrower,
        debt_token,
        collateral_token,
        swap_data,
        LiquidationType::Standard,
    )
    .await;

    // Remove the borrower from the watchlist after attempting liquidations
    // in order to avoid repeated attempts
    ctx.watchlist
        .write()
        .await
        .retain(|(w, _, _)| *w != borrower);
}

pub(crate) async fn update_pyth_price_then_liquidate_no_wait<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    swap_data: Bytes,
    price_data: Vec<Bytes>,
) {
    info!(collateral_token = ?collateral_token, debt_token = ?debt_token, ?borrower, "🚀 Sending Pyth liquidation tx...");

    liquidate_no_wait_spam(
        ctx,
        borrower,
        debt_token,
        collateral_token,
        swap_data,
        LiquidationType::Pyth(price_data),
    )
    .await;

    // Remove the borrower from the watchlist after attempting liquidation
    // in order to avoid repeated attempts
    ctx.watchlist
        .write()
        .await
        .retain(|(w, _, _)| *w != borrower);
}

async fn get_swap_data(
    tick_spacing: usize,
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
                tickSpacing: I24::unchecked_from(tick_spacing),
                recipient: LIQUIDATOR_ADDRESS,
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
