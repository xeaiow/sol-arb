use std::sync::Arc;
use anyhow::Result;
use base64::Engine as _;
use astralane_quic_client::AstralaneQuicClient;
use solana_sdk::transaction::VersionedTransaction;
use tokio::sync::Mutex;

/// Dual-channel Astralane sender: QUIC (low latency) + HTTP Iris (proven reliable).
/// Both fire in parallel on every send.
#[derive(Clone)]
pub struct AstralaneSender {
    quic_endpoint: String,
    quic_client: Arc<Mutex<Option<AstralaneQuicClient>>>,
    http_endpoint: String,
    api_key: String,
    http_client: reqwest::Client,
}

impl AstralaneSender {
    pub fn new(quic_endpoint: String, api_key: String) -> Self {
        // Derive HTTP Iris endpoint from QUIC endpoint
        // "fr.gateway.astralane.io:7000" → "https://fr.gateway.astralane.io/iris2"
        let host = quic_endpoint.split(':').next().unwrap_or(&quic_endpoint);
        let http_endpoint = format!("https://{}/iris2", host);

        Self {
            quic_endpoint,
            quic_client: Arc::new(Mutex::new(None)),
            http_endpoint,
            api_key,
            http_client: reqwest::Client::new(),
        }
    }

    /// Establish QUIC connection. Called once at startup.
    pub async fn connect(&self) -> Result<()> {
        let client = AstralaneQuicClient::connect(&self.quic_endpoint, &self.api_key).await?;
        *self.quic_client.lock().await = Some(client);
        log::info!("Astralane QUIC connected: {}", self.quic_endpoint);
        Ok(())
    }

    /// Send via both QUIC and HTTP Iris in parallel.
    pub async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<()> {
        let tx_bytes = bincode::serialize(tx)?;

        // QUIC: fire-and-forget
        {
            let guard = self.quic_client.lock().await;
            if let Some(client) = guard.as_ref() {
                if let Err(e) = client.send_transaction(&tx_bytes).await {
                    log::warn!("Astralane QUIC err {}: {}", self.quic_endpoint, e);
                }
            }
        }

        // HTTP Iris: sendTransaction with skipPreflight
        let tx_base64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);
        let url = format!("{}?api-key={}&method=sendTransaction",
            self.http_endpoint, self.api_key);
        match self.http_client
            .post(&url)
            .header("Content-Type", "text/plain")
            .body(tx_base64)
            .send()
            .await
        {
            Ok(resp) if !resp.status().is_success() => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                log::warn!("Astralane HTTP {} {}: {}", status, self.http_endpoint, text);
            }
            Err(e) => {
                log::warn!("Astralane HTTP err {}: {}", self.http_endpoint, e);
            }
            _ => {}
        }

        Ok(())
    }
}
