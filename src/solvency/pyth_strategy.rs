use std::time::Duration;

use alloy::{primitives::Address, providers::Provider};
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::{
    oracles::{hermes::SharedPythPrice, simulator::pyth_is_borrower_solvent_simulated},
    types::LiquidationContext,
};

pub(crate) async fn pyth_solvency(
    ctx: LiquidationContext<impl Provider + Clone + 'static>,
    _price_id: &String,
    price_feed: &SharedPythPrice,
    borrower: Address,
) {
    if let Some(p) = &*price_feed.read().await {
        match pyth_is_borrower_solvent_simulated(
            &ctx,
            borrower,
            p.publish_time,
            p.expo,
            p.price,
            p.conf,
        )
        .await
        {
            Ok((solvent, _max_debt)) => {
                if solvent {
                    info!(?borrower, "Still solvent after Pyth simulation.");
                    return;
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
