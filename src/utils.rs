use std::fs;

use alloy::primitives::{Address, U256};
use rpassword::prompt_password;

pub fn load_borrowers(file_path: &str) -> Vec<Address> {
    let content = fs::read_to_string(file_path)
        .unwrap_or_else(|e| panic!("Failed to read borrower file: {e}"));

    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            match trimmed.parse::<Address>() {
                Ok(addr) => Some(addr),
                Err(e) => {
                    tracing::warn!("Invalid address '{}': {e}", trimmed);
                    None
                }
            }
        })
        .collect()
}

pub fn read_private_key_from_console() -> eyre::Result<String> {
    let key = prompt_password("Enter your private key (0x...): ")?;
    if key.trim().is_empty() {
        eyre::bail!("Private key cannot be empty.");
    }
    Ok(key.trim().to_owned())
}

pub fn read_private_key_from_file(path: &str) -> eyre::Result<String> {
    let key = fs::read_to_string(path)?.trim().to_owned();
    if key.is_empty() {
        eyre::bail!("Private key is empty");
    }
    Ok(key.trim().to_owned())
}

pub fn scale_to_wei(amount: U256, token_decimals: u8) -> U256 {
    match token_decimals {
        18 => amount,
        d if d < 18 => {
            let scale = 10u128.pow((18 - d) as u32);
            amount * U256::from(scale)
        }
        d => {
            let scale = 10u128.pow((d - 18) as u32);
            amount / U256::from(scale)
        }
    }
}

pub fn percent_to_wei(percentage: f64) -> U256 {
    // Convert to fractional (e.g., 0.01% -> 0.0001)
    let fractional = percentage / 100.0;

    // Scale up to 18 decimal places
    let scaled = fractional * 1e18;

    U256::from(scaled as u128)
}
