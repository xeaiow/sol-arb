use anyhow::Result;
use base64::Engine as _;
use solana_sdk::transaction::VersionedTransaction;

#[derive(Clone)]
pub struct NozomiSender {
    url: String,
    client: reqwest::Client,
}

impl NozomiSender {
    pub fn new(api_key: &str, region: &str) -> Self {
        let base = if region.is_empty() {
            "https://nozomi.temporal.xyz".to_string()
        } else {
            format!("https://{}.nozomi.temporal.xyz", region)
        };
        let url = format!("{}/api/sendTransaction2?c={}", base, api_key);
        Self {
            url,
            client: reqwest::Client::new(),
        }
    }

    /// Send pre-serialized base64 transaction (API v2 — fastest, no JSON overhead)
    pub async fn send_raw(&self, tx_base64: &str) -> Result<()> {
        let resp = self.client
            .post(&self.url)
            .header("Content-Type", "text/plain")
            .body(tx_base64.to_string())
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Nozomi HTTP {} ({}): {}", status, &self.url[..40], text);
        }
        Ok(())
    }

    pub async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<()> {
        let tx_bytes = bincode::serialize(tx)?;
        let tx_base64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);
        self.send_raw(&tx_base64).await
    }
}
