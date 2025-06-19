use crate::config::SiloConfig;
use crate::types::SharedAddressList;
use crate::types::IERC20;
use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::rpc::types::{BlockNumberOrTag, Filter};
use futures_util::stream::StreamExt;
use std::collections::HashSet;
use tracing::error;
use tracing::info;

pub async fn borrower_listener<P>(
    provider: P,
    silo_config: &SiloConfig,
    borrowers: SharedAddressList,
    known_addresses: HashSet<Address>,
) -> eyre::Result<()>
where
    P: Provider + Clone + 'static,
{
    let filter = Filter::new()
        .address(silo_config.debt_token_address)
        .event("Transfer(address,address,uint256)")
        .from_block(BlockNumberOrTag::Latest);

    let sub = match provider.subscribe_logs(&filter).await {
        Ok(sub) => sub,
        Err(e) => {
            error!("Failed to subscribe to logs: {e}");
            return Err(eyre::eyre!(
                "Failed to subscribe to logs for {}: {e}",
                silo_config.debt_token_address
            ));
        }
    };

    let mut stream = sub.into_stream();

    let erc20 = IERC20::new(silo_config.debt_token_address, provider.clone());

    while let Some(log) = stream.next().await {
        if let Ok(decoded) = log.log_decode::<IERC20::Transfer>() {
            let from = decoded.data().from;
            let to = decoded.data().to;

            for addr in [from, to] {
                if addr == Address::ZERO || known_addresses.contains(&addr) {
                    continue;
                }

                match erc20.balanceOf(addr).call().await {
                    Ok(balance) if balance > U256::ZERO => {
                        let mut list = borrowers.write().await;
                        if list.insert(addr) {
                            info!(
                                "New borrower detected: {addr:#x}. Borrowers count: {}",
                                list.len()
                            );
                        } else {
                            info!("Address {addr:#x} already in borrower list");
                        }
                    }
                    Ok(_) => {
                        info!("Address {addr:#x} has zero balance, removing from borrowers");
                        let mut list = borrowers.write().await;
                        list.remove(&addr);
                    }
                    _ => {}
                }
            }
        }
    }

    Ok(())
}
