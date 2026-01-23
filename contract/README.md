# LiquidationHelper

Solidity half of `sliquidator`. One stateless helper per chain, deployed once and called by
the bots.

## What it does

A liquidation on Silo is a flashloan plus a `liquidationCall`: the helper borrows the
debt asset, repays the borrower's debt, receives the collateral, swaps it back into the
debt asset, repays the flashloan, and forwards the remainder to a fixed receiver.

```
executeLiquidation
  └─ hook.maxLiquidation(user)            → require maxDebt != 0
  └─ silo.flashLoan(helper, debt, maxDebt, params)
       └─ onFlashLoan
            ├─ hook.liquidationCall(user, collateral, debt, maxDebt)
            ├─ swap collateral → debt (if the assets differ)
            ├─ require(flashloan + fee <= debt balance)
            └─ approve repayment
  └─ reset approval
```

Entry points:

- `executeLiquidation(silo, debtAsset, liquidation, swapData)` reads the maximum debt from
  the hook and starts a flashloan.
- `executeLiquidationBlind(silo, hook, debtAsset, liquidation, swapData)` does the same but
  takes the hook explicitly, for calls where the hook address is not already known.
- `updatePythPriceFeedsAndExecuteLiquidation(...)` pushes a Pyth price update (`value: 1`,
  the observed update fee) and then runs the blind path.
- `withdraw(token)` forwards any balance to `TOKENS_RECEIVER`.

## Swaps

The generic route is a single `exactInputSingle` call on the configured exchange proxy.
`X33` has a dedicated three hop route in `_executeX33Swap`: X33 → SHADOW on the v3 router,
SHADOW → wrapped native on the v2 router, wrapped native → USDC on the v3 router. The
routes are hard-coded because they are the only liquid path for that collateral on the
deployment this was written for.

## Fail-closed behaviour

- `NoDebtToCover` when the hook reports zero debt.
- `UnableToRepayFlashloan` when the swap output cannot cover the flashloan plus fee.
- `ZeroAddress` for zero constructor arguments.
- All token approvals are reset after use (`forceApprove` to zero), so the helper does
  not leave allowances behind.
- The receiver is immutable and set at construction; the helper never sends funds to the
  caller.

An unprofitable liquidation therefore reverts atomically and costs only gas. The helper
does not try to keep bad debt or socialise losses.

## Build

```shell
git submodule update --init --recursive
forge build
```

Dependencies are pinned as git submodules: OpenZeppelin, `silo-contracts-v2`, and the Pyth
Solidity SDK. `Deploy.sol` imports `forge-std`; install it with
`forge install foundry-rs/forge-std` if it is not present.

The addresses in `script/Deploy.sol` are placeholders. Replace the wrapped-native token,
exchange proxy, Pyth aggregator, and receiver with real addresses for the target chain
before broadcasting.

## Tests

There are no Foundry tests in this repository. The helper was exercised against the live
Silo deployment and through the Rust bot's integration runs, not through a local test
suite.
