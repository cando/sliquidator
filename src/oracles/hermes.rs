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
    binary: BinaryData,
    parsed: Vec<ParsedPriceUpdate>,
}

#[derive(Debug, Deserialize)]
struct BinaryData {
    data: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ParsedPriceUpdate {
    id: String,
    price: Price,
}

pub type SharedPythPrice = Arc<RwLock<Option<(Price, Vec<String>)>>>;

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
                            // TODO REMOVE THISSSS
                            let price = Price {
                                price: 284885100,
                                conf: 1795945,
                                expo: -8,
                                publish_time: 1752681181,
                            };
                            let fake_data_for_testing = vec!["504e41550100000003b801000000040d02ff13b3e92a3eaae45a2faefd5e816d08db9081c5774b869544622c5b68f7c4930784285bf06af64a693f0ebefe1a7ec62dff30acc719dce34e37b100ac30c71a0003815642c3da05867af34696658eabb127dc72a442e4b6d0d5d5742b4f253802473ba4d69f04e194018de663b4d6ee900a5da48b0f34cf06dbe3f657443b776f160104d4c50ee2c42d7664b391bc430ec28d5b9d4f46cd9be592402aaa0378931b00e15726859e2a30bf37aba030d92e727c85b0e1c0fb7d92516d35297b2549623dde00061d1c81f3405ccdb8d7558bb0ce93ecd3df18323e70758a34b57c5813034d9bf95ca436bc8a69bc433ba26562396f4ae688864098267ce6993cd60ec40b94d6cf00083b9057d9239aaa13ffedff2fb4e5ce5073131e7568c66459adad2a6f12f6ad1852082c2a49b35decb737b642c27be890de0bc2a6fd1fa29f5177dbe660c920540009fa9b0ef3855c1be6435cb4128650525649d533f4828ee06e4c2d75f930dcc2c967b48e8380d6d0ac18a15850f6b1a710ad3596c3d6f39e416c76b05a52f79df2000acc9f05e30c802a00effd949c2b0f1b1cac45cf5f1822b03ee15b7fc590fa3ec05c90afd3a9c3d086afdfd4c083102adf7b975efedb267ebb72db6eb87d3552ec010cfc1df2bb4015e3630e0c57bdf9fdec658d2abd6fa296e90e9cc4ab42c3b1fc0947ec7340b128a771d40730c90daf4ee06a366a429262c2fe61ce47842a490333000daab2ecff880b4eec28400360cca06513d8f4f4b346bb30f6ec34ed379ca58a8238e007348a8ee9235a5184a04820544421fb645e2d32461f69ca86f9b0d9d6f6000e6c17545ee59bf5285f275545ddffb4808355ca6946ecff893d84fd4f39f5cc3e7240e036050f9f6b7762ce9a4d999136c1dd3d7ffa24f8a936592006eff3225c010fd1d7e744d1d2c881802f46b58b24688955f34fa37fef26e3064adce3c469fdf51081a6501bd5e2f0f79dfac077be9541121f52a3062cb5f1ecb9ba3a27d45f27001088e7b36a83e1a7c458b26dc4c2d963f6f2c0565d7a3d51799ae7c93a25f356f40d1476449a60da913ff599c67c14fbf294619d9f191f5c7e19bbb84e8707499b0011bb28354b2c48108c90c7081d64308e01fe5b95d0a9884306174629384c503e02227a1c96569cf15f3d3ac1f5cd776111c079a9827e7b270f3e0a0f83cda7506f006877caea00000000001ae101faedac5851e32b9b23b5f9411a8c2bac4aae3ed4dd7b811dd1a72ea4aa710000000008aa59c8014155575600000000000db7e8ea00002710fed9300b75f6fef8ed77f7009324a384bbd9ffd4010055007a36855b8a4a6efd701ed82688694bbf67602de9faae509ae28f91065013cb820000000010f5b7b700000000001d219efffffff8000000006877caea000000006877cae9000000001167848000000000001868d10c31ecac7369d97f1ad0ce6102d4ddd5e663d75d0cf3b6a01f96c36fbf9b0deb33814c9f59b30bc2952171bf3ecfe9c9480ad90eff43a04958e5b1ef1625c5851d70ed06c9c0b37815abdb941442a14ecbfee613a36182d07158c0bcd1366876b97da43920b85ade33585fd0998d47d7745b05c860e95a6cd8686cb4a2264a343c2ef98474dfb33a2923e60ca592d1cb55706fbf4da5dd34ffbcee78d2259d38ce7bb09e0d542edda1fe6f551e7ae6377239db6e24eb9ec77af6eb420857b3cf81ffedcb99654376d0a328f1dc96d54e5dfc85afefebdb73e20f339e5a57e4a4668075cc934a368f88e15a96aca5431a33".to_string()];
                            let mut w = shared_price.write().await;
                            *w = Some((price.clone(), fake_data_for_testing.clone()));
                            // *w = Some((update.price.clone(), event.binary.data.clone()));
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
