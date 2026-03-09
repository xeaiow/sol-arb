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
        let Some(&_node_a) = graph.mint_to_index.get(&pool.mint_a) else { return };
        let Some(&_node_b) = graph.mint_to_index.get(&pool.mint_b) else { return };

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
