use anyhow::Result;
use base64::Engine as _;
use solana_sdk::transaction::VersionedTransaction;

#[derive(Clone)]
pub struct FlashblockSender {
    endpoint: String,
    api_key: String,
    client: reqwest::Client,
}

impl FlashblockSender {
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

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [tx_base64, {"encoding": "base64"}]
        });

        let resp = self
            .client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Flashblock HTTP {}: {}", status, text);
        }

        log::debug!("Flashblock sent to {}", self.endpoint);
        Ok(())
    }
}
