use alloy::{
    primitives::{Address, U256},
    providers::Provider,
};
use tracing::{debug, error, info, warn};

use crate::{
    solvency::common::{
        get_swap_data, liquidate_and_wait_for_receipt,
        MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX,
    },
    types::LiquidationContext,
};

pub(crate) async fn base_solvency(
    ctx: LiquidationContext<impl Provider + Clone + 'static>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
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
        debug!(?borrower, "Standard check: solvent. Skipping.");
        return;
    }

    if !is_solvent
        && liquidation.maxDebt
            < U256::from(
                ctx.config.min_amounts_for_liquidation
                    [MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX],
            )
    {
        warn!(
            ?borrower,
            "Insolvent but below min debt threshold. Skipping."
        );
        ctx.watchlist
            .write()
            .await
            .retain(|(w, _, _)| *w != borrower);
        return;
    }

    info!(?borrower, max_debt = %liquidation.maxDebt, "🚨 Borrower is insolvent → preparing liquidation");

    liquidate_and_wait_for_receipt(
        ctx.clone(),
        borrower,
        debt_token,
        collateral_token,
        get_swap_data(
            &ctx,
            collateral_token,
            debt_token,
            liquidation.collateralToLiquidate,
        )
        .await,
        liquidation.maxDebt,
    )
    .await;
}
