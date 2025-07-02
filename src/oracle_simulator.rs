use std::time::{SystemTime, UNIX_EPOCH};

use alloy::{
    eips::BlockId,
    network::TransactionBuilder,
    primitives::{aliases::I192, Address, Bytes, FixedBytes, Signed, U256},
    providers::Provider,
    rpc::types::{
        state::{StateOverride, StateOverridesBuilder},
        TransactionRequest,
    },
    sol_types::SolCall,
};

use crate::types::ISiloLens::isSolventCall;
use hex::FromHex;

async fn is_borrower_solvent_simulated<P>(
    provider: &P,
    silo: Address,
    borrower: Address,
    state_override: StateOverride,
) -> eyre::Result<bool>
where
    P: Provider + Clone + 'static,
{
    // ABI call encoding
    let call = isSolventCall { borrower };

    let result = provider
        .call(
            TransactionRequest::default()
                .with_to(silo)
                .with_input(Bytes::from(call.abi_encode())),
        )
        .overrides(state_override)
        .block(BlockId::Number(alloy::eips::BlockNumberOrTag::Latest))
        .await?;

    Ok(isSolventCall::abi_decode_returns(&result)
        .map_err(|_| eyre::eyre!("Failed to decode isSolvent return value"))?)
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

pub async fn chainlink_is_borrower_solvent_simulated<P>(
    provider: &P,
    silo: Address,
    borrower: Address,
    price: Signed<192, 3>,
) -> eyre::Result<bool>
where
    P: Provider + Clone + 'static,
{
    let encoded_hot_vars_with_first_last_trasmission = FixedBytes::<32>::from_slice(
        &<[u8; 32]>::from_hex("00000000000000000000000000000000000000000000000000010001392b0203")?,
    );

    let s_tramissions_slot =
        "0xd421a5181c571bba3f01190c922c3b2a896fc1d84e86c9f17ac10e67ebef8b5c".parse()?;

    let encoded_trasmission_padding = FixedBytes::<32>::from_slice(&encode_transmission(price));

    let s_usdc_chainlink: Address = "0x0BdbFF19543B20d0bc2d1eA08deE2be4C0b76743".parse()?;

    let storage_overrides = vec![
        (
            FixedBytes::<32>::from(U256::from(11)),
            encoded_hot_vars_with_first_last_trasmission,
        ),
        (s_tramissions_slot, encoded_trasmission_padding),
    ];

    dbg!(&storage_overrides);

    let state_overrides_builder = StateOverridesBuilder::default();
    let state_override = state_overrides_builder
        .with_state_diff(s_usdc_chainlink, storage_overrides)
        .build();

    is_borrower_solvent_simulated(provider, silo, borrower, state_override).await
}

pub async fn pyth_is_borrower_solvent_simulated<P>(
    provider: &P,
    silo: Address,
    borrower: Address,
    publish_time: u64,
    expo: i32,
    price: i64,
    conf: u64,
) -> eyre::Result<bool>
where
    P: Provider + Clone + 'static,
{
    // Compute slot 0 override
    let encoded_slot0 = encode_priceinfo_slot0(publish_time, expo, price, conf);
    let value0 = FixedBytes::<32>::from_slice(&encoded_slot0);

    // Slot = keccak(price_id, 213 + 12), hardcoded for ANON/USDC
    let anon_usdc_pyth_slot0: FixedBytes<32> =
        "0x586baa15fa17b0fbbc6939b28eb3223994dfa95a07202f334b47673e00596743".parse()?;

    // ANON/USDC Pyth Oracle address
    let anon_usdc_pyth: Address = "0x2880aB155794e7179c9eE2e38200202908C17B43".parse()?;

    let storage_overrides = vec![(anon_usdc_pyth_slot0, value0)];

    let state_overrides_builder = StateOverridesBuilder::default();
    let state_override = state_overrides_builder
        .with_state_diff(anon_usdc_pyth, storage_overrides)
        .build();

    is_borrower_solvent_simulated(provider, silo, borrower, state_override).await
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

/// Encodes the Python PriceInfo struct into Solidity's packed storage layout for slot 0.
///
/// Layout (packed, 28 bytes):
///   [24..32] ← uint64 publishTime
///   [20..24] ← int32 expo
///   [12..20] ← int64 price
///   [ 4..12] ← uint64 conf
/// [0..4] is zero-padded
fn encode_priceinfo_slot0(publish_time: u64, expo: i32, price: i64, conf: u64) -> [u8; 32] {
    let mut slot0 = [0u8; 32];

    slot0[4..12].copy_from_slice(&conf.to_be_bytes());
    slot0[12..20].copy_from_slice(&price.to_be_bytes());
    slot0[20..24].copy_from_slice(&expo.to_be_bytes());
    slot0[24..32].copy_from_slice(&publish_time.to_be_bytes());

    slot0
}
