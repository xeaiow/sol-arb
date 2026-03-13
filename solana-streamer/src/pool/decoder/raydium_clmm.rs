use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, PoolMath, PoolState, Tick, TickArray};
use crate::streaming::event_parser::protocols::raydium_clmm::events::RaydiumClmmPoolStateAccountEvent;
use crate::streaming::event_parser::protocols::raydium_clmm::types::{
    pool_state_decode, tick_array_state_decode, AmmConfig,
    PoolState as ClmmPoolState, TickArrayState, POOL_STATE_SIZE, TICK_ARRAY_STATE_SIZE,
};

/// Raydium CLMM program ID
const RAYDIUM_CLMM_PROGRAM: Pubkey =
    solana_sdk::pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");

/// Decode from a parsed account event
pub fn decode(event: &RaydiumClmmPoolStateAccountEvent) -> Option<PoolState> {
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

fn from_pool_state(address: &Pubkey, ps: &ClmmPoolState, slot: u64) -> PoolState {
    PoolState {
        address: *address,
        dex_type: DexType::RaydiumClmm,
        mint_a: ps.token_mint0,
        mint_b: ps.token_mint1,
        vault_a: Some(ps.token_vault0),
        vault_b: Some(ps.token_vault1),
        mint_a_is_2022: false,
        mint_b_is_2022: false,
        extra_accounts: vec![ps.amm_config, ps.observation_key],
        math: PoolMath::Concentrated {
            sqrt_price_x64: ps.sqrt_price_x64,
            liquidity: ps.liquidity,
            tick_current: ps.tick_current,
            tick_spacing: ps.tick_spacing,
            fee_rate: 0, // patched from AmmConfig in streamer
            tick_arrays: vec![], // loaded in streamer via RPC
            limit_in_a: 0,
            limit_in_b: 0,
        },
        last_updated_slot: slot,
    }
}

/// Decode an AmmConfig from raw account bytes (skip 8-byte discriminator)
pub fn decode_amm_config(data: &[u8]) -> Option<AmmConfig> {
    use crate::streaming::event_parser::protocols::raydium_clmm::types::{
        amm_config_decode, AMM_CONFIG_SIZE,
    };
    if data.len() < AMM_CONFIG_SIZE + 8 {
        return None;
    }
    amm_config_decode(&data[8..AMM_CONFIG_SIZE + 8])
}

/// Decode a TickArrayState from raw account bytes (skip 8-byte discriminator)
pub fn decode_tick_array(data: &[u8]) -> Option<TickArray> {
    if data.len() < TICK_ARRAY_STATE_SIZE + 8 {
        return None;
    }
    let ta = tick_array_state_decode(&data[8..TICK_ARRAY_STATE_SIZE + 8])?;
    Some(tick_array_from_state(&ta))
}

/// Convert on-chain TickArrayState to our lightweight TickArray
pub fn tick_array_from_state(ta: &TickArrayState) -> TickArray {
    let ticks: Vec<Tick> = ta
        .ticks
        .iter()
        .filter(|t| t.liquidity_gross > 0)
        .map(|t| Tick {
            index: t.tick,
            liquidity_net: t.liquidity_net,
        })
        .collect();
    TickArray {
        start_tick_index: ta.start_tick_index,
        ticks,
    }
}

/// Compute the start_tick_index for a tick array containing `tick`.
/// Each tick array holds TICK_ARRAY_SIZE (60) ticks.
pub fn tick_array_start_index(tick: i32, tick_spacing: u16) -> i32 {
    let ticks_in_array = tick_spacing as i32 * 60;
    let mut start = tick / ticks_in_array * ticks_in_array;
    if tick < 0 && tick % ticks_in_array != 0 {
        start -= ticks_in_array;
    }
    start
}

/// Derive the PDA for a tick array account.
/// Raydium CLMM uses big-endian encoding for start_index in PDA seeds.
pub fn tick_array_pda(pool_id: &Pubkey, start_index: i32) -> Option<Pubkey> {
    let start_bytes = start_index.to_be_bytes();
    let (pda, _) = Pubkey::find_program_address(
        &[b"tick_array", pool_id.as_ref(), &start_bytes],
        &RAYDIUM_CLMM_PROGRAM,
    );
    Some(pda)
}

/// Get the 7 tick array start indices needed for quoting: current ± 3.
pub fn tick_array_start_indices(tick_current: i32, tick_spacing: u16) -> [i32; 7] {
    let ticks_in_array = tick_spacing as i32 * 60;
    let current_start = tick_array_start_index(tick_current, tick_spacing);
    [
        current_start - 3 * ticks_in_array,
        current_start - 2 * ticks_in_array,
        current_start - ticks_in_array,
        current_start,
        current_start + ticks_in_array,
        current_start + 2 * ticks_in_array,
        current_start + 3 * ticks_in_array,
    ]
}
