use std::sync::Arc;

use alloy::{
    consensus::Transaction,
    primitives::{address, aliases::I192, Address, FixedBytes, Selector, Signed, U256},
    providers::Provider,
    rpc::types::state::StateOverridesBuilder,
    sol,
    sol_types::{SolCall, SolValue},
};

use futures::StreamExt;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use once_cell::sync::Lazy;

use hex::FromHex;

use crate::oracles::chainlink::{
    IChainlinkAggregator::{transmitCall, ReportDecode},
    IChainlinkForwarder::forwardCall,
};

sol! {

    #[sol(rpc)]
    interface IChainlinkForwarder {
        function forward(address to, bytes data) external;
    }

    #[sol(rpc)]
    interface IChainlinkAggregator {
        struct ReportDecode {
            uint32 epoch;
            bytes32 config_digest;
            int192[] observations;
            int192 juels_per_fee_coin;
        }

        function transmit(bytes32[3] reportContext, bytes report, bytes32[] rs, bytes32[] ss, bytes32 rawVs);
    }

}

#[derive(Debug, Clone, Copy)]
pub struct ChainLinkPriceData {
    pub price: I192,
    pub max_priority_fee_per_gas: Option<u128>,
    pub max_fee_per_gas: u128,
}

pub type SharedChainlinkPrice = Arc<RwLock<Option<ChainLinkPriceData>>>;

const MEV_1: Address = address!("0xbad0000000000000000000000000000000000001");
const MEV_2: Address = address!("0xbad0000000000000000000000000000000000002");
const MEV_3: Address = address!("0xbad0000000000000000000000000000000000003");

pub async fn chainlink_forward_listener_task<P>(
    provider: P,
    shared_price: SharedChainlinkPrice,
    chainlink_aggregator_address: Address,
) where
    P: Provider + Clone + 'static,
{
    let sub = match provider.subscribe_pending_transactions().await {
        Ok(sub) => sub,
        Err(e) => {
            error!("Failed to subscribe to pending transactions: {e}");
            return;
        }
    };

    let chainlink_forward_function_selector: Selector = FixedBytes::from_slice(&[
        111, 173, 207, 114, // forward function selector
    ]);

    let mev_function_selector: Selector = FixedBytes::from_slice(&[255, 77, 225, 2]);

    let mut stream = sub.into_stream();

    info!("Awaiting pending Chainlink Forward transactions on {chainlink_aggregator_address}...");

    while let Some(tx_hash) = stream.next().await {
        if let Ok(Some(_tx)) = provider.get_transaction_by_hash(tx_hash).await {
            let tx = _tx.into_inner();
            debug!("Pending transaction detected: {:?}", &tx.hash());

            if tx.to() == Some(MEV_1)
                || tx.to() == Some(MEV_2)
                || (tx.to() == Some(MEV_3)
                    && tx.function_selector() != Some(&mev_function_selector))
            {
                info!(
                    "Known MEV bot transaction, to {:?} : {:?}",
                    &tx.to(),
                    &tx.hash()
                );
            }

            // forwarder address
            if tx.function_selector() == Some(&chainlink_forward_function_selector) {
                match forwardCall::abi_decode(&tx.input()) {
                    Ok(call) => {
                        if call.to == chainlink_aggregator_address {
                            match transmitCall::abi_decode(&call.data) {
                                Ok(transmit_call) => {
                                    debug!(report = ?transmit_call.report, "Chainlink transmit call detected");
                                    match ReportDecode::abi_decode_params(&transmit_call.report) {
                                        Ok(report) => {
                                            let median_value =
                                                report.observations[report.observations.len() / 2];
                                            info!("Chainlink report {chainlink_aggregator_address} received: epoch = {}, median_value = {}", report.epoch, median_value);
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

// ✅ Obtaining and manipulating storage for testing Chainlink OCR2Aggregator behavior:
//
// 1. Get the full storage layout using Slither (requires contract source):
//    slither-read-storage sonic:0x0BdbFF19543B20d0bc2d1eA08deE2be4C0b76743 \
//      --json storage_layout.json \
//      --rpc-url http://localhost:8545 \
//      --etherscan-apikey <API_KEY> \
//      --value
//
// 2. Inspect slot 11 to extract the packed `s_hotVars` struct:
//    cast storage 0x0BdbFF19543B20d0bc2d1eA08deE2be4C0b76743 11
//
//    Example output:
//    0x0000000000000000000000000000000000000000000000003bea0001392b0203
//                               ^^^^
//                               This 4-byte field (offset 48 bits) is `latestAggregatorRoundId`.
//
//    To simulate round ID = 1:
//    Overwrite slot 11 with:
//    0x00000000000000000000000000000000000000000000000000010001392b0203
//
// 3. Override transmission data at index 1 in mapping `s_transmissions` (slot 12):
//
//    Compute storage slot for `s_transmissions[1]`:
//    cast index uint32 1 12
//    → 0xd421a5181c571bba3f01190c922c3b2a896fc1d84e86c9f17ac10e67ebef8b5c
//
//    Transmission struct layout:
//    struct Transmission {
//        int192 answer;
//        uint32 observationsTimestamp;
//        uint32 transmissionTimestamp;
//    }
//
//    Example encoded value:
//    0x67896a48678969ff000000000000000000000000000000000000000004b62060
//                                 ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
//                                 Replace this portion with the encoded `Transmission` struct
//                                 generated from your simulated report.
//
// 4. Final override plan:
//
//    - Slot 11:
//      0x00000000000000000000000000000000000000000000000000010001392b0203
//
//    - Slot keccak256(abi.encode(1, 12)) = s_transmissions[1]:
//      0xd421a5... → 0x<encoded Transmission struct>

static HOTVARS_SLOTS_11: Lazy<FixedBytes<32>> = Lazy::new(|| {
    FixedBytes::<32>::from_slice(
        &<[u8; 32]>::from_hex("00000000000000000000000000000000000000000000000000010001392b0203")
            .expect("valid hex"),
    )
});

static TRANSMISSION_SLOT: Lazy<FixedBytes<32>> = Lazy::new(|| {
    "0xd421a5181c571bba3f01190c922c3b2a896fc1d84e86c9f17ac10e67ebef8b5c"
        .parse()
        .expect("valid slot")
});

pub fn get_multiple_chainlink_state_overrides(
    chainlink_aggregator_address_to_price_map: HashMap<Address, Signed<192, 3>>,
) -> HashMap<
    Address,
    alloy::rpc::types::state::AccountOverride,
    alloy::primitives::map::FbBuildHasher<20>,
> {
    let mut builder = StateOverridesBuilder::default();
    for (chainlink_aggregator_address, price) in
        chainlink_aggregator_address_to_price_map.into_iter()
    {
        builder =
            builder.with_state_diff(chainlink_aggregator_address, get_storage_overrides(price))
    }

    builder.build()
}

pub fn get_chainlink_state_overrides(
    chainlink_aggregator_address: Address,
    price: Signed<192, 3>,
) -> HashMap<
    Address,
    alloy::rpc::types::state::AccountOverride,
    alloy::primitives::map::FbBuildHasher<20>,
> {
    StateOverridesBuilder::default()
        .with_state_diff(chainlink_aggregator_address, get_storage_overrides(price))
        .build()
}

fn get_storage_overrides(price: Signed<192, 3>) -> Vec<(FixedBytes<32>, FixedBytes<32>)> {
    let encoded_transmission = FixedBytes::<32>::from_slice(&encode_transmission(price));

    let storage_overrides = vec![
        (FixedBytes::<32>::from(U256::from(11)), *HOTVARS_SLOTS_11),
        (*TRANSMISSION_SLOT, encoded_transmission),
    ];
    storage_overrides
}

/// Encodes the ChainLink Trasmission struct into Solidity's packed storage layout for slot 0.
///
/// Layout (packed, 32 bytes):
///   [0..4] ← uint32 transmissionTimestamp
///   [4..8] ← uint32 observationsTimestamp
///   [8..32] ← int192 price
fn encode_transmission(price: I192) -> [u8; 32] {
    let mut slot = [0u8; 32];

    // Get current Unix timestamp (as u32)
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;

    slot[0..4].copy_from_slice(&now.to_be_bytes()); // transmissionTimestamp
    slot[4..8].copy_from_slice(&now.to_be_bytes()); // observationsTimestamp
    slot[8..32].copy_from_slice(&price.to_be_bytes::<24>());

    slot
}
