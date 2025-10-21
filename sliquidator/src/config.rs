use alloy::primitives::{address, Address, FixedBytes};
use serde::Deserialize;

pub const LIQUIDATOR_ADDRESS: Address = address!("dead000000000000000000000000000000000005");

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub silos: Vec<SiloConfig>,
    pub fast_check_solvency_threshold_percentage: f64,
    pub fast_check_solvency_threshold_same_token_percentage: f64,
    pub fast_check_solvency_threshold_stable_market_percentage: f64,
    pub all_borrowers_full_health_refresh_secs: u64,
    pub near_insolvency_check_millis: u64,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", content = "config")]
pub enum Oracle {
    Chainlink,
    Pyth {
        price_id: String,
        slot_0: FixedBytes<32>,
    },
    Redstone,
}
#[derive(Debug, Clone, Deserialize)]
pub struct SiloConfig {
    pub config_address: Address,
    pub silo_address: Address,
    pub hook_address: Address,
    pub debt_token_address: Address,
    pub borrowers_file: String,
    pub oracle: Oracle,
    pub swap_pool_tick_spacing: usize,
    pub min_amounts_for_liquidation: Vec<u64>, // [amount_base_solvency_tier, amount_simulated_solvency_tier]
    pub is_stable_market: bool,
}

pub fn load_app_config(path: &str) -> eyre::Result<AppConfig> {
    let file = std::fs::read_to_string(path)?;
    let config: AppConfig = serde_json::from_str(&file)?; // or serde_yaml::from_str()
    Ok(config)
}
