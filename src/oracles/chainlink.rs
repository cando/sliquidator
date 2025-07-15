use std::sync::Arc;

use alloy::{
    consensus::Transaction,
    primitives::{address, aliases::I192, Address, FixedBytes, Selector},
    providers::Provider,
    sol_types::{SolCall, SolValue},
};

use futures::StreamExt;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::types::{
    IChainlinkAggregator::{transmitCall, ReportDecode},
    IChainlinkForwarder::forwardCall,
    LiquidationContext,
};

const CHAINLINK_AGGREGATOR_ADDRESS: Address =
    address!("0x0BdbFF19543B20d0bc2d1eA08deE2be4C0b76743");

#[derive(Debug, Clone, Copy)]
pub(crate) struct ChainLinkPriceData {
    pub price: I192,
    pub max_priority_fee_per_gas: Option<u128>,
    pub max_fee_per_gas: u128,
}

pub type SharedChainlinkPrice = Arc<RwLock<Option<ChainLinkPriceData>>>;

pub async fn chainlink_forward_listener_task<P>(
    ctx: LiquidationContext<P>,
    shared_price: SharedChainlinkPrice,
) where
    P: Provider + Clone + 'static,
{
    let sub = match ctx.provider.subscribe_pending_transactions().await {
        Ok(sub) => sub,
        Err(e) => {
            error!("Failed to subscribe to pending transactions: {e}");
            return;
        }
    };

    let chainlink_forward_function_selector: Selector = FixedBytes::from_slice(&[
        111, 173, 207, 114, // forward function selector
    ]);

    let mut stream = sub.into_stream();

    info!("Awaiting pending S/USDC Chainlink Forward transactions...");

    while let Some(tx_hash) = stream.next().await {
        if let Ok(Some(_tx)) = ctx.provider.get_transaction_by_hash(tx_hash).await {
            let tx = _tx.into_inner();
            debug!("Pending transaction detected: {:?}", &tx.hash());
            // forwarder address
            if tx.function_selector() == Some(&chainlink_forward_function_selector) {
                match forwardCall::abi_decode(&tx.input()) {
                    Ok(call) => {
                        if call.to == CHAINLINK_AGGREGATOR_ADDRESS {
                            match transmitCall::abi_decode(&call.data) {
                                Ok(transmit_call) => {
                                    debug!(report = ?transmit_call.report, "Chainlink transmit call detected");
                                    match ReportDecode::abi_decode_params(&transmit_call.report) {
                                        Ok(report) => {
                                            let median_value =
                                                report.observations[report.observations.len() / 2];
                                            info!("Chainlink report S/USDC received: epoch = {}, median_value = {}", report.epoch, median_value);
                                            let mut w = shared_price.write().await;
                                            *w = Some(ChainLinkPriceData {
                                                price: median_value.clone(),
                                                max_priority_fee_per_gas: tx
                                                    .max_priority_fee_per_gas(),
                                                max_fee_per_gas: tx.max_fee_per_gas(),
                                            });
                                        }
                                        Err(e) => {
                                            warn!("Failed to decode Chainlink report: {e}");
                                            continue;
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to decode transmit call: {e}");
                                    continue;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to decode transaction input: {e}");
                        continue;
                    }
                }
            }
        }
    }
}
