use anyhow::Result;
use base64::Engine as _;
use solana_sdk::transaction::VersionedTransaction;

#[derive(Clone)]
pub struct AstralaneSender {
    endpoint: String,
    api_key: String,
    client: reqwest::Client,
    /// Pre-built URL (avoids format! on hot path)
    url: String,
}

impl AstralaneSender {
    pub fn new(endpoint: String, api_key: String) -> Self {
        let url = format!("{}?api-key={}&method=sendTransaction", endpoint, api_key);
        Self {
            endpoint,
            api_key,
            client: reqwest::Client::new(),
            url,
        }
    }

    /// Send pre-serialized base64 transaction. Does not wait for response body.
    pub async fn send_raw(&self, tx_base64: &str) -> Result<()> {
        let resp = self
            .client
            .post(&self.url)
            .header("Content-Type", "text/plain")
            .body(tx_base64.to_string())
            .send()
            .await?;

        // Only check status code, don't read body (fire-and-forget)
        if !resp.status().is_success() {
            let status = resp.status();
            // Read body only on error for diagnostics
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Astralane HTTP {} ({}): {}", status, self.endpoint, text);
        }

        Ok(())
    }

    /// Convenience: serialize + send (used when only one endpoint)
    pub async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<()> {
        let tx_bytes = bincode::serialize(tx)?;
        let tx_base64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);
        self.send_raw(&tx_base64).await
    }
}
