use alloy::{
    network::{EthereumWallet, TransactionBuilder},
    primitives::{Address, U256},
    providers::Provider,
    rpc::types::TransactionRequest,
    signers::{local::PrivateKeySigner, Signer},
    sol,
    sol_types::SolInterface,
};
use bip32::{secp256k1::elliptic_curve::rand_core::OsRng, DerivationPath, Mnemonic, XPrv};
use eyre::{eyre, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use hex::ToHex;
use std::{fs, path::Path, str::FromStr, time::Duration};
use tokio::time::interval;
use tracing::{error, info, warn};

use crate::soldiers::soldier_manager::IMulticall3::aggregate3ValueCall;

sol! {
    struct Call3Value {
        address target;
        bool allowFailure;
        uint256 value;
        bytes callData;
    }

    struct Result3 {
        bool success;
        bytes returnData;
    }

    #[sol(rpc)]
    interface IMulticall3 {
        function aggregate3Value(Call3Value[] calldata calls)
            external
            payable
            returns (Result3[] memory returnData);
    }
}

/// Manages N soldiers derived from a single mnemonic (bip32).
/// Derivation base: m/44'/60'/0'/0/
pub struct SoldierManager<P>
where
    P: Provider + Clone + 'static,
{
    base_provider: P,
    base_signer: PrivateKeySigner,
    chain_id: u64,

    mnemonic: Mnemonic,
    derivation_base: String,

    multicall3_address: Address,
}

impl<P> SoldierManager<P>
where
    P: Provider + Clone + 'static,
{
    /// Load a mnemonic from file if exists; otherwise generate and save a new one (English).
    pub fn load_or_create_mnemonic(mnemonic_path: &str) -> Result<Mnemonic> {
        let path = Path::new(mnemonic_path);
        if path.exists() {
            let content = fs::read_to_string(path)?.trim().to_string();
            return Ok(Mnemonic::new(&content, Default::default())?);
        }
        let mnemonic = Mnemonic::random(&mut OsRng, Default::default());
        let phrase = mnemonic.phrase();
        fs::write(path, phrase)?;
        info!(
            "Generated new soldier mnemonic and saved to {}",
            mnemonic_path
        );
        Ok(mnemonic)
    }

    /// Build from mnemonic file (auto-creates on first run)
    pub fn from_mnemonic_file(
        base_provider: P,
        base_signer: PrivateKeySigner,
        chain_id: u64,
        mnemonic_path: &str,
        multicall3_address: Address,
    ) -> Result<Self> {
        let mnemonic = Self::load_or_create_mnemonic(mnemonic_path)?;
        Ok(Self {
            base_provider,
            base_signer: base_signer.with_chain_id(Some(chain_id)),
            chain_id,
            mnemonic,
            derivation_base: "m/44'/60'/0'/0/".to_string(),
            multicall3_address,
        })
    }

    /// Derive the i-th soldier signer (bip32).
    pub fn derive_signer(&self, i: u32) -> Result<PrivateKeySigner> {
        let seed = self.mnemonic.to_seed(""); // empty passphrase
        let path_str = format!("{}{}", self.derivation_base, i);
        let path = DerivationPath::from_str(&path_str)
            .map_err(|e| eyre!("Invalid derivation path {path_str}: {e}"))?;
        let xprv = XPrv::derive_from_path(seed.as_bytes(), &path)?;
        let sk_bytes = xprv.private_key().to_bytes();
        let pk_hex = format!("0x{}", sk_bytes.encode_hex::<String>());
        Ok(pk_hex
            .parse::<PrivateKeySigner>()?
            .with_chain_id(Some(self.chain_id)))
    }

    #[inline]
    pub fn soldier_address(&self, i: u32) -> Result<Address> {
        Ok(self.derive_signer(i)?.address())
    }

    pub async fn balance(&self, addr: Address) -> Result<U256> {
        self.base_provider
            .get_balance(addr)
            .await
            .map_err(|e| eyre!("get_balance failed: {e}"))
    }

    /// Batch fund with Multicall3.aggregate3Value (ONE tx). Fire-and-forget.
    async fn fund_batch(&self, transfers: Vec<(Address, U256)>) -> Result<()> {
        let mut calls: Vec<Call3Value> = Vec::with_capacity(transfers.len());
        let mut total = U256::ZERO;

        for (to, amount) in transfers {
            if amount.is_zero() {
                continue;
            }
            total = total.saturating_add(amount);
            calls.push(Call3Value {
                target: to,
                allowFailure: false, // set true for partial success if desired
                value: amount,
                callData: Vec::new().into(), // empty calldata => native transfer
            });
        }
        if total.is_zero() {
            return Ok(());
        }

        let call_data =
            IMulticall3::IMulticall3Calls::aggregate3Value(aggregate3ValueCall { calls })
                .abi_encode();

        let nonce = self
            .base_provider
            .get_transaction_count(self.base_signer.address())
            .await?;

        let gas = self.base_provider.get_gas_price().await?;

        let tx = TransactionRequest::default()
            .with_from(self.base_signer.address())
            .with_to(self.multicall3_address)
            .with_nonce(nonce)
            .with_input(call_data)
            .with_gas_limit(2_500_000u64)
            .with_value(total)
            .with_gas_price(gas);

        let tx_envelope = tx
            .build(&EthereumWallet::from(self.base_signer.clone()))
            .await?;

        let pending_tx = self.base_provider.send_tx_envelope(tx_envelope).await?;

        let receipt = pending_tx.get_receipt().await?;
        info!(
            "Multicall3 funding tx succeeded in block {:?}, hash: {:?}",
            receipt.block_number, receipt.transaction_hash
        );

        Ok(())
    }

    /// Bootstrap soldiers:
    ///  - derive all
    ///  - read all balances in parallel
    ///  - send **one** multicall for all underfunded
    pub async fn bootstrap_soldiers(
        &self,
        n: u32,
        min: U256,
        target: U256,
    ) -> Result<Vec<EthereumWallet>> {
        // derive all
        let mut wallets = Vec::with_capacity(n as usize);
        for i in 0..n {
            wallets.push(EthereumWallet::from(self.derive_signer(i)?));
        }

        // read balances in parallel (unbounded for our scale)
        let mut futs = FuturesUnordered::new();
        for w in &wallets {
            let addr = w.default_signer().address();
            let mgr = self.clone();
            futs.push(async move { (addr, mgr.balance(addr).await) });
        }

        let mut needed: Vec<(Address, U256)> = Vec::new();
        while let Some((addr, res)) = futs.next().await {
            match res {
                Ok(bal) if bal < min => {
                    let delta = target.saturating_sub(bal);
                    if !delta.is_zero() {
                        needed.push((addr, delta));
                    }
                }
                Ok(_) => {}
                Err(e) => warn!("bootstrap balance error for {addr:?}: {e}"),
            }
        }

        if !needed.is_empty() {
            self.fund_batch(needed).await?;
        } else {
            info!("Bootstrap: all soldiers >= min; nothing to fund.");
        }

        Ok(wallets)
    }

    /// Periodic task every `every`:
    ///  - read all balances in parallel
    ///  - compute total needed; if base < total_needed => log error & SKIP
    ///  - else send **one** multicall
    pub async fn run_periodic_funding_task(
        &self,
        n: u32,
        min: U256,
        target: U256,
        every: Duration,
    ) {
        let mut tick = interval(every);

        // Skip the very first tick (so we start only after `every`)
        tick.tick().await;

        loop {
            tick.tick().await;

            let base_addr = self.base_signer.address();
            info!(
                "Periodic soldier funding check starting... base={:?}",
                base_addr
            );

            // parallel balance reads
            let mut futs = FuturesUnordered::new();
            for i in 0..n {
                let addr = match self.soldier_address(i) {
                    Ok(a) => a,
                    Err(e) => {
                        warn!("derive address for soldier#{i} failed: {e}");
                        continue;
                    }
                };
                let mgr = self.clone();
                futs.push(async move { (addr, mgr.balance(addr).await) });
            }

            let mut needed: Vec<(Address, U256)> = Vec::new();
            let mut total_needed = U256::ZERO;

            while let Some((addr, res)) = futs.next().await {
                match res {
                    Ok(bal) if bal < min => {
                        let delta = target.saturating_sub(bal);
                        if !delta.is_zero() {
                            total_needed = total_needed.saturating_add(delta);
                            needed.push((addr, delta));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => warn!("periodic balance error for {addr:?}: {e}"),
                }
            }

            // base balance check BEFORE funding
            match self.balance(base_addr).await {
                Ok(base_bal) => {
                    if base_bal < total_needed {
                        error!(
                            "Base balance insufficient: need {}, base {}. Skipping funding round.",
                            total_needed, base_bal
                        );
                        continue;
                    }
                }
                Err(e) => {
                    error!("Failed to read base balance: {e}. Skipping funding round.");
                    continue;
                }
            }

            if needed.is_empty() {
                info!("Periodic check: all soldiers >= min; nothing to fund.");
                continue;
            }

            if let Err(e) = self.fund_batch(needed).await {
                warn!("Periodic funding multicall failed: {e}");
            } else {
                info!("Periodic soldier funding round completed.");
            }
        }
    }
}

impl<P> Clone for SoldierManager<P>
where
    P: Provider + Clone + 'static,
{
    fn clone(&self) -> Self {
        Self {
            base_provider: self.base_provider.clone(),
            base_signer: self.base_signer.clone(),
            chain_id: self.chain_id,
            mnemonic: self.mnemonic.clone(),
            derivation_base: self.derivation_base.clone(),
            multicall3_address: self.multicall3_address,
        }
    }
}
