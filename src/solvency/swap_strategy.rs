use std::time::Duration;

use alloy::{
    primitives::{Address, Bytes, U256},
    providers::Provider,
};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::{
    oracles::{
        chainlink::SharedChainlinkPrice, simulator::chainlink_is_borrower_solvent_simulated,
    },
    solvency::common::{
        get_swap_data, liquidate_and_wait_for_receipt, liquidate_no_debt_and_no_wait,
        MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX,
        MIN_AMOUNT_FOR_LIQUIDATION_SIMULATED_STRATEGY_INDEX,
    },
    types::{IHookReceiver::maxLiquidationReturn, LiquidationContext},
};

pub(crate) async fn swap_solvency(
    ctx: LiquidationContext<impl Provider + Clone + 'static>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    forwarded_price: &SharedChainlinkPrice,
) {
    let (liquidation, is_solvent) = match ctx
        .provider
        .multicall()
        .add(ctx.hook.maxLiquidation(borrower))
        .add(ctx.silo.isSolvent(borrower))
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
        try_proactive_simulated_liquidation(
            &ctx,
            borrower,
            debt_token,
            collateral_token,
            forwarded_price,
        )
        .await;
    } else {
        try_direct_liquidation(&ctx, borrower, debt_token, collateral_token, liquidation).await;
    }
}

async fn try_proactive_simulated_liquidation<P: Provider + Clone + 'static>(
    ctx: &LiquidationContext<P>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    forwarded_price: &SharedChainlinkPrice,
) {
    let Some(price) = *forwarded_price.read().await else {
        debug!(?borrower, "Solvent now, but no forward price available.");
        return;
    };

    match chainlink_is_borrower_solvent_simulated(ctx, borrower, price).await {
        Ok((true, _)) => {
            debug!(?borrower, "Still solvent in simulation. Skipping.");
        }
        Ok((false, max_debt)) => {
            debug!(?borrower, max_debt = %max_debt, "Insolvent in simulation.");

            let min_threshold = U256::from(
                ctx.config.min_amounts_for_liquidation
                    [MIN_AMOUNT_FOR_LIQUIDATION_SIMULATED_STRATEGY_INDEX],
            );

            if max_debt >= min_threshold {
                info!(?borrower, max_debt = %max_debt, next_price = %price,
                      "🚨 Simulated insolvency above threshold. Attempting proactive liquidation.");

                attempt_multiple_liquidations(ctx, borrower, debt_token, collateral_token).await;
            } else {
                debug!(?borrower, max_debt = %max_debt, "Simulated insolvency but below threshold. Skipping.");
            }
        }
        Err(e) => {
            error!(?borrower, "Simulation failed: {e}");
        }
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
        warn!(?borrower, max_debt = %max_debt, "Insolvent but below base threshold. Skipping.");
        ctx.watchlist
            .write()
            .await
            .retain(|(w, _, _)| *w != borrower);
        return;
    }

    info!(?borrower, max_debt = %max_debt, "🚨 Borrower is insolvent → proceeding with direct liquidation.");

    let swap_data = get_swap_data(
        ctx,
        collateral_token,
        debt_token,
        liquidation.collateralToLiquidate,
    )
    .await;

    liquidate_and_wait_for_receipt(
        ctx.clone(),
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
) {
    // current block time in sonic is around 500-600ms.
    // so trying 10 times with 100ms sleep it measn we cover ~ 1 seconds,
    // which is ~ 2 blocks where we hope to catch the chainlink Forward transaction
    for _ in 0..10 {
        liquidate_no_debt_and_no_wait(
            ctx.clone(),
            borrower,
            debt_token,
            collateral_token,
            Bytes::new(),
        )
        .await;

        sleep(Duration::from_millis(100)).await;
    }
}
