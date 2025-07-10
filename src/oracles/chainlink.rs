use std::sync::Arc;

use alloy::{
    consensus::Transaction,
    primitives::{address, aliases::I192, Address},
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

const CHAINLINK_FORWARDER_ADDRESS: Address = address!("0xF2Ab88678d19E8c7D3d621177a901F7d476ad92b");
const CHAINLINK_AGGREGATOR_ADDRESS: Address =
    address!("0x0BdbFF19543B20d0bc2d1eA08deE2be4C0b76743");

pub type SharedChainlinkPrice = Arc<RwLock<Option<I192>>>;

pub async fn chainlink_forward_listener_task<P>(
    ctx: LiquidationContext<P>,
    shared_price: SharedChainlinkPrice,
) where
    P: Provider + Clone + 'static,
{
    let sub = match ctx.provider.subscribe_full_pending_transactions().await {
        Ok(sub) => sub,
        Err(e) => {
            error!("Failed to subscribe to pending transactions: {e}");
            return;
        }
    };

    let mut stream = sub.into_stream();

    info!("Awaiting pending S/USDC Chainlink Forward transactions...");

    while let Some(_tx) = stream.next().await {
        let tx = _tx.into_inner();
        info!("Pending transaction detected: {:?}", &tx.hash());
        // forwarder address
        if tx.to() == Some(CHAINLINK_FORWARDER_ADDRESS) {
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
                                        *w = Some(median_value.clone());
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
