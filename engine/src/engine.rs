use tokio::sync::mpsc;

use solana_streamer_sdk::pool::state::PoolUpdate;

use crate::config::EngineConfig;
use crate::opportunity::Opportunity;
use crate::scanner::Scanner;

/// Public entry point for the arbitrage engine.
pub struct Engine {
    scanner: Scanner,
}

impl Engine {
    /// Create a new Engine.
    /// Returns the Engine and a Receiver for Opportunity events.
    pub fn new(
        config: EngineConfig,
        update_rx: mpsc::Receiver<PoolUpdate>,
    ) -> (Self, mpsc::Receiver<Opportunity>) {
        let (opp_tx, opp_rx) = mpsc::channel(4096);
        let scanner = Scanner::new(config, update_rx, opp_tx);
        (Self { scanner }, opp_rx)
    }

    /// Run the engine (blocks forever).
    pub async fn run(mut self) {
        self.scanner.run().await;
    }
}
