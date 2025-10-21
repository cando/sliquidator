use alloy::{
    network::TransactionBuilder,
    primitives::{Address, Bytes, U256},
    providers::Provider,
    rpc::types::TransactionRequest,
    sol_types::SolCall,
};
use tracing::{error, info};

use crate::{
    config::LIQUIDATOR_ADDRESS,
    types::{EoaFanout, LiquidationContext, LiquidationData},
};

pub(crate) const MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX: usize = 0;
pub(crate) const MIN_AMOUNT_FOR_LIQUIDATION_SIMULATED_STRATEGY_INDEX: usize = 1;

pub(crate) const DEFAULT_MAX_FEE_PER_GAS: u128 = 65_000_000_000;

pub(crate) async fn liquidate_no_wait<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: LiquidationContext<PWatch, PSend>,
    helper: EoaFanout<PSend>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    max_debt: Option<U256>,
    swap_data: Bytes,
    max_priority_fee_per_gas: Option<u128>,
    max_fee_per_gas: u128,
    listen_to_receipt: bool,
) {
    let liquidation_call = if let Some(max_debt_to_cover) = max_debt {
        crate::types::ILiquidationHelper::executeLiquidationCall {
            debtAsset: debt_token,
            maxDebtToCover: max_debt_to_cover,
            liquidation: LiquidationData {
                hook: ctx.config.hook_address,
                collateralAsset: collateral_token,
                borrower,
            },
            swapData: swap_data.clone(),
            silo: ctx.config.silo_address,
        }
        .abi_encode()
    } else {
        crate::types::ILiquidationHelper::executeLiquidationBlindCall {
            debtAsset: debt_token,
            liquidation: LiquidationData {
                hook: ctx.config.hook_address,
                collateralAsset: collateral_token,
                borrower,
            },
            swapData: swap_data.clone(),
            silo: ctx.config.silo_address,
            hook: ctx.config.hook_address,
        }
        .abi_encode()
    };

    let tx = TransactionRequest::default()
        .with_to(LIQUIDATOR_ADDRESS)
        .with_input(liquidation_call)
        .with_gas_limit(5_000_000u64)
        .with_max_fee_per_gas(max_fee_per_gas)
        .with_max_priority_fee_per_gas(max_priority_fee_per_gas.unwrap_or(DEFAULT_MAX_FEE_PER_GAS));
    let tx_envelope = tx.build(&helper.signer).await.unwrap();

    let mut tasks = Vec::new();

    for p in &helper.endpoints {
        let p = p.clone();
        let tx_envelope = tx_envelope.clone();
        tasks.push(tokio::spawn(async move {
            p.send_tx_envelope(tx_envelope).await
        }));
    }

    let results = futures::future::join_all(tasks).await;

    for liquidation_call in results {
        match liquidation_call {
            Ok(Ok(pending_tx)) => {
                info!(
                    ?borrower,
                    "Liquidation (no wait) tx sent: {:?}",
                    pending_tx.tx_hash()
                );
                if listen_to_receipt {
                    let _ = ctx
                        .liquidation_tx_sender
                        .send((borrower, pending_tx))
                        .inspect_err(|e| {
                            error!(?borrower, "Failed to send liquidation (no wait) tx: {e}");
                        });
                }
            }
            Ok(Err(e)) => {
                error!(?borrower, "Failed to send liquidation (no wait) tx: {e}");
            }
            Err(e) => {
                error!(
                    ?borrower,
                    "Task join error for liquidation (no wait) tx: {e}"
                );
            }
        }
    }
}
