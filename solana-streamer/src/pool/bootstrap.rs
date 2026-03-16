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
use crate::pool::state::{DexType, PoolMath, PoolState};
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
        // PumpSwap: 5M+ pools, too large for gPA. Covered by gRPC event-driven discovery.
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
        DexType::PumpSwap => decoder::pumpswap::decode_bytes(address, data),
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

    info!(
        "[GPA] Phase 2 complete: {} vault balances fetched ({} failed) in {:.1}s",
        fetched_count, failed_count, vault_start.elapsed().as_secs_f64(),
    );

    // ── Phase 3: Fetch bin/tick arrays for cross-DEX pools ──
    // Only fetch for pools whose non-SOL mint also appears in a different DEX.
    // This enables PumpSwap+DLMM, CPMM+CLMM, etc. arbitrage routes.
    let phase3_start = Instant::now();
    let mut bin_tick_fetched = 0usize;

    // Build mint → dex_type set map from all cached pools
    let mut mint_dex_map: std::collections::HashMap<Pubkey, HashSet<DexType>> =
        std::collections::HashMap::new();
    let all_pools: Vec<(Pubkey, Pubkey, Pubkey, DexType)> = {
        let cache = streamer.cache();
        cache.all_pool_addresses().iter().filter_map(|addr| {
            cache.get(addr).map(|p| (p.address, p.mint_a, p.mint_b, p.dex_type))
        }).collect()
    };
    for &(_, mint_a, mint_b, dex_type) in &all_pools {
        mint_dex_map.entry(mint_a).or_default().insert(dex_type);
        mint_dex_map.entry(mint_b).or_default().insert(dex_type);
    }

    // Find mints with 2+ DEX types (arbitrage candidates)
    let multi_dex_mints: HashSet<Pubkey> = mint_dex_map.iter()
        .filter(|(_, dex_types)| dex_types.len() >= 2)
        .map(|(mint, _)| *mint)
        .collect();

    info!("[GPA] Phase 3: {} mints with 2+ DEX coverage, fetching bin/tick arrays...",
        multi_dex_mints.len());

    // Collect DLMM pools that need bin arrays
    let dlmm_pools: Vec<(Pubkey, i32)> = {
        let cache = streamer.cache();
        all_pools.iter().filter_map(|&(addr, mint_a, mint_b, dex_type)| {
            if dex_type != DexType::MeteoraDlmm {
                return None;
            }
            // Only if one of the mints has multi-DEX coverage
            if !multi_dex_mints.contains(&mint_a) && !multi_dex_mints.contains(&mint_b) {
                return None;
            }
            // Get active_id from pool math
            cache.get(&addr).and_then(|p| {
                if let PoolMath::MeteoraDlmm { active_id, bin_arrays, .. } = &p.math {
                    if bin_arrays.is_empty() {
                        Some((addr, *active_id))
                    } else {
                        None // already has bin arrays
                    }
                } else {
                    None
                }
            })
        }).collect()
    };

    info!("[GPA] Fetching bin arrays for {} DLMM pools...", dlmm_pools.len());

    // Batch fetch bin arrays: 3 PDAs per pool, use getMultipleAccounts
    for chunk in dlmm_pools.chunks(16) {
        // Collect all PDAs for this batch (up to 48, but getMultipleAccounts limit is 50)
        let mut pda_requests: Vec<(Pubkey, Pubkey, i64)> = Vec::new(); // (pda, pool_addr, bin_array_index)
        for &(pool_addr, active_id) in chunk {
            let pdas = decoder::meteora_dlmm::bin_array_pdas_for_swap(&pool_addr, active_id);
            let idx = decoder::meteora_dlmm::bin_id_to_bin_array_index(active_id);
            for (i, pda) in pdas.iter().enumerate() {
                pda_requests.push((*pda, pool_addr, idx as i64 - 1 + i as i64));
            }
        }

        let pubkeys: Vec<Pubkey> = pda_requests.iter().map(|(pda, _, _)| *pda).collect();
        match streamer.rpc().get_multiple_accounts(&pubkeys).await {
            Ok(accounts) => {
                // Group results by pool
                let mut pool_bin_arrays: std::collections::HashMap<Pubkey, Vec<crate::pool::state::DlmmBinArray>> =
                    std::collections::HashMap::new();
                let mut pool_pdas: std::collections::HashMap<Pubkey, Vec<Pubkey>> =
                    std::collections::HashMap::new();

                for (i, maybe_account) in accounts.iter().enumerate() {
                    if let Some(account) = maybe_account {
                        let (pda, pool_addr, _) = pda_requests[i];
                        if let Some(ba) = decoder::meteora_dlmm::decode_bin_array(&account.data) {
                            pool_bin_arrays.entry(pool_addr).or_default().push(ba);
                            pool_pdas.entry(pool_addr).or_default().push(pda);
                            streamer.cache().register_tick_array(pda, pool_addr);
                            bin_tick_fetched += 1;
                        }
                    }
                }

                // Update each pool's bin_arrays and extra_accounts
                for (pool_addr, bin_arrays) in pool_bin_arrays {
                    let pdas = pool_pdas.get(&pool_addr).cloned().unwrap_or_default();
                    if let Some(mut pool) = streamer.cache().get_mut(&pool_addr) {
                        if let PoolMath::MeteoraDlmm { bin_arrays: ref mut ba, .. } = pool.math {
                            *ba = bin_arrays;
                        }
                        // Update extra_accounts: [oracle, bin_pda1, bin_pda2, ...]
                        if !pool.extra_accounts.is_empty() {
                            let oracle = pool.extra_accounts[0];
                            let mut new_extra = vec![oracle];
                            new_extra.extend(pdas);
                            pool.extra_accounts = new_extra;
                        }
                    }
                }
            }
            Err(e) => {
                if bin_tick_fetched == 0 {
                    warn!("[GPA] Bin array batch failed: {}", e);
                }
            }
        }

        tokio::task::yield_now().await;
    }

    // Notify subscription updater
    streamer.subscription_dirty().store(true, std::sync::atomic::Ordering::Release);
    streamer.subscription_notify().notify_waiters();

    // ── Phase 4: Re-emit all pools with reserves to engine ──
    // During bootstrap, vault balance updates may have been dropped or not yet
    // consumed by the engine. Re-emit all pools with non-zero reserves so the
    // engine graph has up-to-date data for full_scan.
    let cache = streamer.cache();
    let all_addrs = cache.all_pool_addresses();
    let mut reemit_count = 0usize;
    for addr in &all_addrs {
        if let Some(pool) = cache.get(addr) {
            // Only re-emit pools that have useful data (reserves or bin/tick arrays)
            let has_data = match &pool.math {
                PoolMath::ConstantProduct { reserve_a, reserve_b, .. } => {
                    *reserve_a > 0 || *reserve_b > 0
                }
                PoolMath::Concentrated { tick_arrays, .. } => !tick_arrays.is_empty(),
                PoolMath::DammV2Concentrated { liquidity, .. } => *liquidity > 0,
                PoolMath::MeteoraDlmm { bin_arrays, .. } => !bin_arrays.is_empty(),
                PoolMath::BondingCurve { .. } => true,
            };
            if has_data {
                cache.re_emit_pool(&pool);
                reemit_count += 1;
            }
        }
        if reemit_count % 10_000 == 0 && reemit_count > 0 {
            tokio::task::yield_now().await;
        }
    }
    info!(
        "[GPA] Phase 4: re-emitted {} pools with data to engine (of {} total)",
        reemit_count, all_addrs.len(),
    );

    info!(
        "[GPA] Bootstrap complete: {} pools, {} vaults, {} bin/tick arrays in {:.1}s",
        total_registered, fetched_count, bin_tick_fetched, total_start.elapsed().as_secs_f64(),
    );
}
