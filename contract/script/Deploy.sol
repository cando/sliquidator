// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.20;

import "forge-std/Script.sol";
import "../src/Liquidator.sol";

contract Deploy is Script {
    function run() external {
        address shadowRouter = address(
            0x1D368773735ee1E678950B7A97bcA2CafB330CDc
        );
        address nativeToken = address(
            0x039e2fB66102314Ce7b64Ce5Ce3E5183bc94aD38
        ); // WS
        address payable liquidator_receiver = payable(
            0xfeed000000000000000000000000000000000001
        ); // Replace with your address

        vm.startBroadcast();

        LiquidationHelper liquidator = new LiquidationHelper(
            nativeToken,
            shadowRouter,
            liquidator_receiver
        );

        vm.stopBroadcast();

        console.log("Liquidator deployed at:", address(liquidator));
    }
}
