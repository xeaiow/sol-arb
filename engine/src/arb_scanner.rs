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
use log::{info, debug};
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;

use solana_streamer_sdk::pool::cache::PoolStateCache;
use solana_streamer_sdk::pool::decoder::{orca_whirlpool, raydium_clmm};
use solana_streamer_sdk::pool::state::{DexType, PoolMath, PoolState, PoolUpdate};

use crate::opportunity::{Hop, Opportunity, PoolSnapshot, Route};

const WSOL: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
const MIN_SOL_RESERVE: u64 = 5_000_000_000; // 5 SOL
const MAX_SPREAD_PCT: f64 = 50.0;
const MIN_PROFIT_LAMPORTS: i64 = 10_000; // 0.00001 SOL minimum profit

pub struct ArbScanner {
    cache: Arc<PoolStateCache>,
    update_rx: mpsc::Receiver<PoolUpdate>,
    opportunity_tx: mpsc::Sender<Opportunity>,
    /// Dedup: (buy_pool, sell_pool) → last emit time
    recent: HashMap<(Pubkey, Pubkey), Instant>,
    /// Probe amount for price comparison
    probe_amount: u64,
}

impl ArbScanner {
    pub fn new(
        cache: Arc<PoolStateCache>,
        update_rx: mpsc::Receiver<PoolUpdate>,
        opportunity_tx: mpsc::Sender<Opportunity>,
        probe_amount: u64,
    ) -> Self {
        Self {
            cache,
            update_rx,
            opportunity_tx,
            recent: HashMap::new(),
            probe_amount,
        }
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
                        let (c, a) = self.process_update(&upd);
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

    fn process_update(&mut self, update: &PoolUpdate) -> (u64, u64) {
        // Only process SOL-paired pools
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

            // Must be different DEX
            if pool_b.dex_type == update.dex_type {
                continue;
            }

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

            if spread > MAX_SPREAD_PCT || spread < 0.1 {
                continue;
            }

            // Simulate profit
            let sol_is_a_buy = buy_pool.mint_a == WSOL;
            let tokens = buy_pool.math.get_amount_out(self.probe_amount, sol_is_a_buy);
            if tokens == 0 { continue; }

            let sol_is_a_sell = sell_pool.mint_a == WSOL;
            let sol_back = sell_pool.math.get_amount_out(tokens, !sol_is_a_sell);
            let profit = sol_back as i64 - self.probe_amount as i64;

            if profit < MIN_PROFIT_LAMPORTS {
                continue;
            }

            // Dedup: same pair within 2 seconds
            let pair_key = (buy_pool.address, sell_pool.address);
            if let Some(last) = self.recent.get(&pair_key) {
                if last.elapsed() < Duration::from_secs(2) {
                    continue;
                }
            }
            self.recent.insert(pair_key, Instant::now());

            // Build opportunity
            if let Some(opp) = self.build_opportunity(buy_pool, sell_pool, profit as u64) {
                let profit_sol = profit as f64 / 1e9;
                info!(
                    "[ARB] {} | buy@{:?}({}) sell@{:?}({}) | spread={:.2}% profit={:.6} SOL",
                    &token_mint.to_string()[..8],
                    buy_pool.dex_type, &buy_pool.address.to_string()[..8],
                    sell_pool.dex_type, &sell_pool.address.to_string()[..8],
                    spread, profit_sol,
                );
                let _ = self.opportunity_tx.try_send(opp);
                emitted += 1;
            }
        }

        (comparisons, emitted)
    }

    fn get_price(&self, pool: &PoolState) -> Option<f64> {
        let sol_is_a = pool.mint_a == WSOL;
        if !sol_is_a && pool.mint_b != WSOL {
            return None;
        }

        // Filter low-liquidity CP pools
        if let PoolMath::ConstantProduct { reserve_a, reserve_b, .. } = &pool.math {
            let sol_reserve = if sol_is_a { *reserve_a } else { *reserve_b };
            if sol_reserve < MIN_SOL_RESERVE {
                return None;
            }
        }

        // Filter CLMM/Whirlpool without tick arrays
        if let PoolMath::Concentrated { tick_arrays, fee_rate, .. } = &pool.math {
            if tick_arrays.is_empty() || *fee_rate == 0 {
                return None;
            }
        }

        let probe = 10_000_000u64; // 0.01 SOL
        let out = pool.math.get_amount_out(probe, sol_is_a);
        if out == 0 { return None; }
        Some(out as f64 / probe as f64 * 1e9)
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
                    mint_a_is_2022: buy_pool.mint_a_is_2022,
                    mint_b_is_2022: buy_pool.mint_b_is_2022,
                    accounts: buy_accounts,
                },
                PoolSnapshot {
                    address: sell_pool.address,
                    dex_type: sell_pool.dex_type,
                    mint_a: sell_pool.mint_a,
                    mint_b: sell_pool.mint_b,
                    is_a_to_b: !sell_sol_is_a,
                    mint_a_is_2022: sell_pool.mint_a_is_2022,
                    mint_b_is_2022: sell_pool.mint_b_is_2022,
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
