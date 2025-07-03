use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use alloy::{
    primitives::{Address, U256},
    providers::Provider,
};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::{
    hermes::SharedPythPrice,
    types::{
        IHookReceiver::IHookReceiverInstance, ILiquidationHelper::ILiquidationHelperInstance,
        ISilo::ISiloInstance, ISiloConfig::ISiloConfigInstance, ISiloLens::ISiloLensInstance,
        SharedAddressList,
    },
};

// Configuration struct for bot-wide constants
#[derive(Debug, Clone, Deserialize)]
pub struct SiloConfig {
    pub config_address: Address,
    pub silo_address: Address,
    pub hook_address: Address,
    pub debt_token_address: Address,
    pub borrowers_file: String,
    pub min_amounts_for_liquidation: HashMap<Address, u64>,
    pub simulate_solvency_price_id: Option<String>,
}

#[derive(Clone)]
pub enum SolvencyMode {
    Standard,
    SimulatedPyth {
        price_id: String,
        price_feed: SharedPythPrice,
    },
}

#[derive(Clone)]
pub struct LiquidationContext<P: Provider + Clone + 'static> {
    pub provider: P,
    pub lens: ISiloLensInstance<P>,
    pub helper: ILiquidationHelperInstance<P>,
    pub silo_config: ISiloConfigInstance<P>,
    pub config: SiloConfig,
    pub hook: IHookReceiverInstance<P>,
    pub silo: ISiloInstance<P>,
    pub mode: SolvencyMode,
    pub watchlist: Arc<RwLock<Vec<(Address, Address, Address)>>>,
    pub blacklist: Arc<RwLock<HashSet<Address>>>,
    pub borrowers: SharedAddressList,
    pub fast_check_threshold: U256,
    pub liquidation_threshold: U256,
}

pub fn load_silo_configs(path: &str) -> eyre::Result<Vec<SiloConfig>> {
    let file = std::fs::read_to_string(path)?;
    let configs: Vec<SiloConfig> = serde_json::from_str(&file)?; // or serde_yaml::from_str()
    Ok(configs)
}
