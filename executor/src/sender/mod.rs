pub mod jito;
pub mod flashblock;
pub mod astralane;

use log::{info, warn};
use tokio::task::JoinHandle;

use crate::config::ExecutorConfigFile;
use crate::tx_builder::TxPair;

pub struct MultiSender {
    jito_senders: Vec<jito::JitoSender>,
    flashblock_senders: Vec<flashblock::FlashblockSender>,
    astralane_senders: Vec<astralane::AstralaneSender>,
}

impl MultiSender {
    pub fn from_config(config: &ExecutorConfigFile) -> Self {
        let mut jito_senders = Vec::new();
        if let Some(jito_cfg) = &config.jito {
            if jito_cfg.enabled {
                for url in &jito_cfg.block_engine_urls {
                    jito_senders.push(jito::JitoSender::new(url.clone()));
                }
            }
        }

        let mut flashblock_senders = Vec::new();
        if let Some(fb_cfg) = &config.flashblock {
            if fb_cfg.enabled {
                for endpoint in &fb_cfg.endpoints {
                    flashblock_senders.push(flashblock::FlashblockSender::new(
                        endpoint.clone(),
                        fb_cfg.api_key.clone(),
                    ));
                }
            }
        }

        let mut astralane_senders = Vec::new();
        if let Some(ast_cfg) = &config.astralane {
            if ast_cfg.enabled {
                for endpoint in &ast_cfg.endpoints {
                    astralane_senders.push(astralane::AstralaneSender::new(
                        endpoint.clone(),
                        ast_cfg.api_key.clone(),
                    ));
                }
            }
        }

        info!(
            "MultiSender initialized: {} Jito, {} Flashblock, {} Astralane endpoints",
            jito_senders.len(),
            flashblock_senders.len(),
            astralane_senders.len(),
        );

        Self {
            jito_senders,
            flashblock_senders,
            astralane_senders,
        }
    }

    /// Send both tx variants to all enabled channels concurrently
    pub async fn send_all(&self, pair: &TxPair) {
        let mut handles: Vec<JoinHandle<()>> = Vec::new();

        if let Some(ref tx) = pair.jito_tx {
            for sender in &self.jito_senders {
                let sender = sender.clone();
                let tx = tx.clone();
                handles.push(tokio::spawn(async move {
                    if let Err(e) = sender.send_bundle(&tx).await {
                        warn!("Jito send failed: {}", e);
                    }
                }));
            }
        }

        if let Some(ref tx) = pair.swqos_tx {
            for sender in &self.flashblock_senders {
                let sender = sender.clone();
                let tx = tx.clone();
                handles.push(tokio::spawn(async move {
                    if let Err(e) = sender.send_transaction(&tx).await {
                        warn!("Flashblock send failed: {}", e);
                    }
                }));
            }
        }

        if let Some(ref tx) = pair.swqos_tx {
            for sender in &self.astralane_senders {
                let sender = sender.clone();
                let tx = tx.clone();
                handles.push(tokio::spawn(async move {
                    if let Err(e) = sender.send_transaction(&tx).await {
                        warn!("Astralane send failed: {}", e);
                    }
                }));
            }
        }

        for handle in handles {
            let _ = handle.await;
        }
    }
}
