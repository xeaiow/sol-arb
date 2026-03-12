use std::collections::HashMap;
use std::time::Instant;
use log::{info, debug};
use rayon::prelude::*;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;

use solana_streamer_sdk::pool::state::{DexType, PoolMath, PoolUpdate};
use solana_streamer_sdk::pool::decoder::raydium_clmm;

use crate::config::EngineConfig;
use crate::graph::{PoolEntry, TokenGraph};
use crate::opportunity::{Opportunity, PoolSnapshot, Route};
use crate::optimizer;
use crate::route::RouteTable;

/// Scanner state
enum Phase {
    Warmup { start: Instant },
    Running,
}

/// A compact key for deduplicating routes (up to 4 hops)
type RouteKey = ([u32; 4], u8); // (pool_indices, hop_count)

fn route_key(route: &Route) -> RouteKey {
    let mut pools = [0u32; 4];
    for (i, hop) in route.hops.iter().enumerate() {
        pools[i] = hop.pool_index;
    }
    (pools, route.hops.len() as u8)
}

/// The main scanner that ties everything together
pub struct Scanner {
    config: EngineConfig,
    graph: TokenGraph,
    route_table: RouteTable,
    phase: Phase,
    update_rx: mpsc::Receiver<PoolUpdate>,
    opportunity_tx: mpsc::Sender<Opportunity>,
    /// Dedup: route_key -> (slot, profit) of last emitted opportunity
    recent_emissions: HashMap<RouteKey, (u64, u64)>,
}

impl Scanner {
    pub fn new(
        config: EngineConfig,
        update_rx: mpsc::Receiver<PoolUpdate>,
        opportunity_tx: mpsc::Sender<Opportunity>,
    ) -> Self {
        Self {
            config,
            graph: TokenGraph::new(),
            route_table: RouteTable::new(),
            phase: Phase::Warmup { start: Instant::now() },
            update_rx,
            opportunity_tx,
            recent_emissions: HashMap::new(),
        }
    }

    /// Main loop
    pub async fn run(&mut self) {
        let full_scan_interval =
            tokio::time::Duration::from_secs(self.config.full_scan_interval_secs);
        let mut full_scan_timer = tokio::time::interval(full_scan_interval);
        // First tick fires immediately; skip it
        full_scan_timer.tick().await;

        loop {
            tokio::select! {
                Some(update) = self.update_rx.recv() => {
                    self.handle_update(update);
                }
                _ = full_scan_timer.tick() => {
                    if matches!(self.phase, Phase::Running) {
                        self.full_scan();
                    }
                }
            }
        }
    }

    fn handle_update(&mut self, update: PoolUpdate) {
        // Detect reserve transition: was the pool previously dead (zero reserves)?
        let was_dead = self.graph.address_to_pool.get(&update.pool_address)
            .map(|&idx| self.graph.is_pool_dead(idx))
            .unwrap_or(false);

        // Upsert pool into graph
        let entry = PoolEntry {
            address: update.pool_address,
            mint_a: update.mint_a,
            mint_b: update.mint_b,
            math: update.math,
            dex_type: update.dex_type,
            vault_a: update.vault_a,
            vault_b: update.vault_b,
            mint_a_is_2022: update.mint_a_is_2022,
            mint_b_is_2022: update.mint_b_is_2022,
            extra_accounts: update.extra_accounts,
            last_updated_slot: update.slot,
        };

        let is_new = !self.graph.address_to_pool.contains_key(&update.pool_address);
        let pool_index = self.graph.add_pool(entry);

        // Check if pool transitioned from dead to alive (reserves became non-zero)
        let now_alive = was_dead && !self.graph.is_pool_dead(pool_index);

        match &self.phase {
            Phase::Warmup { start } => {
                let elapsed = start.elapsed().as_secs();
                let pool_count = self.graph.pool_count();

                if elapsed >= self.config.warmup_secs
                    || pool_count >= self.config.warmup_pool_count
                {
                    info!(
                        "Warmup complete: {} pools, {} mints in {}s. Building route table...",
                        pool_count,
                        self.graph.mint_count(),
                        elapsed,
                    );
                    self.route_table.build_from_graph(&self.graph, &self.config);
                    self.phase = Phase::Running;
                    info!(
                        "Route table ready: {} routes. Scanning for opportunities.",
                        self.route_table.route_count(),
                    );
                }
            }
            Phase::Running => {
                // Build routes when pool is new OR when it transitions from dead to alive
                // (e.g., vault balances arrive after initial registration with reserves=0)
                if is_new || now_alive {
                    let before = self.route_table.route_count();
                    self.route_table.add_routes_for_pool(
                        &self.graph,
                        &self.config,
                        pool_index,
                    );
                    let after = self.route_table.route_count();
                    if after > before {
                        let reason = if now_alive { "revived" } else { "new" };
                        info!("Pool {} ({}) added {} routes (total {})",
                            update.pool_address, reason, after - before, after);
                    }
                }

                // Incremental scan: only routes affected by this pool
                self.scan_routes_for_pool(pool_index, update.slot);
            }
        }
    }

    /// Scan routes affected by a specific pool update.
    /// Uses parallel probe → sort → parallel ternary → emit best.
    fn scan_routes_for_pool(&mut self, pool_index: u32, slot: u64) {
        let route_indices = self.route_table.index.routes_for_pool(pool_index);
        let indices: Vec<u32> = route_indices.to_vec();

        if indices.is_empty() {
            return;
        }

        debug!("Incremental scan: pool {} triggered {} routes (slot {})",
            pool_index, indices.len(), slot);

        // Collect routes (clone to release borrow on route_table)
        let routes: Vec<(u32, Route)> = indices.iter()
            .map(|&idx| (idx, self.route_table.routes[idx as usize].clone()))
            .collect();

        // Phase 1: parallel probe — filter and rank routes by probe profit
        let probe_amount = self.config.probe_amount_lamports;
        let min_reserve = self.config.min_reserve_lamports;
        let graph = &self.graph;

        let mut probed: Vec<(u32, Route, i64)> = routes.into_par_iter()
            .filter_map(|(idx, route)| {
                // Quick reserve check
                for hop in &route.hops {
                    if !graph.pool_has_min_reserve(hop.pool_index, min_reserve) {
                        return None;
                    }
                }
                let probe_profit = optimizer::simulate_route_profit(&route, graph, probe_amount);
                if probe_profit > 0 {
                    Some((idx, route, probe_profit))
                } else {
                    None
                }
            })
            .collect();

        if probed.is_empty() {
            return;
        }

        // Sort by probe profit descending — best candidates first
        probed.sort_unstable_by(|a, b| b.2.cmp(&a.2));

        // Phase 2: parallel ternary search on top candidates (cap to avoid wasted work)
        let top_n = probed.len().min(10);
        let top_candidates = &probed[..top_n];

        let max_input = self.config.max_input_lamports;
        let ternary_iters = self.config.ternary_iterations;
        let min_profit = self.config.min_profit_lamports;
        let max_profit_ratio = self.config.max_profit_ratio;

        let mut evaluated: Vec<(Route, u64, u64)> = top_candidates.par_iter()
            .filter_map(|(_, route, _)| {
                let (amount_in, profit) = optimizer::find_optimal_amount(
                    route, graph, max_input, ternary_iters,
                )?;
                if profit < min_profit {
                    return None;
                }
                if amount_in > 0 && (profit as f64 / amount_in as f64) > max_profit_ratio {
                    return None;
                }
                Some((route.clone(), amount_in, profit))
            })
            .collect();

        if evaluated.is_empty() {
            return;
        }

        // Sort by profit descending — emit best first
        evaluated.sort_unstable_by(|a, b| b.2.cmp(&a.2));

        // Phase 3: emit opportunities (sequential — needs &mut self for dedup + channel)
        for (route, amount_in, profit) in evaluated {
            self.emit_opportunity(&route, slot, amount_in, profit);
        }
    }

    /// Periodic maintenance: prune dead pools, reset dedup, log stats.
    /// Route evaluation is fully handled by incremental scan on each pool update.
    fn full_scan(&mut self) {
        let start = Instant::now();

        // --- Dead pool pruning ---
        let pool_count = self.graph.pool_count();
        let mut dead_pools: Vec<u32> = Vec::new();
        for i in 0..pool_count {
            let pool = &self.graph.pools[i];
            if pool.last_updated_slot == 0 {
                continue;
            }
            if let PoolMath::ConstantProduct { reserve_a, reserve_b, .. } = &pool.math {
                if *reserve_a == 0 && *reserve_b == 0 {
                    continue;
                }
            }
            if pool.dex_type == DexType::RaydiumClmm {
                if let PoolMath::Concentrated { tick_arrays, .. } = &pool.math {
                    if tick_arrays.is_empty() {
                        continue;
                    }
                }
            }
            if self.graph.is_pool_dead(i as u32) {
                dead_pools.push(i as u32);
            }
        }

        if !dead_pools.is_empty() {
            for &pi in &dead_pools {
                self.graph.remove_pool_edges(pi);
            }
            let routes_before = self.route_table.routes.len();
            self.route_table.remove_routes_with_pools(&dead_pools);
            let routes_after = self.route_table.routes.len();
            info!(
                "Pruned {} dead pools, removed {} routes ({} -> {})",
                dead_pools.len(),
                routes_before - routes_after,
                routes_before,
                routes_after,
            );
        }

        // --- Clear dedup map (allow re-evaluation of previously seen routes) ---
        self.recent_emissions.clear();

        // --- Stats log ---
        let mut dex_counts = [0usize; 7];
        let mut clmm_ready = 0usize;
        let mut clmm_not_ready = 0usize;
        for pool in &self.graph.pools {
            let idx = pool.dex_type as usize;
            if idx < dex_counts.len() {
                dex_counts[idx] += 1;
            }
            if pool.dex_type == DexType::RaydiumClmm {
                if let PoolMath::Concentrated { tick_arrays, .. } = &pool.math {
                    if tick_arrays.is_empty() { clmm_not_ready += 1; } else { clmm_ready += 1; }
                }
            }
        }

        let elapsed = start.elapsed();
        info!(
            "Maintenance: {} routes, {} pools in {:?} | [AmmV4:{} CPMM:{} CLMM:{}/{} PumpFun:{} PumpSwap:{} Bonk:{} Meteora:{}]",
            self.route_table.routes.len(),
            self.graph.pool_count(),
            elapsed,
            dex_counts[0], dex_counts[1],
            clmm_ready, clmm_ready + clmm_not_ready,
            dex_counts[3], dex_counts[4], dex_counts[5], dex_counts[6],
        );
    }

    /// Emit a pre-evaluated opportunity. Handles dedup and snapshot building.
    fn emit_opportunity(&mut self, route: &Route, slot: u64, amount_in: u64, profit: u64) -> bool {
        let key = route_key(route);

        // Dedup: skip if same route was recently emitted with >= profit
        if let Some(&(prev_slot, prev_profit)) = self.recent_emissions.get(&key) {
            if slot <= prev_slot + 1 && profit <= prev_profit + prev_profit / 10 {
                debug!("Route {:?} dedup: profit={} vs prev={} (slot {} vs {})",
                    key.0, profit, prev_profit, slot, prev_slot);
                return false;
            }
        }

        // Build opportunity with pool snapshots
        let mut pool_snapshots = Vec::with_capacity(route.hops.len());
        let mut max_slot = slot;

        for hop in &route.hops {
            let pool = &self.graph.pools[hop.pool_index as usize];
            if pool.last_updated_slot > max_slot {
                max_slot = pool.last_updated_slot;
            }
            let accounts = Self::build_pool_accounts(pool, hop.is_a_to_b);
            if accounts.is_empty() {
                return false;
            }
            pool_snapshots.push(PoolSnapshot {
                address: pool.address,
                dex_type: pool.dex_type,
                mint_a: pool.mint_a,
                mint_b: pool.mint_b,
                is_a_to_b: hop.is_a_to_b,
                mint_a_is_2022: pool.mint_a_is_2022,
                mint_b_is_2022: pool.mint_b_is_2022,
                accounts,
            });
        }

        let opportunity = Opportunity {
            route: route.clone(),
            amount_in,
            expected_profit: profit,
            pool_snapshots,
            slot: max_slot,
        };

        if self.opportunity_tx.try_send(opportunity).is_ok() {
            self.recent_emissions.insert(key, (max_slot, profit));
            true
        } else {
            debug!("Opportunity channel full, dropping");
            false
        }
    }

    /// Build the per-hop pool accounts list for a given pool.
    /// These are the accounts that swap.rs receives as `pool_accounts`.
    /// User-specific accounts (payer, user ATAs) and fixed program IDs are
    /// filled in by TxBuilder — this only returns pool-specific accounts.
    fn build_pool_accounts(pool: &PoolEntry, _is_a_to_b: bool) -> Vec<Pubkey> {
        // Layout: [pool_address, vault_a, vault_b, extra_accounts...]
        // TxBuilder expects extra_accounts ordering per DEX:
        //   RaydiumCpmm: [amm_config, observation_key]
        //   RaydiumClmm: [amm_config, observation_key, tick_array]
        //   PumpFun: [creator]
        //   Bonk: [global_config, platform_config]
        let mut accounts = Vec::with_capacity(4 + pool.extra_accounts.len() + 1);
        accounts.push(pool.address);
        if let Some(vault_a) = pool.vault_a {
            accounts.push(vault_a);
        }
        if let Some(vault_b) = pool.vault_b {
            accounts.push(vault_b);
        }
        accounts.extend_from_slice(&pool.extra_accounts);

        // CLMM: pass tick array PDAs. Raydium CLMM requires the FIRST tick array
        // to contain tick_current. If no loaded tick array covers current tick,
        // return empty to signal pool isn't ready (needs tick array reload).
        if pool.dex_type == DexType::RaydiumClmm {
            if let PoolMath::Concentrated { tick_current, tick_spacing, tick_arrays, .. } = &pool.math {
                if tick_arrays.is_empty() {
                    return vec![];
                }
                let ts = *tick_spacing as i32;
                let ticks_per_array = ts * 60; // 60 ticks per array (Raydium convention)
                let current_start = raydium_clmm::tick_array_start_index(*tick_current, *tick_spacing);

                // Find the tick array that contains tick_current (must be first)
                let containing = tick_arrays.iter().find(|ta| {
                    *tick_current >= ta.start_tick_index
                        && *tick_current < ta.start_tick_index + ticks_per_array
                });

                let Some(first_ta) = containing else {
                    // No tick array covers current tick — pool not ready
                    return vec![];
                };

                // First: the containing tick array
                if let Some(pda) = raydium_clmm::tick_array_pda(&pool.address, first_ta.start_tick_index) {
                    accounts.push(pda);
                }

                // Then: remaining tick arrays sorted by distance from current
                let mut others: Vec<_> = tick_arrays.iter()
                    .filter(|ta| ta.start_tick_index != first_ta.start_tick_index)
                    .collect();
                others.sort_by_key(|ta| (ta.start_tick_index - current_start).unsigned_abs());

                for ta in others.iter().take(2) {
                    if let Some(pda) = raydium_clmm::tick_array_pda(&pool.address, ta.start_tick_index) {
                        accounts.push(pda);
                    }
                }
            }
        }

        accounts
    }
}
