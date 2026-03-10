use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, PoolMath, PoolState};
use crate::streaming::event_parser::protocols::raydium_cpmm::events::RaydiumCpmmPoolStateAccountEvent;
use crate::streaming::event_parser::protocols::raydium_cpmm::types::{
    pool_state_decode, PoolState as CpmmPoolState, POOL_STATE_SIZE,
};

/// Decode from a parsed account event
pub fn decode(event: &RaydiumCpmmPoolStateAccountEvent) -> Option<PoolState> {
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

const TOKEN_2022_PROGRAM_ID: Pubkey =
    solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

fn from_pool_state(address: &Pubkey, ps: &CpmmPoolState, slot: u64) -> PoolState {
    PoolState {
        address: *address,
        dex_type: DexType::RaydiumCpmm,
        mint_a: ps.token_0_mint,
        mint_b: ps.token_1_mint,
        vault_a: Some(ps.token_0_vault),
        vault_b: Some(ps.token_1_vault),
        mint_a_is_2022: ps.token_0_program == TOKEN_2022_PROGRAM_ID,
        mint_b_is_2022: ps.token_1_program == TOKEN_2022_PROGRAM_ID,
        extra_accounts: vec![ps.amm_config, ps.observation_key],
        math: PoolMath::ConstantProduct {
            reserve_a: 0,
            reserve_b: 0,
            fee_numerator: 25,
            fee_denominator: 10000,
        },
        last_updated_slot: slot,
    }
}
