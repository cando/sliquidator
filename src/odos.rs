use alloy::primitives::{Address, U256};
use eyre::Result;
use reqwest::Client;
use serde_json::{json, Value};
use tracing::warn;

const ODOS_QUOTE_URL: &str = "https://api.odos.xyz/sor/quote/v2";
const ODOS_ASSEMBLE_URL: &str = "https://api.odos.xyz/sor/assemble";

/// Fetches swap calldata from Odos API for a given input and output token and amount.
/// Returns the raw transaction calldata bytes if successful.
pub async fn get_odos_swap_data(
    receiver: &Address,
    input_token: &Address,
    output_token: &Address,
    amount: U256,
) -> Result<Option<Vec<u8>>> {
    let client = Client::new();

    let quote_body = json!({
        "chainId": 146,
        "inputTokens": [{
            "tokenAddress": format!("{:#x}", input_token),
            "amount": amount.to_string(),
        }],
        "outputTokens": [{
            "tokenAddress": format!("{:#x}", output_token),
            "proportion": 1,
        }],
        "slippageLimitPercent": 5.0,
        "userAddr": format!("{:#x}", receiver),
    });

    let quote_res = client.post(ODOS_QUOTE_URL).json(&quote_body).send().await;

    let path_id = match quote_res {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(json) => json
                .get("pathId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            Err(e) => {
                warn!("Failed to parse Odos quote JSON: {e}");
                None
            }
        },
        Err(e) => {
            warn!("Failed to send Odos quote request: {e}");
            None
        }
    };

    let Some(path_id) = path_id else {
        warn!("Odos quote pathId missing or invalid.");
        return Ok(None);
    };

    let assemble_body = json!({
        "userAddr": format!("{:#x}", receiver),
        "pathId": path_id,
    });

    let assemble_res = client
        .post(ODOS_ASSEMBLE_URL)
        .json(&assemble_body)
        .send()
        .await;

    let calldata = match assemble_res {
        Ok(resp) => match resp.json::<Value>().await {
            Ok(json) => json
                .get("transaction")
                .and_then(|tx| tx.get("data"))
                .and_then(Value::as_str)
                .and_then(|hex| hex.strip_prefix("0x"))
                .and_then(|hex| hex::decode(hex).ok()),
            Err(e) => {
                warn!("Failed to parse Odos assemble JSON: {e}");
                None
            }
        },
        Err(e) => {
            warn!("Failed to send Odos assemble request: {e}");
            None
        }
    };

    Ok(calldata)
}
