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
        log::info!("Astralane tx: {} raw bytes, {} base64 chars, first20={}",
            tx_bytes.len(), tx_base64.len(), &tx_base64[..20.min(tx_base64.len())]);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [tx_base64, {"encoding": "base64", "skipPreflight": true}]
        });

        let resp = self
            .client
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("api_key", &self.api_key)
            .json(&body)
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
