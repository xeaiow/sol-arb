use std::sync::Arc;
use log::{info, debug, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signer::keypair::Keypair;
use tokio::sync::mpsc;

use arb_engine::opportunity::Opportunity;

use crate::config::ExecutorConfigFile;
use crate::sender::MultiSender;
use crate::tx_builder::TxBuilder;

pub struct Executor {
    _config: ExecutorConfigFile,
    tx_builder: TxBuilder,
    multi_sender: MultiSender,
    opp_rx: mpsc::Receiver<Opportunity>,
    payer: Arc<Keypair>,
    rpc: Arc<RpcClient>,
}

impl Executor {
    pub fn new(
        config: ExecutorConfigFile,
        opp_rx: mpsc::Receiver<Opportunity>,
        payer: Keypair,
        rpc_url: &str,
    ) -> Self {
        let tx_builder = TxBuilder::from_config(&config);
        let multi_sender = MultiSender::from_config(&config);
        let rpc = Arc::new(RpcClient::new(rpc_url.to_string()));

        Self {
            _config: config,
            tx_builder,
            multi_sender,
            opp_rx,
            payer: Arc::new(payer),
            rpc,
        }
    }

    pub async fn run(mut self) {
        info!("Executor started. Waiting for opportunities...");

        let mut latest_slot: u64 = 0;

        let mut recent_blockhash = self
            .rpc
            .get_latest_blockhash()
            .await
            .expect("Failed to get initial blockhash");
        let mut blockhash_age: u64 = 0;

        loop {
            let opp = match self.opp_rx.recv().await {
                Some(opp) => opp,
                None => {
                    info!("Opportunity channel closed. Shutting down.");
                    break;
                }
            };

            if opp.slot > latest_slot {
                latest_slot = opp.slot;
            }

            // Staleness check: > 2 slots -> discard
            if latest_slot > opp.slot && latest_slot - opp.slot > 2 {
                debug!("Discarding stale opportunity (slot {} vs latest {})", opp.slot, latest_slot);
                continue;
            }

            // Refresh blockhash every ~50 opportunities
            blockhash_age += 1;
            if blockhash_age > 50 {
                match self.rpc.get_latest_blockhash().await {
                    Ok(bh) => {
                        recent_blockhash = bh;
                        blockhash_age = 0;
                    }
                    Err(e) => {
                        warn!("Failed to refresh blockhash: {}", e);
                    }
                }
            }

            let pair = self.tx_builder.build(&opp, &self.payer, recent_blockhash);

            debug!(
                "Opportunity: {} hops, profit={} lamports, submitting...",
                opp.route.hops.len(),
                opp.expected_profit,
            );

            self.multi_sender.send_all(&pair).await;
        }
    }
}
