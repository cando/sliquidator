use alloy::primitives::Address;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub silo_configs: Vec<SiloConfig>,
    pub fast_check_solvency_threshold_percentage: f64,
    pub fast_check_solvency_threshold_same_token_percentage: f64,
    pub all_borrowers_full_health_refresh_secs: u64,
    pub near_insolvency_check_millis: u64,
}
#[derive(Debug, Clone, Deserialize)]
pub struct SiloConfig {
    pub config_address: Address,
    pub silo_address: Address,
    pub hook_address: Address,
    pub debt_token_address: Address,
    pub borrowers_file: String,
    pub min_amounts_for_liquidation: Vec<u64>, // [amount_base_solvency_tier, amount_simulated_solvency_tier]
    pub simulate_pyth_solvency_price_id: Option<String>,
}

pub fn load_app_config(path: &str) -> eyre::Result<AppConfig> {
    let file = std::fs::read_to_string(path)?;
    let config: AppConfig = serde_json::from_str(&file)?; // or serde_yaml::from_str()
    Ok(config)
}
