use solana_sdk::transaction::Transaction;
use anyhow::Result;

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

    pub async fn send_transaction(&self, _tx: &Transaction) -> Result<()> {
        // TODO: Implement Flashblock sendTransaction
        log::debug!("Flashblock sendTransaction to {}", self.endpoint);
        Ok(())
    }
}
