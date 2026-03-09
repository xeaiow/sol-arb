use solana_sdk::transaction::Transaction;
use anyhow::Result;

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

    pub async fn send_transaction(&self, _tx: &Transaction) -> Result<()> {
        // TODO: Implement Astralane sendTransaction
        log::debug!("Astralane sendTransaction to {}", self.endpoint);
        Ok(())
    }
}
