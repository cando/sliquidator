use alloy::{
    primitives::{Address, Bytes, U256},
    providers::Provider,
};
use tracing::{error, info};

use crate::types::{
    ILiquidationHelper::ILiquidationHelperInstance, LiquidationContext, LiquidationData,
};

pub(crate) const MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX: usize = 0;
pub(crate) const MIN_AMOUNT_FOR_LIQUIDATION_SIMULATED_STRATEGY_INDEX: usize = 1;

pub(crate) const DEFAULT_MAX_FEE_PER_GAS: u128 = 65_000_000_000;

pub(crate) async fn liquidate_no_wait<P>(
    ctx: LiquidationContext<P>,
    helper: ILiquidationHelperInstance<P>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    max_debt: Option<U256>,
    swap_data: Bytes,
    max_priority_fee_per_gas: Option<u128>,
    max_fee_per_gas: u128,
) where
    P: Provider + Clone + 'static,
{
    let liquidation_call = if let Some(max_debt_to_cover) = max_debt {
        helper
            .executeLiquidation(
                ctx.config.silo_address,
                debt_token,
                max_debt_to_cover,
                LiquidationData {
                    hook: ctx.config.hook_address,
                    collateralAsset: collateral_token,
                    borrower,
                },
                swap_data,
            )
            .gas(5_000_000u64)
            .max_fee_per_gas(max_fee_per_gas)
            .max_priority_fee_per_gas(max_priority_fee_per_gas.unwrap_or(DEFAULT_MAX_FEE_PER_GAS))
            .send()
            .await
    } else {
        helper
            .executeLiquidationBlind(
                ctx.config.silo_address,
                ctx.config.hook_address,
                debt_token,
                LiquidationData {
                    hook: ctx.config.hook_address,
                    collateralAsset: collateral_token,
                    borrower,
                },
                swap_data,
            )
            .gas(5_000_000u64)
            .max_fee_per_gas(max_fee_per_gas)
            .max_priority_fee_per_gas(max_priority_fee_per_gas.unwrap_or(DEFAULT_MAX_FEE_PER_GAS))
            .send()
            .await
    };
    match liquidation_call {
        Ok(pending_tx) => {
            info!(
                ?borrower,
                "Liquidation (no wait) tx sent: {:?}",
                pending_tx.tx_hash()
            );
        }
        Err(e) => {
            error!(?borrower, "Failed to send liquidation (no wait) tx: {e}");
        }
    }
}
