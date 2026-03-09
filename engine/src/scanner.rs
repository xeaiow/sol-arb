use std::time::Instant;
use log::{info, debug};
use tokio::sync::mpsc;

use solana_streamer_sdk::pool::state::PoolUpdate;

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

/// The main scanner that ties everything together
pub struct Scanner {
    config: EngineConfig,
    graph: TokenGraph,
    route_table: RouteTable,
    phase: Phase,
    update_rx: mpsc::Receiver<PoolUpdate>,
    opportunity_tx: mpsc::Sender<Opportunity>,
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
            vault_a: None,
            vault_b: None,
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
                    self.route_table.add_routes_for_pool(
                        &self.graph,
                        &self.config,
                        pool_index,
                    );
                }

                // Incremental scan: only routes affected by this pool
                self.scan_routes_for_pool(pool_index, update.slot);
            }
        }
    }

    /// Scan routes affected by a specific pool update
    fn scan_routes_for_pool(&self, pool_index: u32, slot: u64) {
        let route_indices = self.route_table.index.routes_for_pool(pool_index);

        for &route_idx in route_indices {
            let route = &self.route_table.routes[route_idx as usize];
            self.evaluate_route(route, slot);
        }
    }

    /// Full scan: evaluate all routes
    fn full_scan(&self) {
        let start = Instant::now();
        let mut opportunities = 0;

        for route in &self.route_table.routes {
            if self.evaluate_route(route, 0) {
                opportunities += 1;
            }
        }

        let elapsed = start.elapsed();
        debug!(
            "Full scan: {} routes in {:?}, {} opportunities",
            self.route_table.route_count(),
            elapsed,
            opportunities,
        );
    }

    /// Evaluate a single route. Returns true if an Opportunity was emitted.
    fn evaluate_route(&self, route: &Route, slot: u64) -> bool {
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

        // Build opportunity with pool snapshots
        let mut pool_snapshots = Vec::with_capacity(route.hops.len());
        let mut max_slot = slot;

        for hop in &route.hops {
            let pool = &self.graph.pools[hop.pool_index as usize];
            if pool.last_updated_slot > max_slot {
                max_slot = pool.last_updated_slot;
            }
            pool_snapshots.push(PoolSnapshot {
                address: pool.address,
                dex_type: pool.dex_type,
                mint_a: pool.mint_a,
                mint_b: pool.mint_b,
                is_a_to_b: hop.is_a_to_b,
                accounts: vec![
                    pool.address,
                    pool.mint_a,
                    pool.mint_b,
                ],
            });
        }

        let opportunity = Opportunity {
            route: route.clone(),
            amount_in,
            expected_profit: profit,
            pool_snapshots,
            slot: max_slot,
        };

        let _ = self.opportunity_tx.try_send(opportunity);
        true
    }
}
