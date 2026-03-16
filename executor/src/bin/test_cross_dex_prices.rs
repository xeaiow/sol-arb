//! Cross-DEX price comparison tool — cache-only, zero RPC latency.
//!
//! 1. Bootstrap: build mint → pools index in cache
//! 2. Phase 5: subscribe all cross-DEX vaults to gRPC
//! 3. On gRPC update: read both pools from cache, compare prices
//! 4. Log [ARB] when sim_profit > 0
//!
//! Usage: cd executor && RUST_LOG=info cargo run --release --bin test_cross_dex_prices

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use log::info;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

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
const MIN_SOL_RESERVE: u64 = 5_000_000_000; // 5 SOL

/// Get price from a pool: how many tokens per 1 SOL (in raw smallest units)
fn get_price_tokens_per_sol(pool: &PoolState) -> Option<f64> {
    let wsol: Pubkey = WSOL.parse().unwrap();
    let sol_is_a = pool.mint_a == wsol;
    if !sol_is_a && pool.mint_b != wsol {
        return None;
    }

    // Skip low-liquidity CP pools
    if let PoolMath::ConstantProduct { reserve_a, reserve_b, .. } = &pool.math {
        let sol_reserve = if sol_is_a { *reserve_a } else { *reserve_b };
        if sol_reserve < MIN_SOL_RESERVE {
            return None;
        }
    }

    // Skip CLMM/Whirlpool without tick arrays
    if let PoolMath::Concentrated { tick_arrays, fee_rate, .. } = &pool.math {
        if tick_arrays.is_empty() || *fee_rate == 0 {
            return None;
        }
    }

    let probe = 10_000_000u64; // 0.01 SOL
    let out = pool.math.get_amount_out(probe, sol_is_a);
    if out == 0 {
        return None;
    }
    Some(out as f64 / probe as f64 * 1_000_000_000.0)
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

    let streamer_config = PoolStreamerConfig {
        rpc_url: rpc_url.clone(),
        ..Default::default()
    };
    let (pool_streamer, mut update_rx) = PoolStreamer::new(streamer_config);
    let pool_streamer = Arc::new(pool_streamer);

    eprintln!("Starting gRPC + bootstrap...");

    // gRPC setup
    let grpc = Arc::new(YellowstoneGrpc::new(grpc_endpoint, grpc_token)?);
    let program_ids = vec![
        PUMPFUN_PROGRAM_ID.to_string(), PUMPSWAP_PROGRAM_ID.to_string(),
        BONK_PROGRAM_ID.to_string(), RAYDIUM_CPMM_PROGRAM_ID.to_string(),
        RAYDIUM_CLMM_PROGRAM_ID.to_string(), RAYDIUM_AMM_V4_PROGRAM_ID.to_string(),
        METEORA_DAMM_V2_PROGRAM_ID.to_string(), METEORA_DLMM_PROGRAM_ID.to_string(),
        ORCA_WHIRLPOOL_PROGRAM_ID.to_string(),
    ];
    let protocols = vec![
        Protocol::PumpFun, Protocol::PumpSwap, Protocol::Bonk,
        Protocol::RaydiumCpmm, Protocol::RaydiumClmm, Protocol::RaydiumAmmV4,
        Protocol::MeteoraDammV2, Protocol::MeteoraDlmm, Protocol::OrcaWhirlpool,
    ];

    let sub_notify = pool_streamer.subscription_notify();
    let sub_streamer = pool_streamer.clone();
    let sub_grpc = grpc.clone();
    let sub_program_ids = program_ids.clone();

    let streamer_clone = pool_streamer.clone();
    grpc.subscribe_events_immediate(
        protocols, None,
        vec![TransactionFilter { account_include: program_ids.clone(), account_exclude: vec![], account_required: vec![] }],
        vec![AccountFilter { account: vec![], owner: program_ids.clone(), filters: vec![] }],
        None, None,
        move |event| {
            let s = streamer_clone.clone();
            tokio::spawn(async move { s.on_event(event).await; });
        },
    ).await?;

    // Subscription updater task
    let subscription_updater = {
        let mut account_set = std::collections::HashSet::new();
        let mut account_list = Vec::new();
        async move {
            loop {
                sub_notify.notified().await;
                let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(2);
                loop {
                    let new = sub_streamer.drain_pending_subscriptions().await;
                    for addr in new {
                        if account_set.insert(addr.clone()) { account_list.push(addr); }
                    }
                    if tokio::time::timeout_at(deadline, sub_notify.notified()).await.is_err() {
                        let final_new = sub_streamer.drain_pending_subscriptions().await;
                        for addr in final_new {
                            if account_set.insert(addr.clone()) { account_list.push(addr); }
                        }
                        break;
                    }
                }
                if account_list.is_empty() { continue; }
                info!("gRPC subscription update: {} explicit accounts", account_list.len());
                let tx_f = TransactionFilter { account_include: sub_program_ids.clone(), account_exclude: vec![], account_required: vec![] };
                let owner_f = AccountFilter { account: vec![], owner: sub_program_ids.clone(), filters: vec![] };
                let explicit_f = AccountFilter { account: account_list.clone(), owner: vec![], filters: vec![] };
                let _ = sub_grpc.update_subscription(vec![tx_f], vec![owner_f, explicit_f]).await;
            }
        }
    };
    tokio::spawn(subscription_updater);

    // Vault fetcher task
    let vault_notify = pool_streamer.vault_notify();
    let vault_streamer = pool_streamer.clone();
    tokio::spawn(async move {
        loop { vault_notify.notified().await; vault_streamer.flush_pending_vaults().await; }
    });

    // Tick reload task
    let tick_notify = pool_streamer.tick_reload_notify();
    let tick_streamer = pool_streamer.clone();
    tokio::spawn(async move {
        loop { tick_notify.notified().await; tick_streamer.flush_tick_reloads().await; }
    });

    // Bootstrap
    let gpa_url = config.engine.as_ref()
        .and_then(|e| e.gpa_rpc_url.clone())
        .or_else(|| config.executor.fallback_rpc_url.clone())
        .unwrap_or(rpc_url.clone());
    let bs = pool_streamer.clone();
    let bs_url = gpa_url.clone();
    tokio::spawn(async move {
        solana_streamer_sdk::pool::bootstrap::bootstrap_pools(&bs, &bs_url).await;
    });

    // Wait for bootstrap
    eprintln!("Waiting for bootstrap...");
    let start = Instant::now();
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        let count = pool_streamer.cache().all_pool_addresses().len();
        let elapsed = start.elapsed().as_secs();
        eprintln!("  {}s: {} pools", elapsed, count);
        if count > 100_000 && elapsed > 30 { break; }
        if elapsed > 300 { break; }
    }
    eprintln!("Bootstrap done. Listening for arb opportunities (cache-only, 0 RPC)...\n");

    // Stats
    let mut comparisons = 0u64;
    let mut arb_signals = 0u64;
    let mut updates_received = 0u64;
    let mut updates_cross_dex = 0u64;
    let mut dex_pair_stats: HashMap<String, (u64, u64)> = HashMap::new();
    let run_start = Instant::now();
    let wsol: Pubkey = WSOL.parse().unwrap();

    loop {
        let update = tokio::select! {
            Some(u) = update_rx.recv() => u,
            _ = tokio::signal::ctrl_c() => break,
        };

        updates_received += 1;

        let token_mint = if update.mint_a == wsol { update.mint_b }
            else if update.mint_b == wsol { update.mint_a }
            else { continue; };

        // Cache-only: find other-DEX pools for same token
        let cache = pool_streamer.cache();
        let all_pools = cache.pools_for_mint(&token_mint);
        drop(cache);

        let other_pools: Vec<(Pubkey, DexType)> = {
            let cache = pool_streamer.cache();
            all_pools.iter()
                .filter(|a| **a != update.pool_address)
                .filter_map(|a| {
                    let p = cache.get(a)?;
                    if p.dex_type != update.dex_type { Some((*a, p.dex_type)) } else { None }
                })
                .collect()
        };

        if other_pools.is_empty() { continue; }
        updates_cross_dex += 1;

        // Price from triggered pool (cache, just updated)
        let cache = pool_streamer.cache();
        let pool_a = match cache.get(&update.pool_address) {
            Some(p) => p.clone(), None => continue,
        };
        drop(cache);
        let price_a = match get_price_tokens_per_sol(&pool_a) {
            Some(p) if p > 0.0 => p, _ => continue,
        };

        for (other_addr, other_dex) in &other_pools {
            // Cache-only read (0 RPC!)
            let cache = pool_streamer.cache();
            let pool_b = match cache.get(other_addr) {
                Some(p) => p.clone(), None => continue,
            };
            drop(cache);
            let price_b = match get_price_tokens_per_sol(&pool_b) {
                Some(p) if p > 0.0 => p, _ => continue,
            };

            comparisons += 1;

            // Buy on cheaper DEX (more tokens per SOL), sell on dearer
            let (buy_dex, buy_price, buy_pool, sell_dex, sell_price, sell_pool, buy_pool_ref, sell_pool_ref) =
                if price_a > price_b {
                    (dex_name(update.dex_type), price_a, update.pool_address,
                     dex_name(*other_dex), price_b, *other_addr, &pool_a, &pool_b)
                } else {
                    (dex_name(*other_dex), price_b, *other_addr,
                     dex_name(update.dex_type), price_a, update.pool_address, &pool_b, &pool_a)
                };

            let spread_pct = (buy_price - sell_price) / sell_price * 100.0;

            // Simulate: buy 0.1 SOL worth on cheap DEX, sell on expensive
            let sim_input = 100_000_000u64;
            let tokens = buy_pool_ref.math.get_amount_out(sim_input, buy_pool_ref.mint_a == wsol);
            let sol_back = sell_pool_ref.math.get_amount_out(tokens, sell_pool_ref.mint_a != wsol);
            let profit = sol_back as i64 - sim_input as i64;

            let pair_key = format!("{}-{}", buy_dex, sell_dex);
            let stat = dex_pair_stats.entry(pair_key).or_insert((0, 0));
            stat.0 += 1;

            if profit > 0 && spread_pct < 50.0 {
                arb_signals += 1;
                stat.1 += 1;

                let sol_price_usd = 93.4;
                let token_usd_buy = sol_price_usd / (buy_price / 1e6);
                let token_usd_sell = sol_price_usd / (sell_price / 1e6);
                let profit_sol = profit as f64 / 1e9;

                info!(
                    "[ARB] token={} | buy@{}({}) ${:.6} | sell@{}({}) ${:.6} | spread={:.2}% | profit={:.6} SOL ({:.2}%) | cache_only",
                    token_mint,
                    buy_dex, &buy_pool.to_string()[..8], token_usd_buy,
                    sell_dex, &sell_pool.to_string()[..8], token_usd_sell,
                    spread_pct, profit_sol,
                    profit_sol / (sim_input as f64 / 1e9) * 100.0,
                );
            }

            if comparisons % 500 == 0 {
                let elapsed = run_start.elapsed().as_secs();
                info!(
                    "[SUMMARY] {}s | updates={} cross_dex={} cmp={} arb={} ({:.1}%)",
                    elapsed, updates_received, updates_cross_dex, comparisons, arb_signals,
                    if comparisons > 0 { arb_signals as f64 / comparisons as f64 * 100.0 } else { 0.0 },
                );
            }
        }
    }

    eprintln!("\n=== FINAL ===");
    eprintln!("Runtime: {}s | updates={} cmp={} arb={}",
        run_start.elapsed().as_secs(), updates_received, comparisons, arb_signals);
    let mut pairs: Vec<_> = dex_pair_stats.iter().collect();
    pairs.sort_by(|a, b| b.1.1.cmp(&a.1.1));
    for (pair, (c, a)) in &pairs {
        if *a > 0 { eprintln!("  {}: {} cmp, {} arb", pair, c, a); }
    }

    Ok(())
}
