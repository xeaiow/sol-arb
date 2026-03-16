//! Cross-DEX price comparison test tool.
//!
//! 1. Bootstrap: build mint → Vec<pool> index from gPA
//! 2. gRPC: listen for pool updates
//! 3. On update: find same-token pools on other DEXes, RPC fetch fresh data, compare prices
//! 4. Log price differences as a table
//!
//! Usage: cd executor && RUST_LOG=info cargo run --release --bin test_cross_dex_prices

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use log::{info, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::RwLock;

use solana_streamer_sdk::pool::cache::PoolStateCache;
use solana_streamer_sdk::pool::decoder;
use solana_streamer_sdk::pool::state::{DexType, PoolMath, PoolState, PoolUpdate};
use solana_streamer_sdk::pool::streamer::{PoolStreamer, PoolStreamerConfig};
use solana_streamer_sdk::streaming::event_parser::protocols::{
    meteora_dlmm::parser::METEORA_DLMM_PROGRAM_ID,
    orca_whirlpool::parser::ORCA_WHIRLPOOL_PROGRAM_ID,
    pumpfun::parser::PUMPFUN_PROGRAM_ID,
    pumpswap::parser::PUMPSWAP_PROGRAM_ID,
    raydium_amm_v4::parser::RAYDIUM_AMM_V4_PROGRAM_ID,
    raydium_clmm::parser::RAYDIUM_CLMM_PROGRAM_ID,
    raydium_cpmm::parser::RAYDIUM_CPMM_PROGRAM_ID,
    meteora_damm_v2::parser::METEORA_DAMM_V2_PROGRAM_ID,
    bonk::parser::BONK_PROGRAM_ID,
};
use solana_streamer_sdk::streaming::event_parser::Protocol;
use solana_streamer_sdk::streaming::yellowstone_grpc::{
    AccountFilter, TransactionFilter, YellowstoneGrpc,
};

const WSOL: &str = "So11111111111111111111111111111111111111112";

/// Mint → Vec<(pool_address, dex_type)> index
type MintIndex = Arc<RwLock<HashMap<Pubkey, Vec<(Pubkey, DexType)>>>>;

/// Build mint index from cache
async fn build_mint_index(cache: &Arc<PoolStateCache>) -> HashMap<Pubkey, Vec<(Pubkey, DexType)>> {
    let mut index: HashMap<Pubkey, Vec<(Pubkey, DexType)>> = HashMap::new();
    let wsol: Pubkey = WSOL.parse().unwrap();

    for addr in cache.all_pool_addresses() {
        if let Some(pool) = cache.get(&addr) {
            // Index by non-SOL mint (the token being traded)
            let token_mint = if pool.mint_a == wsol {
                pool.mint_b
            } else if pool.mint_b == wsol {
                pool.mint_a
            } else {
                continue; // Skip non-SOL pairs for now
            };
            index.entry(token_mint).or_default().push((pool.address, pool.dex_type));
        }
    }
    index
}

const MIN_SOL_RESERVE: u64 = 5_000_000_000; // 5 SOL minimum liquidity

/// Get SOL reserve of a pool (vault balance for the SOL side)
fn get_sol_reserve(pool: &PoolState) -> u64 {
    let wsol: Pubkey = WSOL.parse().unwrap();
    let sol_is_a = pool.mint_a == wsol;
    match &pool.math {
        PoolMath::ConstantProduct { reserve_a, reserve_b, .. } => {
            if sol_is_a { *reserve_a } else { *reserve_b }
        }
        // For non-CP pools, we can't easily get SOL reserve from math alone.
        // Return u64::MAX to skip filtering (they have other quality checks).
        _ => u64::MAX,
    }
}

/// Get price from a pool: how many tokens per 1 SOL
fn get_price_tokens_per_sol(pool: &PoolState) -> Option<f64> {
    let wsol: Pubkey = WSOL.parse().unwrap();
    let sol_is_a = pool.mint_a == wsol;
    if !sol_is_a && pool.mint_b != wsol {
        return None;
    }

    // Skip low-liquidity CP pools
    if get_sol_reserve(pool) < MIN_SOL_RESERVE {
        return None;
    }

    // Skip CLMM/Whirlpool without tick arrays (would produce garbage quotes)
    if let PoolMath::Concentrated { tick_arrays, fee_rate, .. } = &pool.math {
        if tick_arrays.is_empty() || *fee_rate == 0 {
            return None;
        }
    }

    let probe = 10_000_000u64; // 0.01 SOL to reduce slippage impact
    let out = pool.math.get_amount_out(probe, sol_is_a);
    if out == 0 {
        return None;
    }

    // Normalize to "tokens per 1 SOL"
    Some(out as f64 / probe as f64 * 1_000_000_000.0)
}

/// Fetch fresh pool data from RPC for accurate comparison.
/// Returns None if SOL vault balance < MIN_SOL_RESERVE.
async fn fetch_fresh_pool(rpc: &RpcClient, cache: &Arc<PoolStateCache>, pool_addr: &Pubkey, dex: DexType) -> Option<PoolState> {
    let mut pool = cache.get(pool_addr)?.clone();
    let wsol: Pubkey = WSOL.parse().unwrap();
    let sol_is_a = pool.mint_a == wsol;

    // Fetch SOL vault balance and check liquidity for ALL pool types
    let sol_vault = if sol_is_a { pool.vault_a } else { pool.vault_b };
    if let Some(vault) = sol_vault {
        if let Ok(data) = rpc.get_account_data(&vault).await {
            if data.len() >= 72 {
                let sol_bal = u64::from_le_bytes(data[64..72].try_into().unwrap_or([0; 8]));
                if sol_bal < MIN_SOL_RESERVE {
                    return None; // Skip low-liquidity pool
                }
            }
        }
    }

    // For CP pools: fetch live vault balances (both sides)
    if matches!(pool.math, PoolMath::ConstantProduct { .. }) {
        if let (Some(va), Some(vb)) = (pool.vault_a, pool.vault_b) {
            for (vault, is_a) in [(va, true), (vb, false)] {
                if let Ok(data) = rpc.get_account_data(&vault).await {
                    if data.len() >= 72 {
                        let bal = u64::from_le_bytes(data[64..72].try_into().ok()?);
                        if let PoolMath::ConstantProduct { ref mut reserve_a, ref mut reserve_b, .. } = pool.math {
                            if is_a { *reserve_a = bal; } else { *reserve_b = bal; }
                        }
                    }
                }
            }
        }
    }

    // For DLMM: fetch bin arrays if missing
    if dex == DexType::MeteoraDlmm {
        if let PoolMath::MeteoraDlmm { active_id, ref mut bin_arrays, .. } = pool.math {
            if bin_arrays.is_empty() {
                for pda in &decoder::meteora_dlmm::bin_array_pdas_for_swap(pool_addr, active_id) {
                    if let Ok(d) = rpc.get_account_data(pda).await {
                        if let Some(ba) = decoder::meteora_dlmm::decode_bin_array(&d) {
                            bin_arrays.push(ba);
                        }
                    }
                }
            }
        }
    }

    // For CLMM: fetch fee + tick arrays if missing
    if dex == DexType::RaydiumClmm {
        if let PoolMath::Concentrated { ref mut fee_rate, ref mut tick_arrays, tick_current, tick_spacing, .. } = pool.math {
            if *fee_rate == 0 {
                if let Some(ac) = pool.extra_accounts.first() {
                    if let Ok(d) = rpc.get_account_data(ac).await {
                        if let Some(c) = decoder::raydium_clmm::decode_amm_config(&d) {
                            *fee_rate = c.trade_fee_rate;
                        }
                    }
                }
            }
            if tick_arrays.is_empty() {
                for s in decoder::raydium_clmm::tick_array_start_indices(tick_current, tick_spacing) {
                    if let Some(pda) = decoder::raydium_clmm::tick_array_pda(pool_addr, s) {
                        if let Ok(d) = rpc.get_account_data(&pda).await {
                            if let Some(ta) = decoder::raydium_clmm::decode_tick_array(&d) {
                                tick_arrays.push(ta);
                            }
                        }
                    }
                }
            }
        }
    }

    // For Whirlpool: fetch tick arrays if missing
    if dex == DexType::OrcaWhirlpool {
        if let PoolMath::Concentrated { ref mut tick_arrays, tick_current, tick_spacing, .. } = pool.math {
            if tick_arrays.is_empty() {
                for s in decoder::orca_whirlpool::tick_array_start_indices(tick_current, tick_spacing) {
                    if let Some(pda) = decoder::orca_whirlpool::tick_array_pda(pool_addr, s) {
                        if let Ok(d) = rpc.get_account_data(&pda).await {
                            if let Some(ta) = decoder::orca_whirlpool::decode_tick_array(&d, tick_spacing) {
                                tick_arrays.push(ta);
                            }
                        }
                    }
                }
            }
        }
    }

    // For CPMM: fetch fee from AmmConfig
    if dex == DexType::RaydiumCpmm {
        if let Some(ac) = pool.extra_accounts.first() {
            if let Ok(d) = rpc.get_account_data(ac).await {
                use solana_streamer_sdk::streaming::event_parser::protocols::raydium_cpmm::types::{
                    amm_config_decode, AMM_CONFIG_SIZE,
                };
                if d.len() >= AMM_CONFIG_SIZE + 8 {
                    if let Some(c) = amm_config_decode(&d[8..AMM_CONFIG_SIZE + 8]) {
                        if let PoolMath::ConstantProduct { ref mut fee_numerator, ref mut fee_denominator, .. } = pool.math {
                            *fee_numerator = c.trade_fee_rate;
                            *fee_denominator = 1_000_000;
                        }
                    }
                }
            }
        }
    }

    Some(pool)
}

fn dex_name(dex: DexType) -> &'static str {
    match dex {
        DexType::PumpSwap => "PumpSwap",
        DexType::RaydiumCpmm => "CPMM",
        DexType::RaydiumAmmV4 => "AmmV4",
        DexType::RaydiumClmm => "CLMM",
        DexType::MeteoraDlmm => "DLMM",
        DexType::MeteoraDammV2 => "DammV2",
        DexType::OrcaWhirlpool => "Whirlpool",
        DexType::PumpFun => "PumpFun",
        DexType::Bonk => "BonkSwap",
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    env_logger::init();

    let config = arb_executor::config::ExecutorConfigFile::load("config.toml")?;
    let general = config.general.as_ref().unwrap();
    let rpc_url = general.rpc_url.clone().unwrap_or_default();
    let grpc_endpoint = general.grpc_endpoint.clone().unwrap_or_default();
    let grpc_token = general.grpc_token.clone().filter(|s| !s.is_empty());

    let rpc = Arc::new(RpcClient::new(rpc_url.clone()));

    // Stage 1: PoolStreamer + bootstrap
    let streamer_config = PoolStreamerConfig {
        rpc_url: rpc_url.clone(),
        ..Default::default()
    };
    let (pool_streamer, mut update_rx) = PoolStreamer::new(streamer_config);
    let pool_streamer = Arc::new(pool_streamer);

    eprintln!("Starting bootstrap...");

    // Start gRPC
    let grpc = Arc::new(YellowstoneGrpc::new(grpc_endpoint, grpc_token)?);
    let program_ids = vec![
        PUMPFUN_PROGRAM_ID.to_string(),
        PUMPSWAP_PROGRAM_ID.to_string(),
        BONK_PROGRAM_ID.to_string(),
        RAYDIUM_CPMM_PROGRAM_ID.to_string(),
        RAYDIUM_CLMM_PROGRAM_ID.to_string(),
        RAYDIUM_AMM_V4_PROGRAM_ID.to_string(),
        METEORA_DAMM_V2_PROGRAM_ID.to_string(),
        METEORA_DLMM_PROGRAM_ID.to_string(),
        ORCA_WHIRLPOOL_PROGRAM_ID.to_string(),
    ];

    let protocols = vec![
        Protocol::PumpFun, Protocol::PumpSwap, Protocol::Bonk,
        Protocol::RaydiumCpmm, Protocol::RaydiumClmm, Protocol::RaydiumAmmV4,
        Protocol::MeteoraDammV2, Protocol::MeteoraDlmm, Protocol::OrcaWhirlpool,
    ];

    let streamer_clone = pool_streamer.clone();
    grpc.subscribe_events_immediate(
        protocols, None,
        vec![TransactionFilter {
            account_include: program_ids.clone(),
            account_exclude: vec![], account_required: vec![],
        }],
        vec![AccountFilter {
            account: vec![], owner: program_ids.clone(), filters: vec![],
        }],
        None, None,
        move |event| {
            let s = streamer_clone.clone();
            tokio::spawn(async move { s.on_event(event).await; });
        },
    ).await?;

    // Bootstrap pools
    let gpa_url = config.engine.as_ref()
        .and_then(|e| e.gpa_rpc_url.clone())
        .or_else(|| config.executor.fallback_rpc_url.clone())
        .unwrap_or(rpc_url.clone());

    let bs_streamer = pool_streamer.clone();
    let bs_url = gpa_url.clone();
    tokio::spawn(async move {
        solana_streamer_sdk::pool::bootstrap::bootstrap_pools(&bs_streamer, &bs_url).await;
    });

    // Wait for bootstrap Phase 1 + Phase 2 (check every 5s)
    eprintln!("Waiting for bootstrap to complete...");
    let start = Instant::now();
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        let pool_count = pool_streamer.cache().all_pool_addresses().len();
        let elapsed = start.elapsed().as_secs();
        eprintln!("  {}s: {} pools in cache", elapsed, pool_count);
        if pool_count > 100_000 && elapsed > 30 {
            break; // Bootstrap Phase 1 done
        }
        if elapsed > 300 {
            eprintln!("  Timeout waiting for bootstrap");
            break;
        }
    }

    // Build mint index
    let mint_index_data = build_mint_index(&pool_streamer.cache()).await;
    let multi_dex_count = mint_index_data.values().filter(|v| v.len() >= 2).count();
    let total_mints = mint_index_data.len();
    eprintln!("\nMint index built: {} tokens, {} with 2+ DEX pools", total_mints, multi_dex_count);

    // Show top multi-DEX tokens
    let mut multi_dex: Vec<_> = mint_index_data.iter()
        .filter(|(_, pools)| pools.len() >= 2)
        .map(|(mint, pools)| {
            let dexes: Vec<&str> = pools.iter().map(|(_, d)| dex_name(*d)).collect();
            (mint, pools.len(), dexes)
        })
        .collect();
    multi_dex.sort_by(|a, b| b.1.cmp(&a.1));
    eprintln!("\nTop multi-DEX tokens:");
    for (mint, count, dexes) in multi_dex.iter().take(10) {
        eprintln!("  {}.. {} pools [{}]", &mint.to_string()[..8], count, dexes.join(", "));
    }

    let mint_index: MintIndex = Arc::new(RwLock::new(mint_index_data));

    // Stats
    let mut comparisons = 0u64;
    let mut arb_candidates = 0u64;
    let mut updates_received = 0u64;
    let mut updates_with_cross_dex = 0u64;
    let mut dex_pair_stats: HashMap<String, (u64, u64)> = HashMap::new();
    let run_start = Instant::now();

    eprintln!("\n=== Listening for gRPC updates, comparing cross-DEX prices ===\n");

    loop {
        let update = tokio::select! {
            Some(u) = update_rx.recv() => u,
            _ = tokio::signal::ctrl_c() => break,
        };

        updates_received += 1;

        let wsol: Pubkey = WSOL.parse().unwrap();
        let token_mint = if update.mint_a == wsol {
            update.mint_b
        } else if update.mint_b == wsol {
            update.mint_a
        } else {
            continue;
        };

        // Update mint index
        {
            let mut idx = mint_index.write().await;
            let entry = idx.entry(token_mint).or_default();
            if !entry.iter().any(|(a, _)| *a == update.pool_address) {
                entry.push((update.pool_address, update.dex_type));
            }
        }

        // Find other-DEX pools for same token
        let other_pools: Vec<(Pubkey, DexType)> = {
            let idx = mint_index.read().await;
            match idx.get(&token_mint) {
                Some(pools) => pools.iter()
                    .filter(|(addr, dex)| *addr != update.pool_address && *dex != update.dex_type)
                    .cloned()
                    .collect(),
                None => continue,
            }
        };

        if other_pools.is_empty() {
            continue;
        }
        updates_with_cross_dex += 1;

        // Get triggered pool price
        let cache = pool_streamer.cache();
        let updated_pool = match cache.get(&update.pool_address) {
            Some(p) => p.clone(),
            None => continue,
        };
        drop(cache);
        let price_a = match get_price_tokens_per_sol(&updated_pool) {
            Some(p) if p > 0.0 => p,
            _ => continue,
        };

        for (other_addr, other_dex) in &other_pools {
            let other_pool = match fetch_fresh_pool(&rpc, &pool_streamer.cache(), other_addr, *other_dex).await {
                Some(p) => p,
                None => continue,
            };
            let price_b = match get_price_tokens_per_sol(&other_pool) {
                Some(p) if p > 0.0 => p,
                _ => continue,
            };

            comparisons += 1;

            // Determine buy/sell direction
            let (buy_dex, buy_price, buy_pool, sell_dex, sell_price, sell_pool) = if price_a > price_b {
                // pool A gives more tokens per SOL = cheaper to buy there
                (dex_name(update.dex_type), price_a, update.pool_address,
                 dex_name(*other_dex), price_b, *other_addr)
            } else {
                (dex_name(*other_dex), price_b, *other_addr,
                 dex_name(update.dex_type), price_a, update.pool_address)
            };

            let spread_pct = (buy_price - sell_price) / sell_price * 100.0;
            let pair_key = format!("{}-{}", buy_dex, sell_dex);
            let stat = dex_pair_stats.entry(pair_key).or_insert((0, 0));
            stat.0 += 1;

            // Token price in USD
            let sol_price_usd = 93.4;
            let token_usd_buy = sol_price_usd / (buy_price / 1e6);
            let token_usd_sell = sol_price_usd / (sell_price / 1e6);

            // Simulate arb profit: buy 0.1 SOL worth, sell on other DEX
            let sim_input = 100_000_000u64; // 0.1 SOL
            let sol_is_a_buy = updated_pool.mint_a == wsol;
            let tokens_bought = if update.pool_address == buy_pool {
                updated_pool.math.get_amount_out(sim_input, sol_is_a_buy)
            } else {
                other_pool.math.get_amount_out(sim_input, other_pool.mint_a == wsol)
            };

            let sol_is_a_sell = if update.pool_address == sell_pool {
                updated_pool.mint_a == wsol
            } else {
                other_pool.mint_a == wsol
            };
            let sol_back = if update.pool_address == sell_pool {
                updated_pool.math.get_amount_out(tokens_bought, !sol_is_a_sell)
            } else {
                other_pool.math.get_amount_out(tokens_bought, !sol_is_a_sell)
            };

            let sim_profit_lamports = sol_back as i64 - sim_input as i64;
            let sim_profit_sol = sim_profit_lamports as f64 / 1e9;

            if sim_profit_lamports > 0 && spread_pct < 50.0 {
                arb_candidates += 1;
                stat.1 += 1;
                info!(
                    "[ARB] token={} | buy@{}({}) ${:.6} | sell@{}({}) ${:.6} | spread={:.2}% | profit={:.6} SOL ({:.4}%)",
                    token_mint,
                    buy_dex, &buy_pool.to_string()[..8], token_usd_buy,
                    sell_dex, &sell_pool.to_string()[..8], token_usd_sell,
                    spread_pct, sim_profit_sol,
                    sim_profit_sol / (sim_input as f64 / 1e9) * 100.0,
                );
            }

            // Periodic summary
            if comparisons % 200 == 0 {
                let elapsed = run_start.elapsed().as_secs();
                info!(
                    "[SUMMARY] {}s | updates={} cross_dex={} comparisons={} arb_signals={} ({:.1}%)",
                    elapsed, updates_received, updates_with_cross_dex,
                    comparisons, arb_candidates,
                    if comparisons > 0 { arb_candidates as f64 / comparisons as f64 * 100.0 } else { 0.0 },
                );
                let mut pairs: Vec<_> = dex_pair_stats.iter().collect();
                pairs.sort_by(|a, b| b.1.1.cmp(&a.1.1));
                for (pair, (comp, arb)) in pairs.iter().take(5) {
                    info!("  {:>20} : {:>5} cmp, {:>5} arb ({:.0}%)",
                        pair, comp, arb, if *comp > 0 { *arb as f64 / *comp as f64 * 100.0 } else { 0.0 });
                }
            }
        }
    }

    // Final report
    eprintln!("\n========== FINAL REPORT ==========");
    eprintln!("Runtime: {}s", run_start.elapsed().as_secs());
    eprintln!("gRPC updates: {}", updates_received);
    eprintln!("Updates with cross-DEX: {}", updates_with_cross_dex);
    eprintln!("Price comparisons: {}", comparisons);
    eprintln!("Arb signals (>1%): {}", arb_candidates);
    eprintln!("\nDEX pair breakdown:");
    let mut pairs: Vec<_> = dex_pair_stats.iter().collect();
    pairs.sort_by(|a, b| b.1.1.cmp(&a.1.1));
    eprintln!("{:<25} {:>8} {:>8} {:>8}", "Pair", "Compare", "Arb", "Rate");
    for (pair, (comp, arb)) in &pairs {
        eprintln!("{:<25} {:>8} {:>8} {:>7.1}%",
            pair, comp, arb, if *comp > 0 { *arb as f64 / *comp as f64 * 100.0 } else { 0.0 });
    }

    Ok(())
}
