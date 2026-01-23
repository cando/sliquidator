use std::fs;

use alloy::primitives::{Address, U256};

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
                    tracing::warn!(?file_path, "Invalid address '{}': {e}", trimmed);
                    None
                }
            }
        })
        .collect()
}

pub fn read_private_key_from_file(path: &str) -> eyre::Result<String> {
    let key = fs::read_to_string(path)?.trim().to_owned();
    if key.is_empty() {
        eyre::bail!("Private key is empty");
    }
    Ok(key.trim().to_owned())
}

pub fn percent_to_wei(percentage: f64) -> U256 {
    // Convert to fractional (e.g., 0.01% -> 0.0001)
    let fractional = percentage / 100.0;

    // Scale up to 18 decimal places
    let scaled = fractional * 1e18;

    U256::from(scaled as u128)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_to_wei_scales_to_eighteen_decimals() {
        assert_eq!(percent_to_wei(0.0), U256::ZERO);
        assert_eq!(percent_to_wei(1.0), U256::from(10_000_000_000_000_000u64));
        assert_eq!(percent_to_wei(50.0), U256::from(500_000_000_000_000_000u64));
        assert_eq!(percent_to_wei(100.0), U256::from(1_000_000_000_000_000_000u64));
    }
}
