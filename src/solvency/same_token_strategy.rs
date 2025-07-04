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
        liquidate_and_wait_for_receipt, liquidate_no_debt_and_no_wait,
        MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX,
    },
    types::LiquidationContext,
};

pub(crate) async fn same_token_solvency(
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
        if liquidation.maxDebt
            < U256::from(
                ctx.config.min_amounts_for_liquidation
                    [MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX],
            )
        {
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
