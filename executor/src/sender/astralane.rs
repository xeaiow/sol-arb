use std::sync::Arc;
use anyhow::Result;
use astralane_quic_client::AstralaneQuicClient;
use solana_sdk::transaction::VersionedTransaction;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct AstralaneSender {
    endpoint: String,
    client: Arc<Mutex<Option<AstralaneQuicClient>>>,
}

impl AstralaneSender {
    pub fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            client: Arc::new(Mutex::new(None)),
        }
    }

    /// Establish QUIC connection. Called once at startup.
    pub async fn connect(&self, api_key: &str) -> Result<()> {
        let client = AstralaneQuicClient::connect(&self.endpoint, api_key).await?;
        *self.client.lock().await = Some(client);
        log::info!("Astralane QUIC connected: {}", self.endpoint);
        Ok(())
    }

    pub async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<()> {
        let tx_bytes = bincode::serialize(tx)?;
        let guard = self.client.lock().await;
        let client = guard.as_ref().ok_or_else(|| anyhow::anyhow!("QUIC not connected: {}", self.endpoint))?;
        client.send_transaction(&tx_bytes).await?;
        log::info!("Astralane QUIC sent: {} ({} bytes)", self.endpoint, tx_bytes.len());
        Ok(())
    }
}
