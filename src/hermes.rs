use eyre::Result;
use futures::TryStreamExt;
use reqwest::Client;
use serde::Deserialize;
use serde_with::{serde_as, DisplayFromStr};
use std::sync::Arc;
use tokio::{io::AsyncBufReadExt, sync::RwLock, time::Duration};
use tokio_util::io::StreamReader;
use tracing::{debug, error, info, warn};

#[serde_as]
#[derive(Debug, Clone, Deserialize)]
pub struct Price {
    #[serde_as(as = "DisplayFromStr")]
    pub price: i64,
    #[serde_as(as = "DisplayFromStr")]
    pub conf: u64,
    pub expo: i32,
    pub publish_time: u64,
}

#[derive(Debug, Deserialize)]
struct HermesStreamEvent {
    parsed: Vec<ParsedPriceUpdate>,
}

#[derive(Debug, Deserialize)]
struct ParsedPriceUpdate {
    id: String,
    price: Price,
}

pub type SharedPythPrice = Arc<RwLock<Option<Price>>>;

/// Spawns a long-lived Hermes SSE stream listener as a Tokio task.
/// Continuously updates `SharedPrice` and reconnects after disconnect or 24h limit.
pub async fn hermes_price_feed_task(price_ids: Vec<String>, shared_price: SharedPythPrice) {
    let client = Arc::new(Client::new());

    loop {
        if let Err(err) = stream_once(&client, &price_ids, shared_price.clone()).await {
            error!("[hermes] stream error: {err}, reconnecting in 3s...");
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }
}

async fn stream_once(
    client: &Client,
    price_ids: &[String],
    shared_price: SharedPythPrice,
) -> Result<()> {
    let query = price_ids
        .iter()
        .map(|id| format!("ids[]={}", id))
        .collect::<Vec<_>>()
        .join("&");

    let url = format!("https://hermes.pyth.network/v2/updates/price/stream?{query}");
    let res = client.get(&url).send().await?.error_for_status()?;

    let byte_stream = res
        .bytes_stream()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
    let reader = StreamReader::new(byte_stream);
    let mut lines = tokio::io::BufReader::new(reader).lines();

    info!("[hermes] connected to SSE stream");

    while let Some(line) = lines.next_line().await? {
        if let Some(json_str) = line.strip_prefix("data:") {
            match serde_json::from_str::<HermesStreamEvent>(json_str) {
                Ok(event) => {
                    for update in event.parsed {
                        {
                            let mut w = shared_price.write().await;
                            *w = Some(update.price.clone());
                        }

                        debug!(
                            "[hermes] {} = {} (conf: {}) @ {}",
                            update.id,
                            update.price.price,
                            update.price.conf,
                            update.price.publish_time
                        );
                    }
                }
                Err(e) => {
                    error!("[hermes] JSON error: {e}. Raw: {json_str}");
                }
            }
        }
    }

    warn!("[hermes] stream closed after full read");
    Ok(())
}
