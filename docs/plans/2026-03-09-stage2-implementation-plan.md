# Stage 2: Route Engine Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build a route engine that consumes PoolUpdate from Stage 1, maintains a token graph, pre-builds 2/3/4-hop cycle routes from 3 base tokens, scans for arbitrage opportunities, and emits Opportunity events.

**Architecture:** Independent `engine/` crate that depends on `solana-streamer-sdk`. Single-thread scanner holds token graph + route table. Batch build after warmup (30s or 1000 pools), then incremental updates + periodic full scan every 5 seconds. Ternary search for optimal input amount.

**Tech Stack:** Rust, solana-streamer-sdk (path dep), solana-sdk 3.0.0, arrayvec, tokio 1.50.0

---

## Task 1: Create engine crate with config and core types

**Files:**
- Create: `engine/Cargo.toml`
- Create: `engine/src/lib.rs`
- Create: `engine/src/config.rs`
- Create: `engine/src/opportunity.rs`

**Step 1: Create Cargo.toml**

Create `engine/Cargo.toml`:
```toml
[package]
name = "arb-engine"
version = "0.1.0"
edition = "2021"

[dependencies]
solana-streamer-sdk = { path = "../solana-streamer" }
solana-sdk = "3.0.0"
arrayvec = "0.7"
tokio = { version = "1.50.0", features = ["full"] }
log = "0.4"
```

**Step 2: Create config.rs**

Create `engine/src/config.rs`:
```rust
use solana_sdk::pubkey::Pubkey;

/// Well-known base token mints
pub const WSOL_MINT: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
pub const USDC_MINT: Pubkey = solana_sdk::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USD1_MINT: Pubkey = solana_sdk::pubkey!("Ue2JCRrno2aAWT7gmjEwpsK5HQGM1ZMqSDZvMNo3S5e");

pub struct EngineConfig {
    /// Base tokens to build routes from (cycles must start and end at a base)
    pub base_mints: Vec<Pubkey>,
    /// Maximum hop count for routes (2, 3, or 4)
    pub max_hops: u8,
    /// Warmup duration in seconds before batch route build
    pub warmup_secs: u64,
    /// Warmup pool count threshold (build routes when reached)
    pub warmup_pool_count: usize,
    /// Full scan interval in seconds
    pub full_scan_interval_secs: u64,
    /// Minimum pool reserve in lamports to include in routes (default 10 SOL)
    pub min_reserve_lamports: u64,
    /// Maximum single-hop fee percentage (0.0 - 1.0)
    pub max_hop_fee: f64,
    /// Minimum profit in lamports to emit an Opportunity
    pub min_profit_lamports: u64,
    /// Upper bound for ternary search input amount (lamports)
    pub max_input_lamports: u64,
    /// Number of ternary search iterations
    pub ternary_iterations: u32,
    /// Probe amount for quick profit check before ternary search (lamports)
    pub probe_amount_lamports: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            base_mints: vec![WSOL_MINT, USDC_MINT, USD1_MINT],
            max_hops: 4,
            warmup_secs: 30,
            warmup_pool_count: 1000,
            full_scan_interval_secs: 5,
            min_reserve_lamports: 10_000_000_000, // 10 SOL
            max_hop_fee: 0.02,
            min_profit_lamports: 1_000_000, // 0.001 SOL
            max_input_lamports: 100_000_000_000, // 100 SOL
            ternary_iterations: 10,
            probe_amount_lamports: 1_000_000_000, // 1 SOL
        }
    }
}
```

**Step 3: Create opportunity.rs**

Create `engine/src/opportunity.rs`:
```rust
use arrayvec::ArrayVec;
use solana_sdk::pubkey::Pubkey;
use solana_streamer_sdk::pool::state::DexType;

/// A single hop in an arbitrage route
#[derive(Debug, Clone, Copy)]
pub struct Hop {
    pub pool_index: u32,
    pub is_a_to_b: bool,
}

/// An arbitrage cycle route
#[derive(Debug, Clone)]
pub struct Route {
    pub hops: ArrayVec<Hop, 4>,
    pub base_mint: Pubkey,
}

/// Snapshot of a pool's state at the time the opportunity was found
#[derive(Debug, Clone)]
pub struct PoolSnapshot {
    pub address: Pubkey,
    pub dex_type: DexType,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub is_a_to_b: bool,
    pub accounts: Vec<Pubkey>,
}

/// A profitable arbitrage opportunity — everything Executor needs
#[derive(Debug, Clone)]
pub struct Opportunity {
    pub route: Route,
    pub amount_in: u64,
    pub expected_profit: u64,
    pub pool_snapshots: Vec<PoolSnapshot>,
    pub slot: u64,
}
```

**Step 4: Create lib.rs**

Create `engine/src/lib.rs`:
```rust
pub mod config;
pub mod opportunity;
```

**Step 5: Verify it compiles**

Run: `cd engine && cargo check`
Expected: compiles successfully.

**Step 6: Commit**

```bash
git add engine/
git commit -m "feat(engine): add crate skeleton with config and opportunity types"
```

---

## Task 2: Build TokenGraph

**Files:**
- Create: `engine/src/graph.rs`
- Modify: `engine/src/lib.rs`

**Step 1: Create graph.rs**

Create `engine/src/graph.rs`:
```rust
use std::collections::HashMap;
use solana_sdk::pubkey::Pubkey;
use solana_streamer_sdk::pool::state::{PoolMath, PoolState};

/// An edge in the token graph (one direction of a pool)
#[derive(Debug, Clone, Copy)]
pub struct Edge {
    pub target: u32,       // target node index
    pub pool_index: u32,   // index into PoolVec
    pub is_a_to_b: bool,
}

/// Stored pool data for the engine (indexed by pool_index)
#[derive(Debug, Clone)]
pub struct PoolEntry {
    pub address: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub math: PoolMath,
    pub dex_type: solana_streamer_sdk::pool::state::DexType,
    pub vault_a: Option<Pubkey>,
    pub vault_b: Option<Pubkey>,
    pub last_updated_slot: u64,
}

impl PoolEntry {
    pub fn from_pool_state(state: &PoolState) -> Self {
        Self {
            address: state.address,
            mint_a: state.mint_a,
            mint_b: state.mint_b,
            math: state.math.clone(),
            dex_type: state.dex_type,
            vault_a: state.vault_a,
            vault_b: state.vault_b,
            last_updated_slot: state.last_updated_slot,
        }
    }
}

/// Token directed graph: mint = node, pool = edge (bidirectional)
pub struct TokenGraph {
    /// mint -> node index
    pub mint_to_index: HashMap<Pubkey, u32>,
    /// node index -> mint
    pub index_to_mint: Vec<Pubkey>,
    /// Adjacency list: node_index -> Vec<Edge>
    pub adjacency: Vec<Vec<Edge>>,
    /// Pool storage indexed by pool_index
    pub pools: Vec<PoolEntry>,
    /// pool address -> pool_index
    pub address_to_pool: HashMap<Pubkey, u32>,
}

impl TokenGraph {
    pub fn new() -> Self {
        Self {
            mint_to_index: HashMap::new(),
            index_to_mint: Vec::new(),
            adjacency: Vec::new(),
            pools: Vec::new(),
            address_to_pool: HashMap::new(),
        }
    }

    /// Get or create a node index for a mint
    pub fn get_or_insert_mint(&mut self, mint: &Pubkey) -> u32 {
        if let Some(&idx) = self.mint_to_index.get(mint) {
            return idx;
        }
        let idx = self.index_to_mint.len() as u32;
        self.mint_to_index.insert(*mint, idx);
        self.index_to_mint.push(*mint);
        self.adjacency.push(Vec::new());
        idx
    }

    /// Add a pool to the graph. Returns the pool_index.
    /// Adds two directed edges (a->b and b->a).
    pub fn add_pool(&mut self, entry: PoolEntry) -> u32 {
        // Skip if already added
        if let Some(&idx) = self.address_to_pool.get(&entry.address) {
            // Update math
            self.pools[idx as usize].math = entry.math;
            self.pools[idx as usize].last_updated_slot = entry.last_updated_slot;
            return idx;
        }

        let pool_index = self.pools.len() as u32;
        let node_a = self.get_or_insert_mint(&entry.mint_a);
        let node_b = self.get_or_insert_mint(&entry.mint_b);

        // Edge: a -> b
        self.adjacency[node_a as usize].push(Edge {
            target: node_b,
            pool_index,
            is_a_to_b: true,
        });

        // Edge: b -> a
        self.adjacency[node_b as usize].push(Edge {
            target: node_a,
            pool_index,
            is_a_to_b: false,
        });

        self.address_to_pool.insert(entry.address, pool_index);
        self.pools.push(entry);
        pool_index
    }

    /// Update a pool's math by address. Returns pool_index if found.
    pub fn update_pool_math(&mut self, address: &Pubkey, math: PoolMath, slot: u64) -> Option<u32> {
        let &idx = self.address_to_pool.get(address)?;
        self.pools[idx as usize].math = math;
        self.pools[idx as usize].last_updated_slot = slot;
        Some(idx)
    }

    /// Check if a pool passes the minimum reserve threshold
    pub fn pool_has_min_reserve(&self, pool_index: u32, min_lamports: u64) -> bool {
        let pool = &self.pools[pool_index as usize];
        match &pool.math {
            PoolMath::ConstantProduct { reserve_a, reserve_b, .. } => {
                *reserve_a >= min_lamports || *reserve_b >= min_lamports
            }
            PoolMath::BondingCurve { virtual_sol_reserves, .. } => {
                *virtual_sol_reserves >= min_lamports
            }
            PoolMath::Concentrated { liquidity, .. } => {
                *liquidity > 0
            }
        }
    }

    /// Get the fee ratio for a pool (0.0 - 1.0)
    pub fn pool_fee_ratio(&self, pool_index: u32) -> f64 {
        let pool = &self.pools[pool_index as usize];
        match &pool.math {
            PoolMath::ConstantProduct { fee_numerator, fee_denominator, .. } => {
                if *fee_denominator == 0 { return 1.0; }
                *fee_numerator as f64 / *fee_denominator as f64
            }
            PoolMath::BondingCurve { .. } => 0.01, // PumpFun 1%
            PoolMath::Concentrated { fee_rate, .. } => {
                *fee_rate as f64 / 1_000_000.0
            }
        }
    }

    pub fn pool_count(&self) -> usize {
        self.pools.len()
    }

    pub fn mint_count(&self) -> usize {
        self.index_to_mint.len()
    }
}
```

**Step 2: Update lib.rs**

Modify `engine/src/lib.rs`:
```rust
pub mod config;
pub mod graph;
pub mod opportunity;
```

**Step 3: Verify it compiles**

Run: `cd engine && cargo check`
Expected: compiles.

**Step 4: Commit**

```bash
git add engine/src/graph.rs engine/src/lib.rs
git commit -m "feat(engine): add TokenGraph with mint nodes, pool edges, and math updates"
```

---

## Task 3: Build RouteTable with batch DFS

**Files:**
- Create: `engine/src/route.rs`
- Modify: `engine/src/lib.rs`

**Step 1: Create route.rs**

Create `engine/src/route.rs`:
```rust
use std::collections::HashSet;
use arrayvec::ArrayVec;
use solana_sdk::pubkey::Pubkey;
use log::info;

use crate::config::EngineConfig;
use crate::graph::TokenGraph;
use crate::opportunity::{Hop, Route};

/// Inverted index: pool_index -> Vec<route_index>
pub struct RouteIndex {
    pub pool_to_routes: Vec<Vec<u32>>,
}

impl RouteIndex {
    pub fn new(pool_count: usize) -> Self {
        Self {
            pool_to_routes: vec![Vec::new(); pool_count],
        }
    }

    /// Grow the index to accommodate new pools
    pub fn ensure_capacity(&mut self, pool_count: usize) {
        if pool_count > self.pool_to_routes.len() {
            self.pool_to_routes.resize(pool_count, Vec::new());
        }
    }

    pub fn register_route(&mut self, route_index: u32, route: &Route) {
        for hop in &route.hops {
            let pi = hop.pool_index as usize;
            if pi < self.pool_to_routes.len() {
                self.pool_to_routes[pi].push(route_index);
            }
        }
    }

    /// Get route indices affected by a pool update
    pub fn routes_for_pool(&self, pool_index: u32) -> &[u32] {
        let pi = pool_index as usize;
        if pi < self.pool_to_routes.len() {
            &self.pool_to_routes[pi]
        } else {
            &[]
        }
    }
}

/// Route table: stores all discovered routes and inverted index
pub struct RouteTable {
    pub routes: Vec<Route>,
    pub index: RouteIndex,
}

impl RouteTable {
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            index: RouteIndex::new(0),
        }
    }

    /// Batch build all routes from base mints via DFS.
    /// Called once after warmup.
    pub fn build_from_graph(&mut self, graph: &TokenGraph, config: &EngineConfig) {
        self.routes.clear();

        for base_mint in &config.base_mints {
            let Some(&base_node) = graph.mint_to_index.get(base_mint) else {
                continue;
            };

            let mut path: ArrayVec<Hop, 4> = ArrayVec::new();
            let mut visited_pools: HashSet<u32> = HashSet::new();

            self.dfs(
                graph,
                config,
                base_node,
                base_node,
                *base_mint,
                &mut path,
                &mut visited_pools,
            );
        }

        // Build inverted index
        self.index = RouteIndex::new(graph.pool_count());
        for (i, route) in self.routes.iter().enumerate() {
            self.index.register_route(i as u32, route);
        }

        info!(
            "Route table built: {} routes from {} pools, {} mints",
            self.routes.len(),
            graph.pool_count(),
            graph.mint_count(),
        );
    }

    fn dfs(
        &mut self,
        graph: &TokenGraph,
        config: &EngineConfig,
        current_node: u32,
        base_node: u32,
        base_mint: Pubkey,
        path: &mut ArrayVec<Hop, 4>,
        visited_pools: &mut HashSet<u32>,
    ) {
        let depth = path.len();

        // If we have at least 2 hops and we're back at base, record route
        if depth >= 2 && current_node == base_node {
            self.routes.push(Route {
                hops: path.clone(),
                base_mint,
            });
            return; // Don't continue DFS from base (would create longer cycles)
        }

        // Max depth reached
        if depth >= config.max_hops as usize {
            return;
        }

        for edge in &graph.adjacency[current_node as usize] {
            // Don't reuse same pool in a route
            if visited_pools.contains(&edge.pool_index) {
                continue;
            }

            // Pruning: min reserve
            if !graph.pool_has_min_reserve(edge.pool_index, config.min_reserve_lamports) {
                continue;
            }

            // Pruning: max fee
            if graph.pool_fee_ratio(edge.pool_index) > config.max_hop_fee {
                continue;
            }

            // Don't revisit base except to close the cycle
            if edge.target == base_node && depth < 1 {
                continue;
            }

            visited_pools.insert(edge.pool_index);
            path.push(Hop {
                pool_index: edge.pool_index,
                is_a_to_b: edge.is_a_to_b,
            });

            self.dfs(graph, config, edge.target, base_node, base_mint, path, visited_pools);

            path.pop();
            visited_pools.remove(&edge.pool_index);
        }
    }

    /// Incremental: add routes involving a newly added pool.
    /// Does a local DFS from the pool's two mint nodes.
    pub fn add_routes_for_pool(
        &mut self,
        graph: &TokenGraph,
        config: &EngineConfig,
        pool_index: u32,
    ) {
        let pool = &graph.pools[pool_index as usize];
        let Some(&node_a) = graph.mint_to_index.get(&pool.mint_a) else { return };
        let Some(&node_b) = graph.mint_to_index.get(&pool.mint_b) else { return };

        let routes_before = self.routes.len();

        // For each base mint, try to find new cycles through node_a and node_b
        for base_mint in &config.base_mints {
            let Some(&base_node) = graph.mint_to_index.get(base_mint) else {
                continue;
            };

            // DFS from base, but only explore paths that include the new pool
            let mut path: ArrayVec<Hop, 4> = ArrayVec::new();
            let mut visited_pools: HashSet<u32> = HashSet::new();

            self.dfs(graph, config, base_node, base_node, *base_mint, &mut path, &mut visited_pools);
        }

        // Deduplicate: only keep routes that actually use the new pool
        // and weren't already in the table
        let new_routes: Vec<Route> = self.routes[routes_before..]
            .iter()
            .filter(|r| r.hops.iter().any(|h| h.pool_index == pool_index))
            .cloned()
            .collect();

        self.routes.truncate(routes_before);

        // Add only genuinely new routes
        self.index.ensure_capacity(graph.pool_count());
        for route in new_routes {
            let route_idx = self.routes.len() as u32;
            self.index.register_route(route_idx, &route);
            self.routes.push(route);
        }
    }

    pub fn route_count(&self) -> usize {
        self.routes.len()
    }
}
```

**Step 2: Update lib.rs**

Add `pub mod route;` to `engine/src/lib.rs`.

**Step 3: Verify it compiles**

Run: `cd engine && cargo check`

**Step 4: Commit**

```bash
git add engine/src/route.rs engine/src/lib.rs
git commit -m "feat(engine): add RouteTable with DFS cycle finder, pruning, and inverted index"
```

---

## Task 4: Build optimizer (ternary search)

**Files:**
- Create: `engine/src/optimizer.rs`
- Modify: `engine/src/lib.rs`

**Step 1: Create optimizer.rs**

Create `engine/src/optimizer.rs`:
```rust
use crate::graph::TokenGraph;
use crate::opportunity::Route;

/// Simulate a route's profit for a given input amount.
/// Returns (amount_out - amount_in) as i64.
pub fn simulate_route_profit(
    route: &Route,
    graph: &TokenGraph,
    amount_in: u64,
) -> i64 {
    let mut current = amount_in;
    for hop in &route.hops {
        let pool = &graph.pools[hop.pool_index as usize];
        current = pool.math.get_amount_out(current, hop.is_a_to_b);
        if current == 0 {
            return i64::MIN;
        }
    }
    current as i64 - amount_in as i64
}

/// Ternary search for the optimal input amount that maximizes profit.
/// Returns Some((optimal_amount_in, expected_profit)) if profitable.
pub fn find_optimal_amount(
    route: &Route,
    graph: &TokenGraph,
    max_amount: u64,
    iterations: u32,
) -> Option<(u64, u64)> {
    let mut lo: u64 = 10_000; // 0.00001 SOL
    let mut hi: u64 = max_amount;

    if lo >= hi {
        return None;
    }

    for _ in 0..iterations {
        if hi - lo < 3 {
            break;
        }
        let m1 = lo + (hi - lo) / 3;
        let m2 = hi - (hi - lo) / 3;

        let p1 = simulate_route_profit(route, graph, m1);
        let p2 = simulate_route_profit(route, graph, m2);

        if p1 < p2 {
            lo = m1;
        } else {
            hi = m2;
        }
    }

    let optimal = (lo + hi) / 2;
    let profit = simulate_route_profit(route, graph, optimal);

    if profit > 0 {
        Some((optimal, profit as u64))
    } else {
        None
    }
}
```

**Step 2: Update lib.rs**

Add `pub mod optimizer;` to `engine/src/lib.rs`.

**Step 3: Verify it compiles**

Run: `cd engine && cargo check`

**Step 4: Commit**

```bash
git add engine/src/optimizer.rs engine/src/lib.rs
git commit -m "feat(engine): add ternary search optimizer for optimal input amount"
```

---

## Task 5: Build Scanner main loop

**Files:**
- Create: `engine/src/scanner.rs`
- Modify: `engine/src/lib.rs`

**Step 1: Create scanner.rs**

Create `engine/src/scanner.rs`:
```rust
use std::time::Instant;
use log::{info, debug};
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;

use solana_streamer_sdk::pool::state::{PoolMath, PoolUpdate};

use crate::config::EngineConfig;
use crate::graph::{PoolEntry, TokenGraph};
use crate::opportunity::{Opportunity, PoolSnapshot};
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
    fn evaluate_route(&self, route: &crate::opportunity::Route, slot: u64) -> bool {
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
```

**Step 2: Update lib.rs**

Add `pub mod scanner;` to `engine/src/lib.rs`.

**Step 3: Verify it compiles**

Run: `cd engine && cargo check`

**Step 4: Commit**

```bash
git add engine/src/scanner.rs engine/src/lib.rs
git commit -m "feat(engine): add Scanner with warmup, incremental scan, and full scan"
```

---

## Task 6: Build Engine public API and example

**Files:**
- Create: `engine/src/engine.rs`
- Modify: `engine/src/lib.rs`
- Modify: `engine/Cargo.toml` (add `env_logger` for example)
- Create: `engine/examples/engine_example.rs`

**Step 1: Create engine.rs**

Create `engine/src/engine.rs`:
```rust
use tokio::sync::mpsc;

use solana_streamer_sdk::pool::state::PoolUpdate;

use crate::config::EngineConfig;
use crate::opportunity::Opportunity;
use crate::scanner::Scanner;

/// Public entry point for the arbitrage engine.
pub struct Engine {
    scanner: Scanner,
}

impl Engine {
    /// Create a new Engine.
    /// Returns the Engine and a Receiver for Opportunity events.
    pub fn new(
        config: EngineConfig,
        update_rx: mpsc::Receiver<PoolUpdate>,
    ) -> (Self, mpsc::Receiver<Opportunity>) {
        let (opp_tx, opp_rx) = mpsc::channel(256);
        let scanner = Scanner::new(config, update_rx, opp_tx);
        (Self { scanner }, opp_rx)
    }

    /// Run the engine (blocks forever).
    pub async fn run(mut self) {
        self.scanner.run().await;
    }
}
```

**Step 2: Create example**

Create `engine/examples/engine_example.rs`:
```rust
use std::sync::Arc;
use arb_engine::config::EngineConfig;
use arb_engine::engine::Engine;

use solana_streamer_sdk::pool::streamer::{PoolStreamer, PoolStreamerConfig};
use solana_streamer_sdk::streaming::event_parser::protocols::{
    bonk::parser::BONK_PROGRAM_ID, meteora_damm_v2::parser::METEORA_DAMM_V2_PROGRAM_ID,
    pumpfun::parser::PUMPFUN_PROGRAM_ID, pumpswap::parser::PUMPSWAP_PROGRAM_ID,
    raydium_amm_v4::parser::RAYDIUM_AMM_V4_PROGRAM_ID,
    raydium_clmm::parser::RAYDIUM_CLMM_PROGRAM_ID,
    raydium_cpmm::parser::RAYDIUM_CPMM_PROGRAM_ID,
};
use solana_streamer_sdk::streaming::event_parser::Protocol;
use solana_streamer_sdk::streaming::yellowstone_grpc::{
    AccountFilter, TransactionFilter, YellowstoneGrpc,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let grpc_endpoint = std::env::var("GRPC_ENDPOINT")
        .unwrap_or_else(|_| "https://solana-yellowstone-grpc.publicnode.com:443".to_string());
    let grpc_token = std::env::var("GRPC_TOKEN").ok();
    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

    // Stage 1: Pool Streamer
    let pool_config = PoolStreamerConfig {
        rpc_url,
        update_channel_size: 4096,
    };
    let (pool_streamer, update_rx) = PoolStreamer::new(pool_config);
    let pool_streamer = Arc::new(pool_streamer);

    // Stage 2: Route Engine
    let engine_config = EngineConfig::default();
    let (engine, mut opp_rx) = Engine::new(engine_config, update_rx);

    // Spawn opportunity consumer
    tokio::spawn(async move {
        while let Some(opp) = opp_rx.recv().await {
            println!(
                "[slot {}] OPPORTUNITY: {} hops, amount_in={}, profit={} lamports",
                opp.slot,
                opp.route.hops.len(),
                opp.amount_in,
                opp.expected_profit,
            );
            for (i, snap) in opp.pool_snapshots.iter().enumerate() {
                println!(
                    "  hop {}: {} ({:?}) {}",
                    i + 1,
                    snap.address,
                    snap.dex_type,
                    if snap.is_a_to_b { "A->B" } else { "B->A" },
                );
            }
        }
    });

    // Spawn engine
    tokio::spawn(async move {
        engine.run().await;
    });

    // Start gRPC subscription
    let grpc = Arc::new(YellowstoneGrpc::new(grpc_endpoint, grpc_token)?);

    let protocols = vec![
        Protocol::PumpFun,
        Protocol::PumpSwap,
        Protocol::Bonk,
        Protocol::RaydiumCpmm,
        Protocol::RaydiumClmm,
        Protocol::RaydiumAmmV4,
        Protocol::MeteoraDammV2,
    ];

    let account_include = vec![
        PUMPFUN_PROGRAM_ID.to_string(),
        PUMPSWAP_PROGRAM_ID.to_string(),
        BONK_PROGRAM_ID.to_string(),
        RAYDIUM_CPMM_PROGRAM_ID.to_string(),
        RAYDIUM_CLMM_PROGRAM_ID.to_string(),
        RAYDIUM_AMM_V4_PROGRAM_ID.to_string(),
        METEORA_DAMM_V2_PROGRAM_ID.to_string(),
    ];

    let transaction_filter = TransactionFilter {
        account_include: account_include.clone(),
        account_exclude: vec![],
        account_required: vec![],
    };

    let account_filter = AccountFilter {
        account: vec![],
        owner: account_include,
        filters: vec![],
    };

    let streamer = pool_streamer.clone();
    grpc.subscribe_events_immediate(
        protocols,
        None,
        vec![transaction_filter],
        vec![account_filter],
        None,
        None,
        move |event| {
            let streamer = streamer.clone();
            tokio::spawn(async move {
                streamer.on_event(event).await;
            });
        },
    )
    .await?;

    println!("Engine running. Warming up...");
    tokio::signal::ctrl_c().await?;
    println!("Shutting down.");

    Ok(())
}
```

**Step 3: Update Cargo.toml for example deps**

Add to `engine/Cargo.toml` under `[dependencies]`:
```toml
env_logger = "0.11"
anyhow = "1.0"
```

**Step 4: Update lib.rs**

Add `pub mod engine;` to `engine/src/lib.rs`.

**Step 5: Verify it compiles**

Run: `cd engine && cargo check && cargo check --example engine_example`

**Step 6: Commit**

```bash
git add engine/
git commit -m "feat(engine): add Engine public API and full pipeline example"
```

---

## Task 7: Fix compilation and verify

**Files:**
- Any files that need fixes from previous tasks

**Step 1: Run full compilation**

Run: `cd engine && cargo check --all-targets 2>&1`

Fix any errors (field name mismatches, missing imports, type mismatches).

**Step 2: Verify solana-streamer still compiles**

Run: `cd solana-streamer && cargo check --all-targets 2>&1`

**Step 3: Commit fixes**

```bash
git commit -am "fix(engine): fix compilation issues"
```
