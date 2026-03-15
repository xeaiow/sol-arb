//! Startup bootstrap: getProgramAccounts to bulk-load pool data for coverage.
//!
//! gRPC only pushes pools with active trades. This module fills the gap by
//! fetching all SOL-paired pools at startup, so the engine can discover
//! arbitrage routes through tokens that have pools on 2+ DEXes.
//!
//! After pool registration, vault balances are batch-fetched via the private
//! RPC node to populate reserves (otherwise pools stay at reserve=0 and can't
//! be scanned for arbitrage).

use std::collections::HashSet;
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
use crate::pool::state::{DexType, PoolState};
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
        GpaQuery {
            name: "Meteora DLMM",
            program_id: pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"),
            dex_type: DexType::MeteoraDlmm,
            filters: vec![RpcFilterType::DataSize(904)],
        },
        GpaQuery {
            name: "Orca Whirlpool",
            program_id: pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"),
            dex_type: DexType::OrcaWhirlpool,
            filters: vec![RpcFilterType::DataSize(653)],
        },
        GpaQuery {
            name: "Raydium CLMM (SOL=mint0)",
            program_id: pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"),
            dex_type: DexType::RaydiumClmm,
            filters: vec![
                RpcFilterType::DataSize(1544),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(73, wsol.clone())),
            ],
        },
        GpaQuery {
            name: "Raydium CLMM (SOL=mint1)",
            program_id: pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"),
            dex_type: DexType::RaydiumClmm,
            filters: vec![
                RpcFilterType::DataSize(1544),
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(105, wsol.clone())),
            ],
        },
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
fn decode_pool(dex_type: DexType, address: &Pubkey, data: &[u8]) -> Option<PoolState> {
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
/// Phase 1: Fetch all SOL-paired pools from DLMM, Whirlpool, CLMM, and CPMM.
/// Phase 2: Batch fetch vault balances for all registered pools.
pub async fn bootstrap_pools(streamer: &Arc<PoolStreamer>, gpa_rpc_url: &str) {
    let gpa_rpc = RpcClient::new_with_commitment(
        gpa_rpc_url.to_string(),
        CommitmentConfig::confirmed(),
    );

    // ── Phase 1: Fetch and register pools ──
    let queries = build_queries();
    let total_start = Instant::now();
    let mut total_registered = 0usize;
    let mut total_fetched = 0usize;
    let mut all_vaults: Vec<(Pubkey, bool)> = Vec::new(); // (vault_pubkey, is_vault_a)

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

        match gpa_rpc.get_program_accounts_with_config(&query.program_id, config).await {
            Ok(accounts) => {
                let fetched = accounts.len();
                total_fetched += fetched;
                let mut decoded = 0usize;

                for (pubkey, account) in &accounts {
                    if let Some(pool_state) = decode_pool(query.dex_type, pubkey, &account.data) {
                        // Collect vaults before registering (registration doesn't queue vaults)
                        if let Some(va) = pool_state.vault_a {
                            all_vaults.push((va, true));
                        }
                        if let Some(vb) = pool_state.vault_b {
                            all_vaults.push((vb, false));
                        }
                        streamer.register_pool_lightweight(pool_state).await;
                        decoded += 1;
                    }
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

        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    info!(
        "[GPA] Phase 1 complete: {} pools registered ({} fetched) in {:.1}s. Fetching vault balances...",
        total_registered, total_fetched, total_start.elapsed().as_secs_f64(),
    );

    // ── Phase 2: Batch fetch vault balances via private RPC ──
    // Dedup vaults (many pools share the same WSOL vault)
    let mut seen = HashSet::new();
    let unique_vaults: Vec<(Pubkey, bool)> = all_vaults
        .into_iter()
        .filter(|(pubkey, _)| seen.insert(*pubkey))
        .collect();

    info!("[GPA] Fetching {} unique vault balances...", unique_vaults.len());

    // Get current slot for vault balance timestamps
    let current_slot = streamer.rpc().get_slot().await.unwrap_or(1);

    // Use streamer's private RPC (not Helius) for vault fetches
    let vault_start = Instant::now();
    let mut fetched_count = 0usize;
    let mut failed_count = 0usize;

    for chunk in unique_vaults.chunks(50) {
        let pubkeys: Vec<Pubkey> = chunk.iter().map(|(pk, _)| *pk).collect();
        match streamer.rpc().get_multiple_accounts(&pubkeys).await {
            Ok(accounts) => {
                for (i, maybe_account) in accounts.iter().enumerate() {
                    if let Some(account) = maybe_account {
                        if account.data.len() >= 72 {
                            let balance = u64::from_le_bytes(
                                account.data[64..72].try_into().unwrap_or([0; 8]),
                            );
                            streamer.cache().update_vault_balance(
                                &chunk[i].0,
                                balance,
                                chunk[i].1,
                                current_slot,
                            );
                            fetched_count += 1;
                        }
                    }
                }
            }
            Err(e) => {
                failed_count += chunk.len();
                if failed_count <= 100 {
                    warn!("[GPA] Vault fetch batch failed: {}", e);
                }
            }
        }

        // Yield between batches to not block other tasks
        tokio::task::yield_now().await;
    }

    // Notify subscription updater
    streamer.subscription_dirty().store(true, std::sync::atomic::Ordering::Release);
    streamer.subscription_notify().notify_waiters();

    info!(
        "[GPA] Bootstrap complete: {} pools, {} vault balances fetched ({} failed) in {:.1}s",
        total_registered, fetched_count, failed_count, total_start.elapsed().as_secs_f64(),
    );
}
