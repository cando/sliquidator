use alloy::{
    eips::BlockId,
    network::TransactionBuilder,
    primitives::{Address, Bytes, FixedBytes},
    providers::Provider,
    rpc::types::{state::StateOverridesBuilder, TransactionRequest},
    sol_types::SolCall,
};

use crate::types::ISiloLens::isSolventCall;

pub(crate) async fn is_borrower_solvent_simulated<P>(
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
    // ABI call encoding
    let call = isSolventCall { borrower };

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

/// Encodes the PriceInfo struct into Solidity's packed storage layout for slot 0.
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
