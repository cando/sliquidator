use std::{collections::HashSet, sync::Arc};

use alloy::{primitives::Address, sol};
use tokio::sync::RwLock;

pub type SharedAddressList = Arc<RwLock<HashSet<Address>>>;

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
    }

    #[sol(rpc)]
    interface ISiloConfig {
        function getConfigsForSolvency(address borrower) view returns (
            ConfigData collateralConfig,
            ConfigData debtConfig
        );
    }

    struct Route {
        address from;
        address to;
        bool stable;
    }

    #[sol(rpc)]
    interface IRouter {
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            Route[] routes,
            address to,
            uint256 deadline
        ) external returns (uint256[] amounts);

        function getAmountOut(
            uint256 amountIn,
            address tokenIn,
            address tokenOut
        ) external view returns (uint256 amount, bool stable);
    }

}
