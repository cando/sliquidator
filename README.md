# sliquidator

> A small, fast liquidation bot for Silo markets, written in Rust with a purpose-built Solidity helper.

[![Rust](https://img.shields.io/badge/rust-2021-orange?logo=rust)](https://www.rust-lang.org/)
[![Solidity](https://img.shields.io/badge/solidity-0.8.x-363636?logo=solidity)](https://soliditylang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

This bot watches Silo lending markets, detects positions that are insolvent or about to
become insolvent, and liquidates them through a flashloan. It reads chain state from a
local node over IPC, simulates the next block's oracle state before committing, signs
transactions locally, and broadcasts the same signed transaction to several RPC endpoints
at once. The liquidator contract does the flashloan, the Silo `liquidationCall`, and the
collateral swap in one atomic transaction, so a bad quote costs gas and nothing else.

There is no dashboard, no database, and no hard-coded strategy alpha here: it is a
latency-sensitive event loop plus a careful set of checks.

```
                 ┌──────────────────────────────────────────────┐
   pending txs   │                sliquidator                   │
   ────────────► │  borrower discovery → watchlist → checks      │
                 │      │                    │                  │
   logs (IPC)    │      │              oracle simulation       │
   ────────────► │      ▼                    ▼                  │
                 │   simulate state ──► sign one transaction    │
                 └──────┬───────────────────────┬───────────────┘
                        │                       │
                 broadcast RPC 1..N        receive receipt
                        │                       │
                        ▼                       ▼
              ┌────────────────────┐    success → log, move on
              │ LiquidationHelper  │    revert  → blacklist
              │ flashloan → liquid.│
              │ → swap → repay     │
              └────────────────────┘
```

## How it got here

I wrote the first version of this as a small watcher: read borrower health from the Silo
lens, print whatever was close to insolvency, liquidate the obvious cases. It worked, but
not for long. Other bots showed up, and the only way to stay useful was to make the loop
faster and the checks smarter: fewer round trips first, then simulations against the
oracle state I expected in the next block, then a same-token projection that fires
before a position is formally insolvent.

The other half of the story is gas. For a while I tried to outbid everyone with
hand-rolled fee maths; the version that survived is simpler and faster: sign once,
broadcast the same transaction to several endpoints, and let the node suggest the fee. I
also stopped renting a remote host. The bot ran from my desktop for a while, then from a
small mini PC sitting next to the blockchain node, with local IPC for reads and broadcast
over the network. That last step cut more latency than any code change, and getting the
last few milliseconds out forced me to actually understand the low-level parts of Rust
and `alloy`. It was a lot of fun, and the most useful Rust exercise I have done.

## What it does

- Reads Silo market state and borrower health over a local IPC node.
- Discovers new borrowers from debt-token `Transfer` logs and keeps the list sorted by
  health.
- Verifies insolvency on-chain instead of trusting a stale number.
- Simulates the next oracle price with EVM state overrides for Chainlink and Pyth feeds.
- Builds swap calldata and liquidation calldata in Rust, signs offline, broadcasts to
  every configured endpoint.
- Spams the same liquidation from several derived accounts to win gas races, and watches
  receipts to blacklist consistently reverting borrowers.
- Keeps a funded pool of "soldier" accounts topped up through one Multicall3 transaction.

## Architecture

| Path | Responsibility |
| --- | --- |
| `sliquidator/src/main.rs` | CLI, logging, base IPC provider, broadcast providers, soldier bootstrap, one task tree per Silo market |
| `sliquidator/src/config.rs` | JSON config types, oracle tags, chain constants |
| `sliquidator/src/solvency.rs` | shared liquidation context, borrower discovery, watchlist refresh, receipt monitor |
| `sliquidator/src/solvency/common.rs` | calldata builders, nonce/gas handling, signing, fan-out broadcast |
| `sliquidator/src/solvency/same_token_strategy.rs` | positions where collateral and debt are the same token |
| `sliquidator/src/solvency/swap_strategy.rs` | positions that need a collateral → debt swap, including oracle simulation |
| `sliquidator/src/oracles/hermes.rs` | Pyth Hermes websocket client and update decoding |
| `sliquidator/src/oracles/simulator.rs` | storage-override simulation for Chainlink and Pyth oracles |
| `common/src/oracles/chainlink.rs` | pending-transaction listener for Chainlink reports, price decoding, state overrides |
| `common/src/soldiers/soldier_manager.rs` | BIP-32 soldier derivation, balance checks, batched funding |
| `contract/src/Liquidator.sol` | flashloan + liquidation + swap helper, see `contract/README.md` |

The `sliquidator` crate owns the event loop and strategy. The `common` crate owns the
pieces that are generic enough to be shared: oracle plumbing on top of `alloy` and the
soldier account manager.

## The liquidation path

1. **Bootstrap.** Parse CLI arguments, start logging, connect the read/observe provider
   over IPC with the base key, connect broadcast providers without any wallet attached,
   optionally derive and fund the soldier accounts.
2. **Discovery.** Subscribe to `Transfer` events for each market's debt token. Any address
   with a non-zero debt-token balance is added to the borrower set and persisted to the
   configured file.
3. **Watchlist refresh.** Every `all_borrowers_full_health_refresh_secs`, call
   `ISiloLens.getUsersHealth` in batches of 50. Borrowers whose LTV is within the
   configured gap of their liquidation threshold go on the watchlist with their
   collateral and debt tokens. The gap check is relaxed for same-token and stable
   markets.
4. **Fast loop.** Every `near_insolvency_check_millis`, for each watched borrower do a
   multicall of `hook.maxLiquidation(borrower)` and `silo.isSolvent(borrower)`. Two
   calls, one round trip.
5. **Verify or simulate.** If the position is already insolvent and `maxDebt` clears the
   market's minimum, liquidate directly. If it is still solvent, either simulate with
   the expected oracle price (different tokens) or project LTV forward using recent
   blocks (same token).
6. **Build and bid.** Encode `executeLiquidation`, `executeLiquidationBlind`, or
   `updatePythPriceFeedsAndExecuteLiquidation`, fetch nonce and gas price from the base
   provider, sign locally, then send the signed envelope to every broadcast endpoint in
   parallel. The same calldata is also sent from every soldier account, each with its own nonce.
7. **Settle.** A receipt monitor watches pending hashes. Reverted liquidations add the
   borrower to a blacklist so the loop does not burn gas on it repeatedly; the blacklist
   is cleared once a day.

### Swap calldata

For a collateral → debt swap the bot encodes
`exactInputSingle(tokenIn=collateral, tokenOut=debt, tickSpacing=<market>, recipient=<helper>, deadline=now+300, amountIn=collateralToLiquidate*995/1000, amountOutMinimum=0)`.
Swapping 99.5% of the collateral leaves a small buffer inside the helper. The contract
requires the flashloan plus fee to be repayable from the swap output, otherwise the whole
transaction reverts. Any leftover collateral dust stays in the helper and can be
withdrawn later, or is recovered by a later liquidation of the opposite side.

## Safety and fail-closed behaviour

The code prefers doing nothing over sending an obviously bad transaction:

- **Minimum amounts.** Each market has `min_amounts_for_liquidation` for the base and
  simulated strategies. Below the minimum, the bot skips and drops the borrower from the
  watchlist instead of retrying forever.
- **Simulation before sending.** With Chainlink and Pyth the bot executes the
  `maxLiquidation`/`isSolvent` multicall against overridden oracle storage. If the
  position is still solvent at the simulated price, nothing is sent.
- **Projection guard.** For same-token positions the LTV delta per block is bounded. A
  position whose LTV is moving too fast is not blindly liquidated from a projection.
- **Bounded blind attempts.** Projected liquidations watch a fixed number of blocks and
  then stop.
- **Atomic economics.** The helper reverts with `NoDebtToCover` when the hook reports no
  debt and with `UnableToRepayFlashloan` when the swap cannot cover the flashloan. Bad
  debt is not socialised by the bot.
- **Receipt-driven blacklist.** A reverted liquidation blacklists the borrower until the
  daily reset.
- **Funding guard.** The periodic soldier refunder checks the base account balance
  against the total top-up before sending a Multicall3 batch, and skips the round if the
  base account cannot cover it.
- **No wallet on broadcast providers.** Transactions are built and signed against the
  observe provider, then sent as raw envelopes. RPC endpoints never see a private key.
- **Startup validation.** `--soldiers > 0` requires a mnemonic file, and a target below
  the minimum is rejected before any work starts.

## Design decisions and trade-offs

- **Separate observe and broadcast paths.** Reads go through a local IPC node; writes go
  through as many HTTP endpoints as configured. It costs a fan-out, but a single slow RPC
  no longer decides whether a liquidation lands.
- **Sign once, broadcast everywhere.** `alloy` is used to build and sign the transaction
  once, then the raw envelope is cloned across providers. The alternative, one wallet per
  provider, adds nonce management for no benefit.
- **Multicall instead of multiple awaits.** Health and hook limits are fetched together,
  which matters when the whole decision has a few milliseconds of budget.
- **State overrides instead of forking.** Chainlink reports and Pyth updates are injected
  into a single simulated call rather than running an archive node or a local fork.
- **Simple gas pricing.** Earlier versions hand-tuned EIP-1559 fields; the current path
  asks the node for a gas price and keeps a fixed gas limit. Competition is handled by
  broadcasting and by spamming from several accounts, not by guessing a tip.
- **Soldiers are cheap parallelism.** Each soldier is a derived EOA funded through one
  Multicall3 batch. Only one transaction per account can land, so the cost is bounded by
  the accounts actually funded.
- **Small helper contract.** All the economics live in one stateless Solidity file. The
  Rust side only prepares calldata, which keeps the on-chain surface auditable.

## Build and test

Rust workspace:

```shell
cargo build --release
cargo test
```

The unit tests cover the small, deterministic pieces: percentage scaling and the packed
storage layouts used for oracle overrides. The network paths need a node and are not
covered by offline tests; run the binary against a local anvil instance to exercise them.

Solidity module:

```shell
git submodule update --init --recursive
cd contract
forge build
```

The submodules pin OpenZeppelin, the Silo v2 contracts, and the Pyth SDK. The
deployment script also imports `forge-std`, so install it with
`forge install foundry-rs/forge-std` if your checkout does not already have it. There
are no Foundry tests in this repository; the helper was integration-tested against the
real Silo deployment.

## Running it against a dummy config

Create a config with placeholder addresses. Nothing will liquidate on a dummy market,
but the process starts, loads the config, and exercises the read, logging, and RPC paths:

```json
{
  "fast_check_solvency_threshold_percentage": 0.5,
  "fast_check_solvency_threshold_same_token_percentage": 0.05,
  "fast_check_solvency_threshold_stable_market_percentage": 0.005,
  "all_borrowers_full_health_refresh_secs": 30,
  "near_insolvency_check_millis": 5,
  "silos": [
    {
      "_": "local dummy market",
      "config_address": "0x1000000000000000000000000000000000000001",
      "silo_address": "0x1000000000000000000000000000000000000002",
      "hook_address": "0x1000000000000000000000000000000000000003",
      "debt_token_address": "0x1000000000000000000000000000000000000004",
      "borrowers_file": "sliquidator/seeds/dummy.csv",
      "oracle": { "type": "Chainlink" },
      "swap_pool_tick_spacing": 50,
      "min_amounts_for_liquidation": [1000000, 1000000],
      "is_stable_market": false
    }
  ]
}
```

Create the borrower file with one address per line (a single address is enough), and put
the base key in a file outside version control:

```shell
printf '0x0000000000000000000000000000000000000000\n' > sliquidator/seeds/dummy.csv
printf '%s\n' "$BASE_PRIVATE_KEY" > base_key
chmod 600 base_key

cargo run --release --bin sliquidator -- \
  --log-console \
  --config dummy_silos.json \
  --base-ipc /tmp/anvil.ipc \
  --base-key-file base_key \
  --soldiers 4 \
  --soldier-mnemonic-file soldier_mnemonic.txt \
  --soldier-min-wei 100000000000000000 \
  --soldier-target-wei 500000000000000000 \
  --endpoint http://127.0.0.1:8545
```

`silos.json` in the repository is a real, public market configuration and can be used as
a reference for the shape of the file. Key files, mnemonic files, and `*.env` are
gitignored and are never committed.

## Repository layout

```
.
├── sliquidator/        # the bot: CLI, solvency strategies, oracle clients
├── common/             # shared oracle plumbing and soldier manager
├── contract/           # Foundry project with the liquidation helper
├── silos.json          # public Silo market configuration
└── run_sliquidator.sh  # example launch command
```

## Known limits

- The bot targets one chain per process and one JSON config per run.
- The Redstone oracle path does basic liquidation only; there is no Redstone price
  simulation.
- Observability is structured logging to console or a rolling file. There is no metrics
  endpoint.
- The project is tuned for the Sonic deployment of Silo. Addresses are configuration, not
  hard-coded strategy, but the swap and oracle paths assume its contracts.

---

Built by [Stefano Candori](https://github.com/cando).
