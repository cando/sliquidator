use alloy::sol;

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
}
