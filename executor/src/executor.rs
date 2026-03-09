use std::sync::Arc;
use log::{info, debug, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signer::{keypair::Keypair, Signer};
use tokio::sync::mpsc;

use arb_engine::opportunity::Opportunity;

use crate::alt::Tier0Alt;
use crate::config::ExecutorConfigFile;
use crate::marginfi::MarginFiState;
use crate::sender::MultiSender;
use crate::tx_builder::TxBuilder;

pub struct Executor {
    _config: ExecutorConfigFile,
    tx_builder: TxBuilder,
    multi_sender: MultiSender,
    opp_rx: mpsc::Receiver<Opportunity>,
    payer: Arc<Keypair>,
    rpc: Arc<RpcClient>,
    _marginfi_state: Option<Arc<MarginFiState>>,
}

impl Executor {
    pub async fn new(
        config: ExecutorConfigFile,
        opp_rx: mpsc::Receiver<Opportunity>,
        payer: Keypair,
        rpc_url: &str,
    ) -> anyhow::Result<Self> {
        let rpc = Arc::new(RpcClient::new(rpc_url.to_string()));
        let payer_pubkey = payer.pubkey();

        // Pre-initialize TxBuilder
        let mut tx_builder = TxBuilder::from_config(&config, payer_pubkey);

        // Pre-load ALT
        let alt_address = config.executor.alt_address.parse()?;
        match Tier0Alt::load(&rpc, alt_address).await {
            Ok(alt) => {
                tx_builder.set_alt(Arc::new(alt));
            }
            Err(e) => {
                warn!("Failed to load ALT {}: {} — running without ALT", alt_address, e);
            }
        }

        // Pre-connect all senders (Jito gRPC, reqwest clients)
        let multi_sender = MultiSender::from_config(&config).await;

        // Pre-initialize MarginFi state (if flashloan enabled)
        let marginfi_state = if config.executor.flashloan_enabled {
            match MarginFiState::init(&rpc, &payer_pubkey).await {
                Ok(state) => Some(Arc::new(state)),
                Err(e) => {
                    warn!("Failed to init MarginFi: {} — flashloan disabled", e);
                    None
                }
            }
        } else {
            None
        };

        info!("Executor fully initialized. All connections pre-established.");

        Ok(Self {
            _config: config,
            tx_builder,
            multi_sender,
            opp_rx,
            payer: Arc::new(payer),
            rpc,
            _marginfi_state: marginfi_state,
        })
    }

    pub async fn run(mut self) {
        info!("Executor started. Waiting for opportunities...");

        let mut latest_slot: u64 = 0;

        let mut recent_blockhash = self
            .rpc
            .get_latest_blockhash()
            .await
            .expect("Failed to get initial blockhash");
        let mut blockhash_slot: u64 = 0;
        // Blockhash is valid for ~150 slots (~60s). Refresh every 100 slots for safety.
        const BLOCKHASH_REFRESH_SLOTS: u64 = 100;

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

            // Refresh blockhash based on slot distance (not counter)
            if latest_slot.saturating_sub(blockhash_slot) >= BLOCKHASH_REFRESH_SLOTS {
                match self.rpc.get_latest_blockhash().await {
                    Ok(bh) => {
                        recent_blockhash = bh;
                        blockhash_slot = latest_slot;
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
