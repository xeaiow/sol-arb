use std::collections::HashMap;
use std::time::Instant;
use log::{info, debug};
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
                if is_new {
                    // New pool: incrementally add routes
                    let before = self.route_table.route_count();
                    self.route_table.add_routes_for_pool(
                        &self.graph,
                        &self.config,
                        pool_index,
                    );
                    let after = self.route_table.route_count();
                    if after > before {
                        info!("New pool {} added {} routes (total {})",
                            update.pool_address, after - before, after);
                    }
                }

                // Incremental scan: only routes affected by this pool
                self.scan_routes_for_pool(pool_index, update.slot);
            }
        }
    }

    /// Scan routes affected by a specific pool update
    fn scan_routes_for_pool(&mut self, pool_index: u32, slot: u64) {
        let route_indices = self.route_table.index.routes_for_pool(pool_index);
        // Collect indices to avoid borrow conflict
        let indices: Vec<u32> = route_indices.to_vec();

        for route_idx in indices {
            let route = self.route_table.routes[route_idx as usize].clone();
            self.evaluate_route(&route, slot);
        }
    }

    /// Full scan: prune dead pools, then evaluate all routes
    fn full_scan(&mut self) {
        let start = Instant::now();

        // --- Dead pool pruning ---
        let pool_count = self.graph.pool_count();
        let mut dead_pools: Vec<u32> = Vec::new();
        for i in 0..pool_count {
            let pool = &self.graph.pools[i];
            // Only prune pools that have received at least one update (last_updated_slot > 0).
            // Newly registered pools may have reserve=0 because vault balance hasn't arrived yet.
            // Never prune CLMM pools with empty tick_arrays — they aren't dead, just not loaded yet.
            if pool.last_updated_slot > 0 && self.graph.is_pool_dead(i as u32) {
                if pool.dex_type == DexType::RaydiumClmm {
                    if let PoolMath::Concentrated { tick_arrays, .. } = &pool.math {
                        if tick_arrays.is_empty() {
                            continue; // Not dead, just tick arrays not loaded
                        }
                    }
                }
                dead_pools.push(i as u32);
            }
        }

        if !dead_pools.is_empty() {
            for &pi in &dead_pools {
                self.graph.remove_pool_edges(pi);
            }

            // Remove routes that reference any dead pool
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

        // --- Clear dedup map each full scan (allow re-evaluation) ---
        self.recent_emissions.clear();

        // --- Evaluate remaining routes ---
        let mut opportunities = 0;
        let mut probe_passed = 0;

        let routes: Vec<Route> = self.route_table.routes.clone();

        for route in &routes {
            // Quick probe inline to count stats
            let probe_profit = optimizer::simulate_route_profit(
                route,
                &self.graph,
                self.config.probe_amount_lamports,
            );
            if probe_profit > 0 {
                probe_passed += 1;
            }
            if self.evaluate_route(route, 0) {
                opportunities += 1;
            }
        }

        // Count pools by type and CLMM readiness
        let mut clmm_ready = 0usize;
        let mut clmm_not_ready = 0usize;
        for pool in &self.graph.pools {
            if pool.dex_type == DexType::RaydiumClmm {
                if let PoolMath::Concentrated { tick_arrays, .. } = &pool.math {
                    if tick_arrays.is_empty() { clmm_not_ready += 1; } else { clmm_ready += 1; }
                }
            }
        }

        let elapsed = start.elapsed();
        info!(
            "Full scan: {} routes, {} probe+, {} opps in {:?} | CLMM: {}/{} ready | pools: {}",
            routes.len(),
            probe_passed,
            opportunities,
            elapsed,
            clmm_ready,
            clmm_ready + clmm_not_ready,
            self.graph.pool_count(),
        );
    }

    /// Evaluate a single route. Returns true if an Opportunity was emitted.
    fn evaluate_route(&mut self, route: &Route, slot: u64) -> bool {
        // Quick probe
        let probe_profit = optimizer::simulate_route_profit(
            route,
            &self.graph,
            self.config.probe_amount_lamports,
        );
        if probe_profit <= 0 {
            return false;
        }

        // Ternary search for optimal amount
        let Some((amount_in, profit)) = optimizer::find_optimal_amount(
            route,
            &self.graph,
            self.config.max_input_lamports,
            self.config.ternary_iterations,
        ) else {
            return false;
        };

        if profit < self.config.min_profit_lamports {
            return false;
        }

        // --- Dedup: skip if same route was recently emitted with >= profit ---
        let key = route_key(route);
        if let Some(&(prev_slot, prev_profit)) = self.recent_emissions.get(&key) {
            // Same slot or next slot: only re-emit if profit increased by >10%
            if slot <= prev_slot + 1 && profit <= prev_profit + prev_profit / 10 {
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
                // Pool not ready (e.g. CLMM without tick arrays loaded)
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

        // CLMM: use tick_array PDA from pre-loaded tick arrays (known to exist on-chain).
        // If no tick arrays loaded yet, return empty vec to signal this pool isn't ready.
        if pool.dex_type == DexType::RaydiumClmm {
            if let PoolMath::Concentrated { tick_current, tick_spacing, tick_arrays, .. } = &pool.math {
                if tick_arrays.is_empty() {
                    // No tick arrays loaded — pool not ready for CLMM swap
                    return vec![];
                }
                let current_start = raydium_clmm::tick_array_start_index(*tick_current, *tick_spacing);
                // Prefer the tick array containing tick_current; fallback to closest
                let best_ta = tick_arrays.iter()
                    .find(|ta| ta.start_tick_index == current_start)
                    .or_else(|| tick_arrays.iter().min_by_key(|ta| {
                        (ta.start_tick_index - current_start).unsigned_abs()
                    }));
                if let Some(ta) = best_ta {
                    if let Some(pda) = raydium_clmm::tick_array_pda(&pool.address, ta.start_tick_index) {
                        accounts.push(pda);
                    }
                }
            }
        }

        accounts
    }
}
