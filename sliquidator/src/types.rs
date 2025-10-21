use std::{collections::HashSet, sync::Arc};

use alloy::{
    network::{Ethereum, EthereumWallet},
    primitives::{Address, FixedBytes, U256},
    providers::{PendingTransactionBuilder, Provider},
    sol,
};
use common::oracles::chainlink::SharedChainlinkPrice;
use tokio::sync::{mpsc, RwLock};

use crate::{
    config::SiloConfig,
    oracles::hermes::SharedPythPrice,
    types::{
        IHookReceiver::IHookReceiverInstance, ISilo::ISiloInstance,
        ISiloConfig::ISiloConfigInstance, ISiloLens::ISiloLensInstance,
    },
};

pub type SharedAddressList = Arc<RwLock<HashSet<Address>>>;

#[derive(Clone)]
pub enum OraclePriceData {
    Chainlink {
        forwarded_price: SharedChainlinkPrice,
    },
    Pyth {
        price_id: String,
        price_feed: SharedPythPrice,
        slot_0: FixedBytes<32>,
    },
    Redstone,
}

/// A signer + a set of broadcast endpoints (no wallets attached)
#[derive(Clone)]
pub struct EoaFanout<P: Provider + Clone + 'static> {
    pub signer: EthereumWallet,
    pub endpoints: Vec<P>,
}

#[derive(Clone)]
pub struct LiquidationContext<PWatch: Provider + Clone + 'static, PSend: Provider + Clone + 'static>
{
    pub provider: PWatch,
    pub lens: ISiloLensInstance<PWatch>,
    pub helpers: Vec<EoaFanout<PSend>>,
    pub silo_config: ISiloConfigInstance<PWatch>,
    pub config: SiloConfig,
    pub hook: IHookReceiverInstance<PWatch>,
    pub silo: ISiloInstance<PWatch>,
    pub oracle_price_data: OraclePriceData,
    pub watchlist: Arc<RwLock<Vec<(Address, Address, Address)>>>,
    pub blacklist: Arc<RwLock<HashSet<Address>>>,
    pub borrowers: SharedAddressList,
    pub liquidation_tx_sender:
        mpsc::UnboundedSender<(Address, PendingTransactionBuilder<Ethereum>)>,
    pub fast_check_threshold: U256,
    pub fast_check_threshold_same_token: U256,
    pub fast_check_threshold_stable_market: U256,
    pub liquidation_threshold: U256,
}

sol! {
    #[derive(Debug)]
    struct Borrower {
        address silo;
        address wallet;
    }

    #[derive(Debug)]
    struct BorrowerHealth {
        uint256 lt;
        uint256 ltv;
    }

    #[derive(Debug)]
    struct ConfigData {
        uint256 daoFee;
        uint256 deployerFee;
        address silo;
        address token;
        address protectedShareToken;
        address collateralShareToken;
        address debtShareToken;
        address solvencyOracle;
        address maxLtvOracle;
        address interestRateModel;
        uint256 maxLtv;
        uint256 lt;
        uint256 liquidationTargetLtv;
        uint256 liquidationFee;
        uint256 flashloanFee;
        address hookReceiver;
        bool callBeforeQuote;
    }

    #[derive(Debug)]
    struct LiquidationData {
        address hook;
        address collateralAsset;
        address borrower;
    }

    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address owner) view returns (uint256);
        event Transfer(address indexed from, address indexed to, uint256 value);
    }

    #[sol(rpc)]
    interface ISiloLens {
        function getUsersHealth(Borrower[] borrowers) view returns (BorrowerHealth[]);
        function isSolvent(address borrower) view returns (bool);
        function getLtv(address silo, address borrower) view returns (uint256);
        function getLt(address silo) view returns (uint256);


    }

    #[sol(rpc)]
    interface ISilo {
        function isSolvent(address borrower) view returns (bool);
    }

    #[sol(rpc)]
    interface IHookReceiver {
        function maxLiquidation(address borrower) view returns (
            uint256 collateralToLiquidate,
            uint256 maxDebt,
            bool sTokenRequired
        );
    }

    #[sol(rpc)]
    interface ILiquidationHelper {
        function withdraw(address token);

        function executeLiquidation(
            address silo,
            address debtAsset,
            uint256 maxDebtToCover,
            LiquidationData liquidation,
            bytes swapData
        ) returns (
            uint256 withdrawnCollateral,
            uint256 repaidDebt
        );

        function executeLiquidationBlind(
            address silo,
            address hook,
            address debtAsset,
            LiquidationData liquidation,
            bytes swapData
        ) returns (
            uint256 withdrawnCollateral,
            uint256 repaidDebt
        );

        function updatePythPriceFeedsAndExecuteLiquidation(
            address silo,
            address debtAsset,
            LiquidationData liquidation,
            bytes swapData,
            bytes[] priceUpdate,
        );
    }

    #[sol(rpc)]
    interface ISiloConfig {
        function getConfigsForSolvency(address borrower) view returns (
            ConfigData collateralConfig,
            ConfigData debtConfig
        );
    }

    #[sol(rpc)]
    interface IV3SwapRouter {
        struct ExactInputSingleParams {
            address tokenIn;
            address tokenOut;
            int24 tickSpacing;
            address recipient;
            uint256 deadline;
            uint256 amountIn;
            uint256 amountOutMinimum;
            uint160 sqrtPriceLimitX96;
        }

        function exactInputSingle(ExactInputSingleParams params) payable external returns (uint256 amountOut);
    }

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

    #[sol(rpc)]
    interface IV3Oracle {
        function getAggregatorPrice(bool _primary) view returns (bool success, uint256 price);
    }

}
