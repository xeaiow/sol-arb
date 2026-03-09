use solana_sdk::transaction::Transaction;
use anyhow::Result;

#[derive(Clone)]
pub struct JitoSender {
    endpoint: String,
}

impl JitoSender {
    pub fn new(endpoint: String) -> Self {
        Self { endpoint }
    }

    pub async fn send_bundle(&self, _tx: &Transaction) -> Result<()> {
        // TODO: Implement Jito gRPC sendBundle
        log::debug!("Jito sendBundle to {}", self.endpoint);
        Ok(())
    }
}
