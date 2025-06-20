// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.20;

import "forge-std/Script.sol";
import "../src/Liquidator.sol";

contract Deploy is Script {
    function run() external {
        address odosProxy = address(0xaC041Df48dF9791B0654f1Dbbf2CC8450C5f2e9D);
        address nativeToken = address(
            0x039e2fB66102314Ce7b64Ce5Ce3E5183bc94aD38
        ); // WS
        address payable liquidator_receiver = payable(
            0xfeed000000000000000000000000000000000001
        ); // Replace with your address

        vm.startBroadcast();

        LiquidationHelper liquidator = new LiquidationHelper(
            nativeToken,
            odosProxy,
            liquidator_receiver
        );

        vm.stopBroadcast();

        console.log("Liquidator deployed at:", address(liquidator));
    }
}
