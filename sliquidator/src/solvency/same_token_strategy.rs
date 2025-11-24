use std::time::Duration;

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{Address, Bytes, U256},
    providers::Provider,
};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::{
    solvency::common::{liquidate_no_wait, MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX},
    types::{IHookReceiver::maxLiquidationReturn, LiquidationContext, LiquidationType},
};

pub(crate) async fn same_token_solvency<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
    borrower: Address,
    debt_token: Address,
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
            error!(?borrower, "<same_token_solvency> multicall failed: {e}");
            return;
        }
    };

    if is_solvent {
        try_ltv_projection_liquidation(&ctx, borrower, debt_token).await;
    } else {
        try_direct_same_token_liquidation(&ctx, borrower, debt_token, liquidation).await;
    }
}

async fn try_ltv_projection_liquidation<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
    borrower: Address,
    token: Address,
) {
    let block_number = match ctx.provider.get_block_number().await {
        Ok(n) => n,
        Err(_) => {
            error!(?borrower, "Failed to get current block number");
            return;
        }
    };

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
    let n_blocks_to_check = 4;
    let delta_threshold = U256::from(50_000_000_000u64);

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

    let delta_per_block = current_ltv
        .saturating_sub(past_ltv)
        .div_ceil(U256::from(n_blocks_ago));

    if delta_per_block > delta_threshold {
        warn!(
            %borrower,
            %ctx.config.silo_address,
            %delta_per_block,
            "LTV delta per block exceeds threshold → skipping simulation"
        );
        return;
    }

    let projected_delta = delta_per_block.saturating_mul(U256::from(n_blocks_to_check));
    let projected_ltv = projected_delta + current_ltv;

    if projected_ltv > ctx.liquidation_threshold {
        info!(
            %borrower,
            %ctx.config.silo_address,
            %current_ltv,
            %past_ltv,
            %delta_per_block,
            %projected_ltv,
            "🚨 Simulated LTV exceeds threshold → executing proactive liquidations"
        );

        attempt_projected_liquidations(ctx, borrower, token).await;
    } else {
        debug!(
            %borrower,
            %ctx.config.silo_address,
            %current_ltv,
            %past_ltv,
            %delta_per_block,
            %projected_ltv,
            "Borrower solvent → simulated LTV not high enough to liquidate"
        );
    }
}

async fn try_direct_same_token_liquidation<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
    borrower: Address,
    token: Address,
    liquidation: maxLiquidationReturn,
) {
    let max_debt = liquidation.maxDebt;
    let threshold = U256::from(
        ctx.config.min_amounts_for_liquidation[MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX],
    );

    if max_debt < threshold {
        warn!(
            ?borrower,
            "Already insolvent but debt < threshold → skipping"
        );
        ctx.watchlist
            .write()
            .await
            .retain(|(w, _, _)| *w != borrower);
        return;
    }

    info!(?borrower, max_debt = %max_debt, "🚨 Already insolvent → executing standard liquidation");

    liquidate_no_wait(
        ctx.clone(),
        ctx.helpers[0].clone(),
        borrower,
        token,
        token, // no swap
        Bytes::new(),
        LiquidationType::Standard,
        true,
    )
    .await;

    // Remove the borrower from the watchlist after attempting liquidations
    // in order to avoid repeated attempts
    ctx.watchlist
        .write()
        .await
        .retain(|(w, _, _)| *w != borrower);
}

async fn attempt_projected_liquidations<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
    borrower: Address,
    token: Address,
) {
    let num_helpers = ctx.helpers.len();

    // current block time in sonic is around 500-600ms.
    // so trying 5 times with 500ms sleep it measn we cover ~ 3 seconds, which is more than
    // the 5 blocks with simulated LTV
    for i in 0..5 {
        let helper = ctx.helpers[i % num_helpers].clone();

        liquidate_no_wait(
            ctx.clone(),
            helper,
            borrower,
            token,
            token,
            Bytes::new(),
            LiquidationType::Blind,
            false,
        )
        .await;

        sleep(Duration::from_millis(400)).await;
    }

    // Remove the borrower from the watchlist after attempting liquidations
    // in order to avoid repeated attempts
    ctx.watchlist
        .write()
        .await
        .retain(|(w, _, _)| *w != borrower);
}
