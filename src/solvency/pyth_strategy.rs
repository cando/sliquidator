use std::time::Duration;

use alloy::{
    primitives::{Address, FixedBytes, U256},
    providers::Provider,
};
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::{
    oracles::{hermes::SharedPythPrice, simulator::pyth_is_borrower_solvent_simulated},
    solvency::common::{
        get_swap_data, update_pyth_price_then_liquidate_and_wait_for_receipt,
        MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX,
    },
    types::LiquidationContext,
};

pub(crate) async fn pyth_solvency(
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
            p.publish_time,
            p.expo,
            p.price,
            p.conf,
            slot_0,
        )
        .await
        {
            Ok((solvent, max_debt, collateral_to_liquidate)) => {
                if solvent {
                    info!(?borrower, "Still solvent after Pyth simulation.");
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
                        max_debt,
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
