use arrayvec::ArrayVec;
use solana_sdk::pubkey::Pubkey;
use solana_streamer_sdk::pool::state::DexType;

#[derive(Debug, Clone, Copy)]
pub struct Hop {
    pub pool_index: u32,
    pub is_a_to_b: bool,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub hops: ArrayVec<Hop, 4>,
    pub base_mint: Pubkey,
}

#[derive(Debug, Clone)]
pub struct PoolSnapshot {
    pub address: Pubkey,
    pub dex_type: DexType,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub is_a_to_b: bool,
    pub accounts: Vec<Pubkey>,
}

#[derive(Debug, Clone)]
pub struct Opportunity {
    pub route: Route,
    pub amount_in: u64,
    pub expected_profit: u64,
    pub pool_snapshots: Vec<PoolSnapshot>,
    pub slot: u64,
}
