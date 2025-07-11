use std::time::Duration;

use alloy::{
    eips::{BlockId, BlockNumberOrTag},
    primitives::{Address, Bytes, U256},
    providers::Provider,
};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::{
    solvency::common::{
        liquidate_and_wait_for_receipt, liquidate_no_wait,
        MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX,
    },
    types::{IHookReceiver::maxLiquidationReturn, LiquidationContext},
};

pub(crate) async fn same_token_solvency(
    ctx: LiquidationContext<impl Provider + Clone + 'static>,
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

async fn try_ltv_projection_liquidation<P: Provider + Clone + 'static>(
    ctx: &LiquidationContext<P>,
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

        attempt_multiple_liquidations(ctx, borrower, token).await;
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

async fn try_direct_same_token_liquidation<P: Provider + Clone + 'static>(
    ctx: &LiquidationContext<P>,
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

    liquidate_and_wait_for_receipt(
        ctx.clone(),
        borrower,
        token,
        token, // no swap
        Bytes::new(),
        max_debt,
    )
    .await;
}

async fn attempt_multiple_liquidations<P: Provider + Clone + 'static>(
    ctx: &LiquidationContext<P>,
    borrower: Address,
    token: Address,
) {
    // current block time in sonic is around 500-600ms.
    // so trying 5 times with 500ms sleep it measn we cover ~ 3 seconds, which is more than
    // the 5 blocks with simulated LTV
    for _ in 0..5 {
        liquidate_no_wait(ctx.clone(), borrower, token, token, None, Bytes::new()).await;

        sleep(Duration::from_millis(600)).await;
    }
}
