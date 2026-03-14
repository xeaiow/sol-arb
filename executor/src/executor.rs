use std::sync::Arc;
use log::{info, debug, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::hash::Hash;
use solana_sdk::signer::{keypair::Keypair, Signer};
use tokio::sync::mpsc;

use arb_engine::opportunity::Opportunity;

use crate::alt::Tier0Alt;
use crate::config::ExecutorConfigFile;
use crate::marginfi::MarginFiState;
use crate::sender::MultiSender;
use crate::tx_builder::TxBuilder;

/// Shared blockhash provider — fed by gRPC BlockMeta events.
pub type SharedBlockhash = Arc<tokio::sync::RwLock<Option<Hash>>>;

pub struct Executor {
    _config: ExecutorConfigFile,
    tx_builder: TxBuilder,
    multi_sender: MultiSender,
    opp_rx: mpsc::Receiver<Opportunity>,
    payer: Arc<Keypair>,
    rpc: Arc<RpcClient>,
    /// gRPC-pushed blockhash (primary). Falls back to RPC if not yet available.
    shared_blockhash: Option<SharedBlockhash>,
    #[allow(dead_code)] // Held to keep Arc alive; TxBuilder has a clone
    marginfi_state: Option<Arc<MarginFiState>>,
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

        // Test mode: skip on-chain profit verification (set TEST_MODE=1)
        if std::env::var("TEST_MODE").map_or(false, |v| v == "1") {
            tx_builder.test_mode = true;
            warn!("⚠️ TEST MODE ENABLED — profit verification disabled!");
        }

        // Pre-load ALT
        info!("Loading ALT...");
        let alt_address = config.executor.alt_address.parse()?;
        match Tier0Alt::load(&rpc, alt_address).await {
            Ok(alt) => {
                tx_builder.set_alt(Arc::new(alt));
            }
            Err(e) => {
                warn!("Failed to load ALT {}: {} — running without ALT", alt_address, e);
            }
        }

        // Pre-connect all senders (Jito gRPC, reqwest clients) — non-blocking
        info!("Connecting to senders...");
        let multi_sender = MultiSender::from_config(&config).await;

        // Pre-initialize MarginFi state (if flashloan enabled)
        let marginfi_state = if config.executor.flashloan_enabled {
            let marginfi_rpc = config.executor.fallback_rpc_url.as_ref()
                .map(|url| Arc::new(RpcClient::new(url.clone())))
                .unwrap_or_else(|| rpc.clone());
            let state = MarginFiState::init(&marginfi_rpc, &payer_pubkey)
                .await
                .map_err(|e| anyhow::anyhow!(
                    "flashloan_enabled=true but MarginFi init failed: {}. \
                     Set flashloan_enabled=false or fix the issue.", e
                ))?;
            let state = Arc::new(state);
            tx_builder.set_marginfi(state.clone());
            Some(state)
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
            shared_blockhash: None,
            marginfi_state: marginfi_state,
        })
    }

    /// Set a shared blockhash from gRPC BlockMeta events.
    /// When set, executor uses this instead of RPC polling.
    pub fn set_shared_blockhash(&mut self, bh: SharedBlockhash) {
        self.shared_blockhash = Some(bh);
    }

    pub async fn run(mut self) {
        info!("Executor started. Waiting for opportunities...");

        let mut latest_slot: u64 = 0;

        // Try gRPC blockhash first, fall back to RPC
        let mut recent_blockhash = if let Some(ref sbh) = self.shared_blockhash {
            let bh = sbh.read().await;
            match *bh {
                Some(h) => h,
                None => {
                    info!("gRPC blockhash not yet available, falling back to RPC");
                    self.rpc.get_latest_blockhash().await
                        .expect("Failed to get initial blockhash")
                }
            }
        } else {
            self.rpc.get_latest_blockhash().await
                .expect("Failed to get initial blockhash")
        };
        let mut blockhash_slot: u64 = 0;

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

            // Update blockhash: try gRPC first, fallback to RPC
            let got_grpc_blockhash = if let Some(ref sbh) = self.shared_blockhash {
                if let Ok(bh) = sbh.try_read() {
                    if let Some(h) = *bh {
                        recent_blockhash = h;
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if !got_grpc_blockhash {
                const BLOCKHASH_REFRESH_SLOTS: u64 = 50;
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
            }

            let t_start = std::time::Instant::now();
            let pair = self.tx_builder.build(&opp, &self.payer, recent_blockhash);

            // Log transaction signatures for on-chain lookup
            if let Some(ref tx) = pair.jito_tx {
                info!("Jito tx sig: {}", tx.signatures[0]);
            }
            if let Some(ref tx) = pair.swqos_tx {
                info!("SwQoS tx sig: {}", tx.signatures[0]);
            }

            {
                let path: Vec<String> = opp.pool_snapshots.iter().map(|s| {
                    let dir = if s.is_a_to_b { "→" } else { "←" };
                    format!("{:?}({:.4}..{}{})", s.dex_type,
                        &s.address.to_string()[..6], dir,
                        &s.mint_b.to_string()[..6])
                }).collect();
                info!(
                    "💰 Opp: {} hops, in={:.6} SOL, profit={:.6} SOL, slot={} | {}",
                    opp.route.hops.len(),
                    opp.amount_in as f64 / 1e9,
                    opp.expected_profit as f64 / 1e9,
                    opp.slot,
                    path.join(" → "),
                );
            }

            // Skip if no tx was built (e.g. cross-hop conflict)
            if pair.jito_tx.is_none() && pair.swqos_tx.is_none() {
                continue;
            }

            // TX size check — Solana limit is 1232 bytes
            {
                let check_tx = pair.swqos_tx.as_ref().or(pair.jito_tx.as_ref());
                if let Some(tx) = check_tx {
                    if let Ok(bytes) = bincode::serialize(tx) {
                        if bytes.len() > 1232 {
                            warn!(
                                "[TX_SIZE] {} bytes > 1232 limit | engine_profit={:.6} SOL | {} hops → skipped",
                                bytes.len(),
                                opp.expected_profit as f64 / 1e9,
                                opp.route.hops.len(),
                            );
                            continue;
                        }
                    }
                }
            }

            // No simulate — send directly. On-chain PROD MODE profit check
            // reverts unprofitable txs atomically (no actual loss).
            {
                let dex_types: Vec<String> = opp.pool_snapshots.iter()
                    .map(|s| format!("{:?}", s.dex_type)).collect();
                let slot_lag = latest_slot.saturating_sub(opp.slot);
                info!(
                    "[SEND] engine_profit={:.6} SOL | {} hops slot={} lag={} dexes=[{}]",
                    opp.expected_profit as f64 / 1e9,
                    opp.route.hops.len(),
                    opp.slot,
                    slot_lag,
                    dex_types.join(","),
                );
            }

            // Test mode: send and wait, then exit
            if self.tx_builder.test_mode {
                let build_us = t_start.elapsed().as_micros();
                let sender = self.multi_sender.clone();
                let t_send = std::time::Instant::now();
                sender.send_all(&pair).await;
                let send_us = t_send.elapsed().as_micros();
                info!("⏱️ build={}µs, send={}µs, total={}µs", build_us, send_us, build_us + send_us);
                info!("Test mode: first opportunity sent, exiting.");
                break;
            }

            // Fire-and-forget: don't block the loop waiting for network responses
            let sender = self.multi_sender.clone();
            let build_us = t_start.elapsed().as_micros();
            tokio::spawn(async move {
                let t_send = std::time::Instant::now();
                sender.send_all(&pair).await;
                let send_us = t_send.elapsed().as_micros();
                info!("⏱️ build={}µs, send={}µs, total={}µs", build_us, send_us, build_us + send_us);
            });
        }
    }
}
