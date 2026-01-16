use alloy::{
    eips::BlockId,
    primitives::{Address, FixedBytes, Signed, U256},
    providers::Provider,
    rpc::types::state::{StateOverride, StateOverridesBuilder},
};
use common::oracles::chainlink::get_chainlink_state_overrides;
use once_cell::sync::Lazy;

use crate::types::LiquidationContext;

async fn is_borrower_solvent_simulated<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
    borrower: Address,
    state_override: StateOverride,
) -> eyre::Result<(bool, U256, U256)> {
    let multicall = ctx
        .provider
        .multicall()
        .add(ctx.hook.maxLiquidation(borrower))
        .add(ctx.silo.isSolvent(borrower))
        .overrides(state_override)
        .block(BlockId::Number(alloy::eips::BlockNumberOrTag::Latest));

    let (liquidation, is_solvent) = multicall.aggregate().await?;

    Ok((
        is_solvent,
        liquidation.maxDebt,
        liquidation.collateralToLiquidate,
    ))
}

static S_USDC_CHAINLINK_ADDRESS: Lazy<Address> = Lazy::new(|| {
    "0x0BdbFF19543B20d0bc2d1eA08deE2be4C0b76743"
        .parse()
        .expect("valid address")
});

pub async fn chainlink_is_borrower_solvent_simulated<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
    borrower: Address,
    price: Signed<192, 3>,
) -> eyre::Result<(bool, U256, U256)> {
    let state_override = get_chainlink_state_overrides(*S_USDC_CHAINLINK_ADDRESS, price);

    is_borrower_solvent_simulated(ctx, borrower, state_override).await
}

// Static Pyth Oracle address (ANON/USDC)
static PYTH_ORACLE_ADDRESS: Lazy<Address> = Lazy::new(|| {
    "0x2880aB155794e7179c9eE2e38200202908C17B43"
        .parse()
        .expect("valid address")
});

pub async fn pyth_is_borrower_solvent_simulated<
    PWatch: Provider + Clone + 'static,
    PSend: Provider + Clone + 'static,
>(
    ctx: &LiquidationContext<PWatch, PSend>,
    borrower: Address,
    publish_time: i64,
    expo: i32,
    price: i64,
    conf: u64,
    slot_0: &FixedBytes<32>,
) -> eyre::Result<(bool, U256, U256)> {
    let encoded = encode_priceinfo_slot0(publish_time, expo, price, conf);
    let value = FixedBytes::<32>::from_slice(&encoded);

    let storage_overrides = vec![(*slot_0, value)];

    let state_override = StateOverridesBuilder::default()
        .with_state_diff(*PYTH_ORACLE_ADDRESS, storage_overrides)
        .build();

    is_borrower_solvent_simulated(ctx, borrower, state_override).await
}

/// Encodes the Python PriceInfo struct into Solidity's packed storage layout for slot 0.
///
/// Layout (packed, 28 bytes):
///   [24..32] ← uint64 publishTime
///   [20..24] ← int32 expo
///   [12..20] ← int64 price
///   [ 4..12] ← uint64 conf
/// [0..4] is zero-padded
fn encode_priceinfo_slot0(publish_time: i64, expo: i32, price: i64, conf: u64) -> [u8; 32] {
    let mut slot0 = [0u8; 32];

    slot0[4..12].copy_from_slice(&conf.to_be_bytes());
    slot0[12..20].copy_from_slice(&price.to_be_bytes());
    slot0[20..24].copy_from_slice(&expo.to_be_bytes());
    slot0[24..32].copy_from_slice(&publish_time.to_be_bytes());

    slot0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priceinfo_slot_matches_solidity_packing() {
        let slot = encode_priceinfo_slot0(1_700_000_000, -8, 150_000_000, 42);

        assert_eq!(&slot[0..4], &[0, 0, 0, 0]);
        assert_eq!(&slot[4..12], &42u64.to_be_bytes());
        assert_eq!(&slot[12..20], &150_000_000i64.to_be_bytes());
        assert_eq!(&slot[20..24], &(-8i32).to_be_bytes());
        assert_eq!(&slot[24..32], &1_700_000_000i64.to_be_bytes());
    }
}
