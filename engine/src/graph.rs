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
