//! Cross-DEX arbitrage scanner using cache-only price comparison.
//!
//! Replaces the DFS-based scanner with a simpler approach:
//! 1. Receive gRPC pool updates via PoolUpdate channel
//! 2. Look up same-token pools on other DEXes from cache mint index
//! 3. Compare prices (cache-only, 0 RPC)
//! 4. If profitable, emit Opportunity to executor

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arrayvec::ArrayVec;
use log::info;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;

use solana_streamer_sdk::pool::cache::PoolStateCache;
use solana_streamer_sdk::pool::decoder::{self, orca_whirlpool, raydium_clmm};
use solana_streamer_sdk::pool::state::{DexType, PoolMath, PoolState, PoolUpdate};

use crate::opportunity::{Hop, Opportunity, PoolSnapshot, Route};

const WSOL: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
const MIN_SOL_RESERVE: u64 = 2_000_000_000; // 2 SOL
const MAX_SPREAD_PCT: f64 = 50.0;
const MIN_SPREAD_PCT: f64 = 0.3; // < 0.3% can't profit after fees

pub struct ArbScanner {
    cache: Arc<PoolStateCache>,
    rpc: Arc<RpcClient>,
    update_rx: mpsc::Receiver<PoolUpdate>,
    opportunity_tx: mpsc::Sender<Opportunity>,
    recent: HashMap<(Pubkey, Pubkey), Instant>,
    probe_amount: u64,
    ready: Arc<std::sync::atomic::AtomicBool>,
}

impl ArbScanner {
    pub fn new(
        cache: Arc<PoolStateCache>,
        rpc: Arc<RpcClient>,
        update_rx: mpsc::Receiver<PoolUpdate>,
        opportunity_tx: mpsc::Sender<Opportunity>,
        probe_amount: u64,
    ) -> Self {
        Self {
            cache,
            rpc,
            update_rx,
            opportunity_tx,
            recent: HashMap::new(),
            probe_amount,
            ready: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Get a handle to set ready state (call after bootstrap completes)
    pub fn ready_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.ready.clone()
    }

    pub async fn run(&mut self) {
        let mut stats_timer = tokio::time::interval(Duration::from_secs(30));
        stats_timer.tick().await; // skip first

        let mut updates = 0u64;
        let mut comparisons = 0u64;
        let mut arb_emitted = 0u64;

        loop {
            tokio::select! {
                Some(update) = self.update_rx.recv() => {
                    updates += 1;

                    // Also drain pending updates (batch processing)
                    let mut batch = vec![update];
                    while let Ok(u) = self.update_rx.try_recv() {
                        batch.push(u);
                        if batch.len() >= 100 { break; }
                    }
                    updates += batch.len() as u64 - 1;

                    for upd in batch {
                        let (c, a) = self.process_update(&upd).await;
                        comparisons += c;
                        arb_emitted += a;
                    }
                }
                _ = stats_timer.tick() => {
                    info!("[ARB_SCANNER] updates={} cmp={} emitted={} dedup_cache={}",
                        updates, comparisons, arb_emitted, self.recent.len());
                    // Clean old dedup entries
                    let cutoff = Instant::now() - Duration::from_secs(5);
                    self.recent.retain(|_, t| *t > cutoff);
                }
            }
        }
    }

    async fn process_update(&mut self, update: &PoolUpdate) -> (u64, u64) {
        if !self.ready.load(std::sync::atomic::Ordering::Relaxed) {
            return (0, 0);
        }

        let token_mint = if update.mint_a == WSOL {
            update.mint_b
        } else if update.mint_b == WSOL {
            update.mint_a
        } else {
            return (0, 0);
        };

        // Find other-DEX pools from cache
        let all_pools = self.cache.pools_for_mint(&token_mint);
        if all_pools.len() < 2 {
            return (0, 0);
        }

        // Get triggered pool from cache
        let pool_a = match self.cache.get(&update.pool_address) {
            Some(p) => p.clone(),
            None => return (0, 0),
        };
        let price_a = match self.get_price(&pool_a) {
            Some(p) => p,
            None => return (0, 0),
        };

        let mut comparisons = 0u64;
        let mut emitted = 0u64;

        for other_addr in &all_pools {
            if *other_addr == update.pool_address {
                continue;
            }

            let pool_b = match self.cache.get(other_addr) {
                Some(p) => p.clone(),
                None => continue,
            };

            // Skip same pool (same address), but allow same DEX different pool
            // (e.g. two DLMM pools with different bin_step for same token)

            let price_b = match self.get_price(&pool_b) {
                Some(p) => p,
                None => continue,
            };

            comparisons += 1;

            // Determine buy/sell direction
            let (buy_pool, sell_pool) = if price_a > price_b {
                (&pool_a, &pool_b) // A gives more tokens = cheaper
            } else {
                (&pool_b, &pool_a)
            };

            let buy_price = price_a.max(price_b);
            let sell_price = price_a.min(price_b);
            let spread = (buy_price - sell_price) / sell_price * 100.0;

            if spread > MAX_SPREAD_PCT || spread < MIN_SPREAD_PCT {
                continue;
            }

            let t_compare = Instant::now();
            let sol_is_a_buy = buy_pool.mint_a == WSOL;
            let sol_is_a_sell = sell_pool.mint_a == WSOL;

            let probe_tokens = buy_pool.math.get_amount_out(self.probe_amount, sol_is_a_buy);
            if probe_tokens == 0 { continue; }

            // PumpSwap u64 overflow check: on-chain uses u64 for amt*reserve
            if buy_pool.dex_type == DexType::PumpSwap || sell_pool.dex_type == DexType::PumpSwap {
                // Check buy side: input * token_reserve must fit u64
                if let PoolMath::ConstantProduct { reserve_a, reserve_b, .. } = &buy_pool.math {
                    let r_out = if sol_is_a_buy { *reserve_b } else { *reserve_a };
                    if (self.probe_amount as u128) * (r_out as u128) > u64::MAX as u128 {
                        continue;
                    }
                }
                // Check sell side: tokens * sol_reserve must fit u64
                if let PoolMath::ConstantProduct { reserve_a, reserve_b, .. } = &sell_pool.math {
                    let r_out = if !sol_is_a_sell { *reserve_a } else { *reserve_b };
                    if (probe_tokens as u128) * (r_out as u128) > u64::MAX as u128 {
                        continue;
                    }
                }
            }

            let probe_back = sell_pool.math.get_amount_out(probe_tokens, !sol_is_a_sell);
            let probe_profit = probe_back as i64 - self.probe_amount as i64;

            // Blind probe: allow up to -0.5% at probe stage.
            // On-chain PROD MODE validates — revert if actual loss.
            let probe_threshold = -(self.probe_amount as i64 / 200); // -0.5%
            if probe_profit < probe_threshold { continue; }

            // Dedup: same pair within 2 seconds
            let pair_key = (buy_pool.address, sell_pool.address);
            if let Some(last) = self.recent.get(&pair_key) {
                if last.elapsed() < Duration::from_secs(2) {
                    continue;
                }
            }
            self.recent.insert(pair_key, Instant::now());

            let best_amount = self.probe_amount;
            if let Some(mut opp) = self.build_opportunity(buy_pool, sell_pool, probe_profit.max(0) as u64) {
                opp.amount_in = best_amount;
                let scan_us = t_compare.elapsed().as_micros();
                info!(
                    "[ARB] {} | buy@{:?}({}) sell@{:?}({}) | spread={:.2}% in={:.4} probe_profit={:.6} SOL | scan={}µs",
                    &token_mint.to_string()[..8],
                    buy_pool.dex_type, &buy_pool.address.to_string()[..8],
                    sell_pool.dex_type, &sell_pool.address.to_string()[..8],
                    spread, best_amount as f64 / 1e9, probe_profit as f64 / 1e9, scan_us,
                );
                let _ = self.opportunity_tx.try_send(opp);
                emitted += 1;
            }
        }

        (comparisons, emitted)
    }

    /// Fetch fresh data from RPC for a pool before sending TX.
    /// For DLMM: re-fetch bin arrays. For CP: re-fetch vault balances.
    async fn refresh_pool_data(&self, pool: &PoolState) -> PoolState {
        let mut fresh = pool.clone();

        match fresh.dex_type {
            DexType::MeteoraDlmm => {
                if let PoolMath::MeteoraDlmm { active_id, ref mut bin_arrays, .. } = fresh.math {
                    bin_arrays.clear();
                    for pda in decoder::meteora_dlmm::bin_array_pdas_for_swap(&fresh.address, active_id) {
                        if let Ok(d) = self.rpc.get_account_data(&pda).await {
                            if let Some(ba) = decoder::meteora_dlmm::decode_bin_array(&d) {
                                bin_arrays.push(ba);
                            }
                        }
                    }
                }
            }
            DexType::PumpSwap | DexType::RaydiumCpmm | DexType::RaydiumAmmV4 => {
                if let (Some(va), Some(vb)) = (fresh.vault_a, fresh.vault_b) {
                    for (vault, is_a) in [(va, true), (vb, false)] {
                        if let Ok(d) = self.rpc.get_account_data(&vault).await {
                            if d.len() >= 72 {
                                let bal = u64::from_le_bytes(d[64..72].try_into().unwrap_or([0;8]));
                                if let PoolMath::ConstantProduct { ref mut reserve_a, ref mut reserve_b, .. } = fresh.math {
                                    if is_a { *reserve_a = bal; } else { *reserve_b = bal; }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        fresh
    }

    /// Get price as tokens-per-SOL ratio.
    /// CP pools: exact from reserves.
    /// DLMM/CLMM: use vault balance ratio as proxy (updated from tx meta).
    /// On-chain PROD MODE validates the actual swap — this is just screening.
    fn get_price(&self, pool: &PoolState) -> Option<f64> {
        let sol_is_a = pool.mint_a == WSOL;
        if !sol_is_a && pool.mint_b != WSOL {
            return None;
        }

        match &pool.math {
            PoolMath::ConstantProduct { reserve_a, reserve_b, .. } => {
                let (sol_res, tok_res) = if sol_is_a {
                    (*reserve_a, *reserve_b)
                } else {
                    (*reserve_b, *reserve_a)
                };
                if sol_res < MIN_SOL_RESERVE || tok_res == 0 {
                    return None;
                }
                Some(tok_res as f64 / sol_res as f64 * 1e9)
            }
            _ => {
                // DLMM/CLMM/DammV2/Whirlpool: use get_amount_out as probe
                // (reads whatever bin/tick data is in cache — may be stale, that's OK)
                let probe = 10_000_000u64;
                let out = pool.math.get_amount_out(probe, sol_is_a);
                if out == 0 { return None; }
                Some(out as f64 / probe as f64 * 1e9)
            }
        }
    }

    /// Ternary search for optimal input amount that maximizes profit.
    /// Returns (best_amount, best_profit_lamports).
    fn find_optimal_amount(
        &self,
        buy_pool: &PoolState,
        sell_pool: &PoolState,
        sol_is_a_buy: bool,
        sol_is_a_sell: bool,
    ) -> (u64, i64) {
        let max_input = 10_000_000_000u64; // 10 SOL cap
        let mut lo = 1_000_000u64;   // 0.001 SOL min
        let mut hi = max_input;

        // Ternary search: profit curve is concave (increases then decreases due to slippage)
        for _ in 0..30 {
            if hi - lo < 100_000 { break; } // 0.0001 SOL precision
            let m1 = lo + (hi - lo) / 3;
            let m2 = hi - (hi - lo) / 3;

            let p1 = self.sim_profit(buy_pool, sell_pool, m1, sol_is_a_buy, sol_is_a_sell);
            let p2 = self.sim_profit(buy_pool, sell_pool, m2, sol_is_a_buy, sol_is_a_sell);

            if p1 < p2 {
                lo = m1;
            } else {
                hi = m2;
            }
        }

        let best = (lo + hi) / 2;
        let profit = self.sim_profit(buy_pool, sell_pool, best, sol_is_a_buy, sol_is_a_sell);
        (best, profit)
    }

    #[inline]
    fn sim_profit(
        &self,
        buy_pool: &PoolState,
        sell_pool: &PoolState,
        amount_in: u64,
        sol_is_a_buy: bool,
        sol_is_a_sell: bool,
    ) -> i64 {
        let tokens = buy_pool.math.get_amount_out(amount_in, sol_is_a_buy);
        if tokens == 0 { return i64::MIN; }
        let sol_back = sell_pool.math.get_amount_out(tokens, !sol_is_a_sell);
        sol_back as i64 - amount_in as i64
    }

    fn build_opportunity(&self, buy_pool: &PoolState, sell_pool: &PoolState, profit: u64) -> Option<Opportunity> {
        let buy_sol_is_a = buy_pool.mint_a == WSOL;
        let sell_sol_is_a = sell_pool.mint_a == WSOL;

        let buy_accounts = self.build_pool_accounts(buy_pool, buy_sol_is_a);
        let sell_accounts = self.build_pool_accounts(sell_pool, !sell_sol_is_a);

        if buy_accounts.is_empty() || sell_accounts.is_empty() {
            return None;
        }

        let mut hops = ArrayVec::new();
        hops.push(Hop { pool_index: 0, is_a_to_b: buy_sol_is_a });
        hops.push(Hop { pool_index: 1, is_a_to_b: !sell_sol_is_a });

        let slot = buy_pool.last_updated_slot.max(sell_pool.last_updated_slot);

        // Detect Token-2022: check cache flags, fallback to mint address heuristic
        // (pump tokens ending in "pump" are always Token-2022)
        let detect_2022 = |pool: &PoolState| -> (bool, bool) {
            let mut a = pool.mint_a_is_2022;
            let mut b = pool.mint_b_is_2022;
            if !a && pool.mint_a != WSOL {
                a = pool.mint_a.to_string().ends_with("pump");
            }
            if !b && pool.mint_b != WSOL {
                b = pool.mint_b.to_string().ends_with("pump");
            }
            (a, b)
        };
        let (buy_a_2022, buy_b_2022) = detect_2022(buy_pool);
        let (sell_a_2022, sell_b_2022) = detect_2022(sell_pool);

        Some(Opportunity {
            route: Route { hops, base_mint: WSOL },
            amount_in: self.probe_amount,
            expected_profit: profit,
            pool_snapshots: vec![
                PoolSnapshot {
                    address: buy_pool.address,
                    dex_type: buy_pool.dex_type,
                    mint_a: buy_pool.mint_a,
                    mint_b: buy_pool.mint_b,
                    is_a_to_b: buy_sol_is_a,
                    mint_a_is_2022: buy_a_2022,
                    mint_b_is_2022: buy_b_2022,
                    accounts: buy_accounts,
                },
                PoolSnapshot {
                    address: sell_pool.address,
                    dex_type: sell_pool.dex_type,
                    mint_a: sell_pool.mint_a,
                    mint_b: sell_pool.mint_b,
                    is_a_to_b: !sell_sol_is_a,
                    mint_a_is_2022: sell_a_2022,
                    mint_b_is_2022: sell_b_2022,
                    accounts: sell_accounts,
                },
            ],
            slot,
        })
    }

    /// Build pool accounts list for TxBuilder.
    /// Layout: [pool_address, vault_a, vault_b, extra_accounts..., tick_arrays...]
    fn build_pool_accounts(&self, pool: &PoolState, is_a_to_b: bool) -> Vec<Pubkey> {
        let mut accounts = Vec::with_capacity(8);
        accounts.push(pool.address);
        if let Some(va) = pool.vault_a { accounts.push(va); }
        if let Some(vb) = pool.vault_b { accounts.push(vb); }
        accounts.extend_from_slice(&pool.extra_accounts);

        // DLMM: add bin array PDAs (computed from active_id)
        if pool.dex_type == DexType::MeteoraDlmm {
            if let PoolMath::MeteoraDlmm { active_id, bin_arrays, .. } = &pool.math {
                if bin_arrays.is_empty() {
                    // No bin arrays loaded — compute PDAs from active_id
                    let pdas = decoder::meteora_dlmm::bin_array_pdas_for_swap(&pool.address, *active_id);
                    accounts.extend_from_slice(&pdas);
                } else {
                    // Use cached bin array PDAs (already in extra_accounts from Phase 3)
                    // Only add if not already there
                    let pdas = decoder::meteora_dlmm::bin_array_pdas_for_swap(&pool.address, *active_id);
                    for pda in &pdas {
                        if !accounts.contains(pda) {
                            accounts.push(*pda);
                        }
                    }
                }
            }
        }

        // CLMM/Whirlpool: add tick array PDAs ordered by swap direction
        if pool.dex_type == DexType::RaydiumClmm || pool.dex_type == DexType::OrcaWhirlpool {
            if let PoolMath::Concentrated { tick_current, tick_spacing, tick_arrays, .. } = &pool.math {
                if tick_arrays.is_empty() {
                    return vec![];
                }
                let is_whirlpool = pool.dex_type == DexType::OrcaWhirlpool;
                let ticks_per_array: i32 = if is_whirlpool { 88 } else { 60 };
                let ts = *tick_spacing as i32;
                let ticks_per_array_total = ts * ticks_per_array;

                let ref_tick = if is_whirlpool && !is_a_to_b {
                    *tick_current + ts
                } else {
                    *tick_current
                };

                let containing = tick_arrays.iter().find(|ta| {
                    ref_tick >= ta.start_tick_index
                        && ref_tick < ta.start_tick_index + ticks_per_array_total
                });

                let Some(first_ta) = containing else {
                    return vec![];
                };

                let first_pda = if is_whirlpool {
                    orca_whirlpool::tick_array_pda(&pool.address, first_ta.start_tick_index)
                } else {
                    raydium_clmm::tick_array_pda(&pool.address, first_ta.start_tick_index)
                };
                if let Some(pda) = first_pda {
                    accounts.push(pda);
                }

                let mut others: Vec<_> = tick_arrays.iter()
                    .filter(|ta| ta.start_tick_index != first_ta.start_tick_index)
                    .collect();

                if is_a_to_b {
                    others.retain(|ta| ta.start_tick_index < first_ta.start_tick_index);
                    others.sort_by(|a, b| b.start_tick_index.cmp(&a.start_tick_index));
                } else {
                    others.retain(|ta| ta.start_tick_index > first_ta.start_tick_index);
                    others.sort_by(|a, b| a.start_tick_index.cmp(&b.start_tick_index));
                }

                for ta in others.iter().take(2) {
                    let pda = if is_whirlpool {
                        orca_whirlpool::tick_array_pda(&pool.address, ta.start_tick_index)
                    } else {
                        raydium_clmm::tick_array_pda(&pool.address, ta.start_tick_index)
                    };
                    if let Some(pda) = pda {
                        accounts.push(pda);
                    }
                }
            }
        }

        accounts
    }
}
