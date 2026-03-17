use anyhow::Result;
use base64::Engine as _;
use solana_sdk::transaction::VersionedTransaction;

/// Astralane bundle sender via sendBundle JSON-RPC on iris2 endpoint.
/// Failed bundles don't land on-chain and don't cost fees.
#[derive(Clone)]
pub struct AstralaneSender {
    endpoint: String,
    api_key: String,
    client: reqwest::Client,
    /// Pre-built URL with api-key query param
    url: String,
}

impl AstralaneSender {
    pub fn new(endpoint: String, api_key: String) -> Self {
        let url = format!("{}?api-key={}", endpoint, api_key);
        Self {
            endpoint,
            api_key,
            client: reqwest::Client::new(),
            url,
        }
    }

    /// Send a single transaction as an Astralane bundle (sendBundle).
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
            "params": [[tx_base64]]
        });

        let resp = self.client
            .post(&self.url)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Astralane HTTP {} ({}): {}", status, self.endpoint, text);
        }

        Ok(())
    }
}
