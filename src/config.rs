use alloy::primitives::Address;
use serde::Deserialize;

// Configuration struct for bot-wide constants
#[derive(Debug, Clone, Deserialize)]
pub struct SiloConfig {
    pub config_address: Address,
    pub silo_address: Address,
    pub hook_address: Address,
    pub borrowers_file: String,
    pub min_amount_for_liquidation: u64,
}

pub fn load_silo_configs(path: &str) -> eyre::Result<Vec<SiloConfig>> {
    let file = std::fs::read_to_string(path)?;
    let configs: Vec<SiloConfig> = serde_json::from_str(&file)?; // or serde_yaml::from_str()
    Ok(configs)
}
