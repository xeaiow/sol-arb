//! Startup bootstrap: getProgramAccounts to bulk-load pool data for coverage.
//!
//! gRPC only pushes pools with active trades. This module fills the gap by
//! fetching all SOL-paired pools at startup, so the engine can discover
//! arbitrage routes through tokens that have pools on 2+ DEXes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use log::{info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcProgramAccountsConfig, RpcAccountInfoConfig};
use solana_client::rpc_filter::{RpcFilterType, Memcmp};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

use crate::pool::decoder;
use crate::pool::state::DexType;
use crate::pool::streamer::PoolStreamer;

const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

struct GpaQuery {
    name: &'static str,
    program_id: Pubkey,
    dex_type: DexType,
    filters: Vec<RpcFilterType>,
}

fn build_queries() -> Vec<GpaQuery> {
    let wsol = WSOL.to_bytes().to_vec();

    vec![
        // Meteora DLMM — full scan (dataSize=904)
        GpaQuery {
            name: "Meteora DLMM",
            program_id: pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"),
            dex_type: DexType::MeteoraDlmm,
            filters: vec![RpcFilterType::DataSize(904)],
        },
        // Orca Whirlpool — full scan (dataSize=653)
        GpaQuery {
            name: "Orca Whirlpool",
            program_id: pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"),
            dex_type: DexType::OrcaWhirlpool,
            filters: vec![RpcFilterType::DataSize(653)],
        },
        // Raydium CLMM — SOL as mint0 (offset 73)
        GpaQuery {
            name: "Raydium CLMM (SOL=mint0)",
            program_id: pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"),
            dex_type: DexType::RaydiumClmm,
            filters: vec![
                RpcFilterType::DataSize(1544),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(73, wsol.clone())),
            ],
        },
        // Raydium CLMM — SOL as mint1 (offset 105)
        GpaQuery {
            name: "Raydium CLMM (SOL=mint1)",
            program_id: pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"),
            dex_type: DexType::RaydiumClmm,
            filters: vec![
                RpcFilterType::DataSize(1544),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(105, wsol.clone())),
            ],
        },
        // Raydium CPMM — SOL as token_1_mint (offset 200)
        GpaQuery {
            name: "Raydium CPMM (SOL=token1)",
            program_id: pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C"),
            dex_type: DexType::RaydiumCpmm,
            filters: vec![
                RpcFilterType::DataSize(637),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                    0, vec![0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46],
                )),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(200, wsol.clone())),
            ],
        },
        // Raydium CPMM — SOL as token_0_mint (offset 168)
        GpaQuery {
            name: "Raydium CPMM (SOL=token0)",
            program_id: pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C"),
            dex_type: DexType::RaydiumCpmm,
            filters: vec![
                RpcFilterType::DataSize(637),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                    0, vec![0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46],
                )),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(168, wsol.clone())),
            ],
        },
    ]
}

/// Decode raw account bytes into PoolState based on DEX type.
fn decode_pool(dex_type: DexType, address: &Pubkey, data: &[u8]) -> Option<crate::pool::state::PoolState> {
    match dex_type {
        DexType::MeteoraDlmm => decoder::meteora_dlmm::decode_bytes(address, data),
        DexType::OrcaWhirlpool => decoder::orca_whirlpool::decode_bytes(address, data),
        DexType::RaydiumClmm => decoder::raydium_clmm::decode_bytes(address, data),
        DexType::RaydiumCpmm => decoder::raydium_cpmm::decode_bytes(address, data),
        _ => None,
    }
}

/// Bootstrap pools from getProgramAccounts at startup.
///
/// Fetches all SOL-paired pools from DLMM, Whirlpool, CLMM, and CPMM
/// via Helius gPA, decodes them, and registers them in the streamer's cache
/// using lightweight registration (no tick array / bin array / fee fetch).
pub async fn bootstrap_pools(streamer: &Arc<PoolStreamer>, gpa_rpc_url: &str) {
    let rpc = RpcClient::new_with_commitment(
        gpa_rpc_url.to_string(),
        CommitmentConfig::confirmed(),
    );

    let queries = build_queries();
    let total_start = Instant::now();
    let mut total_registered = 0usize;
    let mut total_fetched = 0usize;

    for query in &queries {
        let start = Instant::now();

        let config = RpcProgramAccountsConfig {
            filters: Some(query.filters.clone()),
            account_config: RpcAccountInfoConfig {
                encoding: Some(solana_account_decoder::UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                ..Default::default()
            },
            ..Default::default()
        };

        match rpc.get_program_accounts_with_config(&query.program_id, config).await {
            Ok(accounts) => {
                let fetched = accounts.len();
                total_fetched += fetched;
                let mut decoded = 0usize;

                for (pubkey, account) in &accounts {
                    if let Some(pool_state) = decode_pool(query.dex_type, pubkey, &account.data) {
                        streamer.register_pool_lightweight(pool_state).await;
                        decoded += 1;
                    }
                    // Yield every 100 pools to let other tasks run
                    if decoded % 100 == 0 && decoded > 0 {
                        tokio::task::yield_now().await;
                    }
                }

                info!(
                    "[GPA] {}: fetched={}, decoded={}, elapsed={:.1}s",
                    query.name, fetched, decoded, start.elapsed().as_secs_f64(),
                );
                total_registered += decoded;
            }
            Err(e) => {
                warn!("[GPA] {} failed ({:.1}s): {}", query.name, start.elapsed().as_secs_f64(), e);
            }
        }

        // Pause between queries to avoid Helius rate limits
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Notify vault fetcher and subscription updater
    streamer.subscription_dirty().store(true, std::sync::atomic::Ordering::Release);
    streamer.subscription_notify().notify_waiters();
    streamer.vault_notify().notify_one();

    info!(
        "[GPA] Bootstrap complete: {} pools registered ({} fetched) in {:.1}s",
        total_registered, total_fetched, total_start.elapsed().as_secs_f64(),
    );
}
