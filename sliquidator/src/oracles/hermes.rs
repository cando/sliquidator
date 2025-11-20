use pyth_hermes_client_rust::{
    backoff::HermesExponentialBackoffBuilder,
    client::HermesClientBuilder,
    ws_connection::{
        HermesClientMessageSubscribe, HermesPrice, HermesServerMessage, HermesServerResponseMessage,
    },
};

use std::sync::Arc;
use tokio::{pin, sync::RwLock};
use tracing::{error, info, warn};

pub type SharedPythPrice = Arc<RwLock<Option<(HermesPrice, String)>>>;

pub async fn hermes_price_feed_task(price_ids: Vec<String>, shared_price: SharedPythPrice) {
    let mut client = match HermesClientBuilder::default()
        .with_backoff(HermesExponentialBackoffBuilder::default().build())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            error!("[hermes] Failed to build Hermes client: {e}");
            return;
        }
    };

    let stream = match client.start().await {
        Ok(s) => s,
        Err(e) => {
            error!("[hermes] Failed to start Hermes client connection stream: {e}");
            return;
        }
    };
    pin!(stream);

    let subscribe_request = HermesClientMessageSubscribe {
        ids: price_ids.clone(),
        verbose: true,
        binary: true,
        allow_out_of_order: true,
        ignore_invalid_price_ids: false,
    };

    match client.subscribe(subscribe_request).await {
        Ok(_) => {}
        Err(e) => {
            error!("[hermes] Failed to subscribe to Hermes: {e}");
            return;
        }
    };

    info!("[hermes] connected to SSE stream");

    while let Some(msg) = stream.recv().await {
        match msg {
            HermesServerMessage::PriceUpdate { price_feed } => {
                {
                    let mut w = shared_price.write().await;
                    *w = Some((price_feed.price.clone(), price_feed.vaa.unwrap_or_default()));
                }

                info!(
                    "[hermes] {} = {} @ {}",
                    price_feed.id, price_feed.price.price, price_feed.price.publish_time
                );
            }
            HermesServerMessage::Response(response) => match response {
                HermesServerResponseMessage::Success => {
                    info!("Received success response from Hermes server.");
                }
                HermesServerResponseMessage::Err { error } => {
                    error!("Received error response from Hermes server: {:?}", error);
                }
            },
        }
    }
    warn!("[hermes] stream closed after full read");
}
