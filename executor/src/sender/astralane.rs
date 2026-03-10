use anyhow::Result;
use base64::Engine as _;
use solana_sdk::transaction::VersionedTransaction;

#[derive(Clone)]
pub struct AstralaneSender {
    endpoint: String,
    api_key: String,
    client: reqwest::Client,
}

impl AstralaneSender {
    pub fn new(endpoint: String, api_key: String) -> Self {
        Self {
            endpoint,
            api_key,
            client: reqwest::Client::new(),
        }
    }

    pub async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<()> {
        let tx_bytes = bincode::serialize(tx)?;
        let tx_base64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

        let url = format!(
            "{}?api-key={}&method=sendTransaction",
            self.endpoint, self.api_key,
        );

        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "text/plain")
            .body(tx_base64)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Astralane HTTP {}: {}", status, text);
        }

        log::info!("Astralane sent OK: {}", self.endpoint);
        Ok(())
    }
}
