use anyhow::Result;
use base64::Engine as _;
use solana_sdk::transaction::VersionedTransaction;

/// Jito bundle sender via HTTP JSON-RPC.
/// Bundles are atomic: all txs succeed or none land on-chain.
/// Failed bundles don't cost any fees.
#[derive(Clone)]
pub struct JitoSender {
    /// HTTP endpoint (e.g. "https://frankfurt.mainnet.block-engine.jito.wtf")
    pub endpoint: String,
    url: String,
    client: reqwest::Client,
}

impl JitoSender {
    pub fn new(endpoint: String) -> Self {
        let url = format!("{}/api/v1/bundles", endpoint);
        Self {
            endpoint,
            url,
            client: reqwest::Client::new(),
        }
    }

    /// Pre-connect is no longer needed (HTTP), but keep the interface.
    pub async fn connect(&mut self) -> Result<()> {
        log::info!("Jito HTTP ready: {}", self.endpoint);
        Ok(())
    }

    /// Send a single transaction as a Jito bundle (sendBundle).
    /// Atomic: failed bundles don't land on-chain and don't cost fees.
    /// Requires the TX to write-lock at least one Jito tip account.
    pub async fn send_bundle(&self, tx: &VersionedTransaction) -> Result<()> {
        let tx_bytes = bincode::serialize(tx)?;
        let tx_base64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);
        self.send_bundle_raw(&tx_base64).await
    }

    /// Send pre-serialized base64 transaction as a single-tx bundle.
    pub async fn send_bundle_raw(&self, tx_base64: &str) -> Result<()> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [
                [tx_base64],
                {"encoding": "base64"}
            ]
        });

        let resp = self.client
            .post(&self.url)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Jito HTTP {} ({}): {}", status, self.endpoint, text);
        }

        Ok(())
    }
}
