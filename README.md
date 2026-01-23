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

At the beginning the bot only looked at state that was already mined, so it saw a
position go underwater after the fact. That is a fine way to watch a market and a bad way
to win a liquidation. The fix was to listen to the pending pool as well, starting with
Chainlink report transactions for the markets I cared about, so a liquidation could be
spotted and prepared before the block that made it profitable landed.

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

## Safety and fail-closed behaviour

The code prefers doing nothing over sending an obviously bad transaction:

- **Simulate first.** Chainlink and Pyth positions are checked against overridden oracle
  storage, and same-token positions against a bounded LTV projection. If the position is
  still healthy at the simulated price, nothing is sent.
- **Respect the minimums.** Each market sets base and simulated thresholds; below them the
  borrower is skipped and dropped from the watchlist instead of being retried forever.
- **Let the contract fail closed.** The helper reverts when the hook reports no debt or
  when the swap cannot repay the flashloan. Bad debt is not socialised, and a losing
  attempt costs gas only.
- **Blacklist on revert.** A reverted liquidation removes the borrower from the loop until
  the daily blacklist reset.
- **No wallet on broadcast providers.** Transactions are signed against the observe
  provider and sent as raw envelopes; RPC endpoints never see a private key.
- **Validate at startup.** Soldier counts, mnemonic files and target/minimum balances are
  checked before any work starts.

## Design decisions and trade-offs

The shape of the system is deliberate:

- **Reads over local IPC.** A local node is the fastest and most consistent view of the
  chain, so all state reads and simulations go through it.
- **Writes over HTTP fan-out.** Signed transactions are sent to several RPC endpoints at
  once, so one slow provider cannot decide whether a liquidation lands.
- **One small on-chain helper.** Flashloan, `liquidationCall`, swap and repayment happen
  atomically in `Liquidator.sol`; the Rust side only prepares calldata.
- **Simulation instead of forking.** Chainlink reports and Pyth updates are injected as
  storage overrides into a single call, no archive node required.
- **One event loop per market.** Discovery, watchlist refresh, the fast solvency loop and
  receipt tracking run as separate tasks over shared state.
- **Sonic and Silo.** The bot targets Silo v2 markets on Sonic; addresses come from config,
  but the swap and oracle paths assume that deployment.

The trade-offs that matter most:

- **Sign once, broadcast everywhere.** `alloy` builds and signs the transaction once, then
  the raw envelope is cloned across providers. One wallet per provider would add nonce
  management for no benefit.
- **Multicall instead of multiple awaits.** Hook limits and solvency are fetched together,
  which matters when a decision has a few milliseconds of budget.
- **Simple gas pricing.** Earlier versions hand-tuned EIP-1559 fields; the surviving path
  asks the node for a gas price and keeps a fixed gas limit. Competition is handled by
  broadcasting and spamming from several accounts, not by guessing a tip.
- **Soldiers are cheap parallelism.** Each soldier is a derived EOA funded in one
  Multicall3 batch; only one transaction per account can land, so cost tracks the
  accounts actually funded.

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
