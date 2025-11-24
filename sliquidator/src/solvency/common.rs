use alloy::{
    network::TransactionBuilder,
    primitives::{Address, Bytes},
    providers::{PendingTransactionBuilder, PendingTransactionConfig, Provider},
    rpc::types::TransactionRequest,
    sol_types::SolCall,
};
use tokio::task::JoinSet;
use tracing::{error, info};

use crate::{
    config::LIQUIDATOR_ADDRESS,
    types::{EoaFanout, LiquidationContext, LiquidationData, LiquidationType},
};

pub(crate) const MIN_AMOUNT_FOR_LIQUIDATION_BASE_STRATEGY_INDEX: usize = 0;
pub(crate) const MIN_AMOUNT_FOR_LIQUIDATION_SIMULATED_STRATEGY_INDEX: usize = 1;

pub(crate) async fn liquidate_no_wait<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: LiquidationContext<PWatch, PSend>,
    helper: EoaFanout<PSend>,
    borrower: Address,
    debt_token: Address,
    collateral_token: Address,
    swap_data: Bytes,
    liquidation_type: LiquidationType,
    listen_to_receipt: bool,
) {
    let liquidation_call = match liquidation_type {
        LiquidationType::Standard => crate::types::ILiquidationHelper::executeLiquidationCall {
            debtAsset: debt_token,
            liquidation: LiquidationData {
                hook: ctx.config.hook_address,
                collateralAsset: collateral_token,
                borrower,
            },
            swapData: swap_data.clone(),
            silo: ctx.config.silo_address,
        }
        .abi_encode(),
        LiquidationType::Blind => crate::types::ILiquidationHelper::executeLiquidationBlindCall {
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
        .abi_encode(),
        LiquidationType::Pyth(price_data) => {
            crate::types::ILiquidationHelper::updatePythPriceFeedsAndExecuteLiquidationCall {
                silo: ctx.config.silo_address,
                debtAsset: debt_token,
                liquidation: LiquidationData {
                    hook: ctx.config.hook_address,
                    collateralAsset: collateral_token,
                    borrower,
                },
                swapData: swap_data.clone(),
                priceUpdate: price_data,
            }
            .abi_encode()
        }
    };

    let nonce = match ctx.provider.get_transaction_count(helper.address()).await {
        Ok(nonce) => nonce,
        Err(e) => {
            error!(?borrower, "Failed to fetch nonce: {e}");
            return;
        }
    };

    let gas = match ctx.provider.get_gas_price().await {
        Ok(gas) => gas,
        Err(e) => {
            error!(?borrower, "Failed to estimate gas: {e}");
            return;
        }
    };

    let tx = TransactionRequest::default()
        .with_from(helper.address())
        .with_to(LIQUIDATOR_ADDRESS)
        .with_nonce(nonce)
        .with_input(liquidation_call)
        .with_gas_limit(5_000_000u64)
        .with_gas_price(gas);

    let tx_envelope = tx.build(&helper.signer).await.unwrap();

    let mut set = JoinSet::new();

    for p in &helper.endpoints {
        let p = p.clone();
        let tx = tx_envelope.clone();
        set.spawn(async move {
            // Result<PendingTransaction, E>
            p.send_tx_envelope(tx).await
        });
    }

    while let Some(liquidation_call) = set.join_next().await {
        match liquidation_call {
            Ok(Ok(pending_tx)) => {
                let hash = *pending_tx.tx_hash();
                drop(pending_tx);

                let pending_tx_watch = PendingTransactionBuilder::from_config(
                    ctx.provider.root().clone(),
                    PendingTransactionConfig::new(hash),
                );

                info!(?borrower, "Liquidation (no wait) tx sent: {:?}", hash);
                if listen_to_receipt {
                    let _ = ctx
                        .liquidation_tx_sender
                        .send((borrower, pending_tx_watch))
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
