use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, PoolMath, PoolState};
use crate::streaming::event_parser::protocols::bonk::events::BonkPoolStateAccountEvent;
use crate::streaming::event_parser::protocols::bonk::types::{
    pool_state_decode, PoolState as BonkPoolStateData, POOL_STATE_SIZE,
};

/// Decode from a parsed account event
pub fn decode(event: &BonkPoolStateAccountEvent) -> Option<PoolState> {
    let ps = &event.pool_state;
    Some(from_pool_state(&event.pubkey, ps, event.metadata.slot))
}

/// Decode from raw account bytes (skip 8-byte discriminator)
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < POOL_STATE_SIZE + 8 {
        return None;
    }
    let ps = pool_state_decode(&data[8..POOL_STATE_SIZE + 8])?;
    Some(from_pool_state(address, &ps, 0))
}

fn from_pool_state(address: &Pubkey, ps: &BonkPoolStateData, slot: u64) -> PoolState {
    PoolState {
        address: *address,
        dex_type: DexType::Bonk,
        mint_a: ps.base_mint,
        mint_b: ps.quote_mint,
        vault_a: Some(ps.base_vault),
        vault_b: Some(ps.quote_vault),
        mint_a_is_2022: false,
        mint_b_is_2022: false,
        extra_accounts: vec![ps.global_config, ps.platform_config],
        math: PoolMath::ConstantProduct {
            reserve_a: ps.virtual_base,
            reserve_b: ps.virtual_quote,
            fee_numerator: 100,
            fee_denominator: 10000,
        },
        last_updated_slot: slot,
    }
}
