// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.13;

import "@openzeppelin/contracts/utils/Address.sol";
import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import "@pythnetwork/pyth-sdk-solidity/IPyth.sol";
import "@pythnetwork/pyth-sdk-solidity/PythStructs.sol";
import {
    IERC3156FlashBorrower
} from "@silo-core/interfaces/IERC3156FlashBorrower.sol";
import {
    IPartialLiquidation
} from "@silo-core/interfaces/IPartialLiquidation.sol";
import {ISilo} from "@silo-core/interfaces/ISilo.sol";
import {IHookReceiver} from "@silo-core/interfaces/IHookReceiver.sol";
import {ILiquidationHelper} from "@silo-core/interfaces/ILiquidationHelper.sol";
import {
    IWrappedNativeToken
} from "@silo-core/interfaces/IWrappedNativeToken.sol";

contract LiquidationHelper is IERC3156FlashBorrower {
    using Address for address payable;
    using SafeERC20 for IERC20;

    // solhint-disable-next-line var-name-mixedcase
    bytes32 internal constant _FLASHLOAN_CALLBACK =
        keccak256("ERC3156FlashBorrower.onFlashLoan");

    /// @dev token receiver will get all rewards from liquidation, does not matter who will execute tx
    address payable public immutable TOKENS_RECEIVER;

    /// @dev address of wrapped native blockchain token eg. WETH on Ethereum
    address public immutable NATIVE_TOKEN;

    address public immutable EXCHANGE_PROXY;

    address public immutable SHADOW_V3_ROUTER =
        address(0x5543c6176FEb9B4b179078205d7C29EEa2e2d695);
    address public immutable SHADOW_V2_ROUTER =
        address(0x1D368773735ee1E678950B7A97bcA2CafB330CDc);

    address immutable X33 = address(0x3333111A391cC08fa51353E9195526A70b333333);
    address public immutable SHADOW =
        address(0x3333b97138D4b086720b5aE8A7844b1345a33333);
    address public immutable USDC =
        address(0x29219dd400f2Bf60E5a23d13Be72B486D4038894);

    struct route {
        address from;
        address to;
        bool stable;
    }

    IPyth PYTH_AGGREGATOR;

    error NoDebtToCover();
    error ZeroAddress();

    /// @param _nativeToken address of wrapped native blockchain token eg. WETH on Ethereum
    /// @param _exchangeProxy exchange address, where to send swap data on liquidation
    /// @param _tokensReceiver all leftover tokens (debt and collateral) will be send to this address after liquidation
    constructor(
        address _nativeToken,
        address _exchangeProxy,
        address _pyth_aggregator,
        address payable _tokensReceiver
    ) {
        require(_nativeToken != address(0), ZeroAddress());
        require(_exchangeProxy != address(0), ZeroAddress());
        require(address(_tokensReceiver) != address(0), ZeroAddress());

        NATIVE_TOKEN = _nativeToken;
        EXCHANGE_PROXY = _exchangeProxy;
        TOKENS_RECEIVER = _tokensReceiver;
        PYTH_AGGREGATOR = IPyth(_pyth_aggregator);
    }

    receive() external payable {}

    /// @notice Withdraws any ERC20 token (including profits) to the TOKENS_RECEIVER
    function withdraw(address token) external {
        IERC20(token).safeTransfer(
            TOKENS_RECEIVER,
            IERC20(token).balanceOf(address(this))
        );
    }

    /// @dev entry point for liquidation
    /// @notice for now we does not support liquidation with sTokens.
    /// On not profitable liquidation we will revert, because we will not be able to repay flashloan with fee
    /// (collateral will not be enough to cover loan + fee)
    function executeLiquidation(
        ISilo _flashLoanFrom,
        address _debtAsset,
        ILiquidationHelper.LiquidationData calldata _liquidation,
        bytes calldata _swapsInputs
    ) external {
        (, uint256 _maxDebtToCover, ) = _liquidation.hook.maxLiquidation(
            _liquidation.user
        );
        require(_maxDebtToCover != 0, NoDebtToCover());

        _flashLoanFrom.flashLoan(
            this,
            _debtAsset,
            _maxDebtToCover,
            abi.encode(_liquidation, _swapsInputs)
        );
        IERC20(_debtAsset).forceApprove(address(_flashLoanFrom), 0);
    }

    /// @dev entry point for liquidation without specifying max debt to cover
    function executeLiquidationBlind(
        ISilo _flashLoanFrom,
        IPartialLiquidation silo_hook,
        address _debtAsset,
        ILiquidationHelper.LiquidationData calldata _liquidation,
        bytes calldata _swapsInputs
    ) public {
        (, uint256 _maxDebtToCover, ) = silo_hook.maxLiquidation(
            _liquidation.user
        );
        require(_maxDebtToCover != 0, NoDebtToCover());

        _flashLoanFrom.flashLoan(
            this,
            _debtAsset,
            _maxDebtToCover,
            abi.encode(_liquidation, _swapsInputs)
        );
        IERC20(_debtAsset).forceApprove(address(_flashLoanFrom), 0);
    }

    function updatePythPriceFeedsAndExecuteLiquidation(
        ISilo _flashLoanFrom,
        address _debtAsset,
        ILiquidationHelper.LiquidationData calldata _liquidation,
        bytes calldata _swapsInputs,
        bytes[] calldata priceUpdate
    ) external {
        // uint fee = PYTH_AGGREGATOR.getUpdateFee(priceUpdate);
        // we can optimize: pyth update fee is always 1 wei (empirically observed)
        PYTH_AGGREGATOR.updatePriceFeeds{value: 1}(priceUpdate);

        executeLiquidationBlind(
            _flashLoanFrom,
            _liquidation.hook,
            _debtAsset,
            _liquidation,
            _swapsInputs
        );
    }

    /// @inheritdoc IERC3156FlashBorrower
    function onFlashLoan(
        address /* _initiator */,
        address _debtAsset,
        uint256 _maxDebtToCover,
        uint256 _fee,
        bytes calldata _data
    ) external returns (bytes32) {
        (
            ILiquidationHelper.LiquidationData memory _liquidation,
            bytes memory _swapInputs
        ) = abi.decode(_data, (ILiquidationHelper.LiquidationData, bytes));

        IERC20(_debtAsset).forceApprove(
            address(_liquidation.hook),
            _maxDebtToCover
        );

        _liquidation.hook.liquidationCall({
            _collateralAsset: _liquidation.collateralAsset,
            _debtAsset: _debtAsset,
            _user: _liquidation.user,
            _maxDebtToCover: _maxDebtToCover,
            _receiveSToken: false
        });

        IERC20(_debtAsset).forceApprove(address(_liquidation.hook), 0);
        uint256 flashLoanWithFee = _maxDebtToCover + _fee;

        if (_liquidation.collateralAsset == _debtAsset) {
            uint256 balance = IERC20(_liquidation.collateralAsset).balanceOf(
                address(this)
            );
            require(
                flashLoanWithFee <= balance,
                ILiquidationHelper.UnableToRepayFlashloan()
            );

            // bad debt is not supported, we will get underflow on bad debt
            _transferToReceiver(
                _liquidation.collateralAsset,
                balance - flashLoanWithFee
            );
        } else {
            // swap all collateral for debt
            // most likely there will be dust left in collateral tokens, this dust will be "recovered"
            // once we will liquidate "oposite" position
            if (_liquidation.collateralAsset == X33) {
                // X33 is a special case, we need to use a more complexing routing
                _executeX33Swap();
            } else {
                _executeSwap(_liquidation.collateralAsset, _swapInputs);
            }

            uint256 debtBalance = IERC20(_debtAsset).balanceOf(address(this));

            if (flashLoanWithFee < debtBalance) {
                unchecked {
                    // safe because of `if (flashLoanWithFee < debtBalance)`
                    _transferToReceiver(
                        _debtAsset,
                        debtBalance - flashLoanWithFee
                    );
                }
            } else if (flashLoanWithFee != debtBalance) {
                revert ILiquidationHelper.UnableToRepayFlashloan();
            }
        }

        IERC20(_debtAsset).forceApprove(msg.sender, flashLoanWithFee);
        return _FLASHLOAN_CALLBACK;
    }

    function _executeX33Swap() internal {
        /////////////////////////////////
        // 1) X33 → SHADOW on V3 router
        /////////////////////////////////
        uint256 _amountIn = IERC20(X33).balanceOf(address(this));
        IERC20(X33).forceApprove(address(SHADOW_V3_ROUTER), _amountIn);

        // encode the single‐swap params as a tuple:
        bytes memory payload1 = abi.encodeWithSignature(
            "exactInputSingle((address,address,int24,address,uint256,uint256,uint256,uint160))",
            X33,
            SHADOW,
            50,
            address(this),
            block.timestamp + 120,
            _amountIn, // amountIn
            0, // amountOutMinimum
            0 // sqrtPriceLimitX96
        );

        (bool ok1, ) = address(SHADOW_V3_ROUTER).call{value: 0}(payload1);
        require(ok1, "V3 swap 1 failed");
        IERC20(X33).forceApprove(address(SHADOW_V3_ROUTER), 0);

        //////////////////////////////////
        // 2) SHADOW → WS on V2 router
        //////////////////////////////////
        uint256 shadowBal = IERC20(SHADOW).balanceOf(address(this));
        IERC20(SHADOW).forceApprove(address(SHADOW_V2_ROUTER), shadowBal);

        route[] memory routes = new route[](1);
        routes[0] = route({from: SHADOW, to: NATIVE_TOKEN, stable: false});

        bytes memory payload2 = abi.encodeWithSignature(
            "swapExactTokensForTokens(uint256,uint256,(address,address,bool)[],address,uint256)",
            shadowBal,
            0,
            routes,
            address(this),
            block.timestamp + 120
        );

        (bool ok2, ) = address(SHADOW_V2_ROUTER).call{value: 0}(payload2);
        require(ok2, "V2 swap failed");

        IERC20(SHADOW).forceApprove(address(SHADOW_V2_ROUTER), 0);

        //////////////////////////////////
        // 3) WS → USDC on V3 router
        //////////////////////////////////
        uint256 wsBal = IERC20(NATIVE_TOKEN).balanceOf(address(this));
        IERC20(NATIVE_TOKEN).forceApprove(address(SHADOW_V3_ROUTER), wsBal);

        bytes memory payload3 = abi.encodeWithSignature(
            "exactInputSingle((address,address,int24,address,uint256,uint256,uint256,uint160))",
            NATIVE_TOKEN,
            USDC,
            50,
            address(this),
            block.timestamp + 120,
            wsBal,
            0,
            0
        );

        (bool ok3, ) = address(SHADOW_V3_ROUTER).call{value: 0}(payload3);
        require(ok3, "V3 swap 3 failed");
        IERC20(NATIVE_TOKEN).forceApprove(address(SHADOW_V3_ROUTER), 0);
    }

    function _executeSwap(
        address _sellToken,
        bytes memory _swapInputs
    ) internal {
        IERC20(_sellToken).approve(EXCHANGE_PROXY, type(uint256).max);

        (bool success, ) = EXCHANGE_PROXY.call{value: 0}(_swapInputs);
        require(success, "Swap failed");

        IERC20(_sellToken).approve(EXCHANGE_PROXY, 0);
    }

    function _transferToReceiver(address _asset, uint256 _amount) internal {
        if (_amount == 0) return;

        if (_asset == NATIVE_TOKEN) {
            _transferNative(_amount);
        } else {
            IERC20(_asset).safeTransfer(TOKENS_RECEIVER, _amount);
        }
    }

    /// @notice We assume that quoteToken is wrapped native token
    function _transferNative(uint256 _amount) internal {
        IWrappedNativeToken(address(NATIVE_TOKEN)).withdraw(_amount);
        TOKENS_RECEIVER.sendValue(_amount);
    }
}
