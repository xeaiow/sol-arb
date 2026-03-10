use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, PoolMath, PoolState};
use crate::streaming::event_parser::protocols::pumpswap::events::PumpSwapPoolAccountEvent;
use crate::streaming::event_parser::protocols::pumpswap::types::{
    pool_decode, Pool, POOL_SIZE,
};

/// Decode from a parsed account event
pub fn decode(event: &PumpSwapPoolAccountEvent) -> Option<PoolState> {
    let pool = &event.pool;
    Some(from_pool(&event.pubkey, pool, event.metadata.slot))
}

/// Decode from raw account bytes (skip 8-byte discriminator)
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < POOL_SIZE + 8 {
        return None;
    }
    let pool = pool_decode(&data[8..POOL_SIZE + 8])?;
    Some(from_pool(address, &pool, 0))
}

fn from_pool(address: &Pubkey, pool: &Pool, slot: u64) -> PoolState {
    PoolState {
        address: *address,
        dex_type: DexType::PumpSwap,
        mint_a: pool.base_mint,
        mint_b: pool.quote_mint,
        vault_a: Some(pool.pool_base_token_account),
        vault_b: Some(pool.pool_quote_token_account),
        mint_a_is_2022: false,
        mint_b_is_2022: false,
        extra_accounts: vec![pool.coin_creator],
        math: PoolMath::ConstantProduct {
            reserve_a: 0,
            reserve_b: 0,
            fee_numerator: 25,
            fee_denominator: 10000,
        },
        last_updated_slot: slot,
    }
}
