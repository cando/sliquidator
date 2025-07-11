use std::time::{SystemTime, UNIX_EPOCH};

use alloy::{
    primitives::{aliases::I24, Address, Bytes, U160, U256},
    providers::Provider,
    sol_types::SolCall,
};
use eyre::eyre;
use tracing::{error, info};

use crate::types::{
    ILiquidationHelper::ILiquidationHelperInstance, LiquidationContext, LiquidationData,
};

pub(crate) const MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX: usize = 0;
pub(crate) const MIN_AMOUNT_FOR_LIQUIDATION_SIMULATED_STRATEGY_INDEX: usize = 1;

pub(crate) async fn get_swap_data<P>(
    helper: &ILiquidationHelperInstance<P>,
    collateral_token: Address,
    debt_token: Address,
    collateral_to_liquidate: U256,
) -> Bytes
where
    P: Provider + Clone + 'static,
{
    if collateral_token != debt_token {
        let amount_for_swap = collateral_to_liquidate * U256::from(995) / U256::from(1000);

        let call = crate::types::IV3SwapRouter::exactInputSingleCall {
            params: crate::types::IV3SwapRouter::ExactInputSingleParams {
                tokenIn: collateral_token,
                tokenOut: debt_token,
                tickSpacing: I24::unchecked_from(50),
                recipient: *helper.address(),
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

pub(crate) async fn liquidate_and_wait_for_receipt<P>(
    ctx: LiquidationContext<P>,
    helper: ILiquidationHelperInstance<P>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    swap_data: Bytes,
    max_debt: U256,
) -> bool
where
    P: Provider + Clone + 'static,
{
    info!(collateral_token = ?collateral_token, debt_token = ?debt_token, ?borrower, "🚀 Sending liquidation tx...");
    let tx: alloy::contract::CallBuilder<
        &P,
        std::marker::PhantomData<crate::types::ILiquidationHelper::executeLiquidationCall>,
    > = helper
        .executeLiquidation(
            ctx.config.silo_address,
            debt_token,
            max_debt,
            LiquidationData {
                hook: ctx.config.hook_address,
                collateralAsset: collateral_token,
                borrower,
            },
            swap_data,
        )
        .gas(5_000_000u64)
        .max_fee_per_gas(65_000_000_000u128)
        .max_priority_fee_per_gas(65_000_000_000u128);

    match async {
        let pending_tx = tx.send().await?;
        let receipt = pending_tx.get_receipt().await?;
        if !receipt.status() {
            return Err(eyre!("Transaction reverted"));
        }
        Ok(receipt)
    }
    .await
    {
        Ok(receipt) => {
            info!(?receipt.block_number, %borrower, "✅ Tx succeeded");
            ctx.watchlist
                .write()
                .await
                .retain(|(w, _, _)| *w != borrower);
            return true;
        }
        Err(e) => {
            error!(?borrower, "❌ Liquidation tx failed: {e}");
            ctx.blacklist.write().await.insert(borrower);

            return false;
        }
    }
}

pub(crate) async fn liquidate_no_wait<P>(
    ctx: LiquidationContext<P>,
    helper: ILiquidationHelperInstance<P>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    max_debt: Option<U256>,
    swap_data: Bytes,
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
            .max_fee_per_gas(65_000_000_000u128)
            .max_priority_fee_per_gas(65_000_000_000u128)
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
            .max_fee_per_gas(65_000_000_000u128)
            .max_priority_fee_per_gas(65_000_000_000u128)
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
