use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, PoolMath, PoolState, Tick, TickArray};
use crate::streaming::event_parser::protocols::orca_whirlpool::events::{
    OrcaWhirlpoolAccountEvent, OrcaWhirlpoolTickArrayAccountEvent,
};

/// Orca Whirlpool program ID
const ORCA_WHIRLPOOL_PROGRAM: Pubkey =
    solana_sdk::pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");

/// Decode from a parsed account event.
///
/// Orca Whirlpool uses the same concentrated liquidity model as Raydium CLMM:
/// tick-based with sqrt_price_x64, liquidity, tick_current, tick_spacing.
/// fee_rate is in basis points (1/100 of 1%), we convert to 1e-6 units.
pub fn decode(event: &OrcaWhirlpoolAccountEvent) -> Option<PoolState> {
    Some(from_whirlpool_data(
        &event.pubkey,
        event.tick_spacing,
        event.fee_rate,
        event.liquidity,
        event.sqrt_price,
        event.tick_current_index,
        event.token_mint_a,
        event.token_mint_b,
        event.token_vault_a,
        event.token_vault_b,
        event.metadata.slot,
    ))
}

/// Decode from raw account bytes (skip 8-byte discriminator).
///
/// Layout (after 8-byte Anchor discriminator):
///   whirlpools_config: Pubkey (32)  offset 8
///   whirlpool_bump: [u8; 1]         offset 40
///   tick_spacing: u16               offset 41
///   tick_spacing_seed: [u8; 2]      offset 43
///   fee_rate: u16                   offset 45
///   protocol_fee_rate: u16          offset 47
///   liquidity: u128                 offset 49
///   sqrt_price: u128                offset 65
///   tick_current_index: i32         offset 81
///   ...
///   token_mint_a: Pubkey (32)       offset 101
///   token_vault_a: Pubkey (32)      offset 133
///   ...
///   token_mint_b: Pubkey (32)       offset 181
///   token_vault_b: Pubkey (32)      offset 213
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < 653 {
        return None;
    }

    let tick_spacing = u16::from_le_bytes(data[41..43].try_into().ok()?);
    let fee_rate = u16::from_le_bytes(data[45..47].try_into().ok()?);
    let liquidity = u128::from_le_bytes(data[49..65].try_into().ok()?);
    let sqrt_price = u128::from_le_bytes(data[65..81].try_into().ok()?);
    let tick_current_index = i32::from_le_bytes(data[81..85].try_into().ok()?);
    let token_mint_a = Pubkey::try_from(&data[101..133]).ok()?;
    let token_vault_a = Pubkey::try_from(&data[133..165]).ok()?;
    let token_mint_b = Pubkey::try_from(&data[181..213]).ok()?;
    let token_vault_b = Pubkey::try_from(&data[213..245]).ok()?;

    Some(from_whirlpool_data(
        address,
        tick_spacing,
        fee_rate,
        liquidity,
        sqrt_price,
        tick_current_index,
        token_mint_a,
        token_mint_b,
        token_vault_a,
        token_vault_b,
        0,
    ))
}

fn from_whirlpool_data(
    address: &Pubkey,
    tick_spacing: u16,
    fee_rate: u16,
    liquidity: u128,
    sqrt_price: u128,
    tick_current_index: i32,
    token_mint_a: Pubkey,
    token_mint_b: Pubkey,
    token_vault_a: Pubkey,
    token_vault_b: Pubkey,
    slot: u64,
) -> PoolState {
    // Orca fee_rate is in basis points (1/10000), convert to 1e-6 units for our CLMM math
    let fee_rate_1e6 = fee_rate as u32 * 100;

    // Derive oracle PDA
    let (oracle, _) = Pubkey::find_program_address(
        &[b"oracle", address.as_ref()],
        &ORCA_WHIRLPOOL_PROGRAM,
    );

    PoolState {
        address: *address,
        dex_type: DexType::OrcaWhirlpool,
        mint_a: token_mint_a,
        mint_b: token_mint_b,
        vault_a: Some(token_vault_a),
        vault_b: Some(token_vault_b),
        mint_a_is_2022: false,
        mint_b_is_2022: false,
        extra_accounts: vec![oracle],
        math: PoolMath::Concentrated {
            sqrt_price_x64: sqrt_price,
            liquidity,
            tick_current: tick_current_index,
            tick_spacing,
            fee_rate: fee_rate_1e6,
            tick_arrays: vec![], // loaded in streamer via RPC
            limit_in_a: 0,
            limit_in_b: 0,
        },
        last_updated_slot: slot,
    }
}

/// Convert an OrcaWhirlpoolTickArrayAccountEvent to our TickArray.
///
/// Each tick in the array is spaced `tick_spacing` apart, so the absolute
/// tick index for slot `i` is `start_tick_index + i * tick_spacing`.
pub fn tick_array_from_event(event: &OrcaWhirlpoolTickArrayAccountEvent, tick_spacing: u16) -> TickArray {
    let ticks: Vec<Tick> = event
        .ticks
        .iter()
        .enumerate()
        .filter(|(_, t)| t.liquidity_gross > 0)
        .map(|(i, t)| Tick {
            index: event.start_tick_index + i as i32 * tick_spacing as i32,
            liquidity_net: t.liquidity_net,
        })
        .collect();
    TickArray {
        start_tick_index: event.start_tick_index,
        ticks,
    }
}

/// Decode a TickArray from raw account bytes (skip 8-byte discriminator).
///
/// Layout (after 8-byte Anchor discriminator):
///   start_tick_index: i32          offset 8
///   ticks: [Tick; 88]              offset 12 (each tick = 113 bytes)
///   whirlpool: Pubkey (32)         offset 12 + 88*113 = 9956
pub fn decode_tick_array(data: &[u8], tick_spacing: u16) -> Option<TickArray> {
    if data.len() < 9988 {
        return None;
    }

    let start_tick_index = i32::from_le_bytes(data[8..12].try_into().ok()?);

    let mut ticks = Vec::new();
    let mut offset = 12;
    for i in 0..88usize {
        if data.len() < offset + 113 {
            break;
        }
        let initialized = data[offset] != 0;
        offset += 1;
        let liquidity_net = i128::from_le_bytes(data[offset..offset + 16].try_into().ok()?);
        offset += 16;
        let liquidity_gross = u128::from_le_bytes(data[offset..offset + 16].try_into().ok()?);
        offset += 16;
        // Skip fee_growth_outside_a(16) + fee_growth_outside_b(16) + reward_growths(3*16) = 80 bytes
        offset += 80;

        if initialized && liquidity_gross > 0 {
            ticks.push(Tick {
                index: start_tick_index + i as i32 * tick_spacing as i32,
                liquidity_net,
            });
        }
    }

    Some(TickArray {
        start_tick_index,
        ticks,
    })
}

/// Compute the start_tick_index for a tick array containing `tick`.
/// Each Orca Whirlpool tick array holds 88 ticks.
pub fn tick_array_start_index(tick: i32, tick_spacing: u16) -> i32 {
    let ticks_in_array = tick_spacing as i32 * 88;
    let mut start = tick / ticks_in_array * ticks_in_array;
    if tick < 0 && tick % ticks_in_array != 0 {
        start -= ticks_in_array;
    }
    start
}

/// Derive the PDA for a Whirlpool tick array account.
pub fn tick_array_pda(whirlpool: &Pubkey, start_index: i32) -> Option<Pubkey> {
    let start_str = start_index.to_string();
    let (pda, _) = Pubkey::find_program_address(
        &[b"tick_array", whirlpool.as_ref(), start_str.as_bytes()],
        &ORCA_WHIRLPOOL_PROGRAM,
    );
    Some(pda)
}

/// Get the 7 tick array start indices needed for quoting: current ± 3.
pub fn tick_array_start_indices(tick_current: i32, tick_spacing: u16) -> [i32; 7] {
    let ticks_in_array = tick_spacing as i32 * 88;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_bytes_basic() {
        let mut data = vec![0u8; 653];
        // discriminator
        data[0..8].copy_from_slice(&[63, 149, 209, 12, 225, 140, 156, 104]);
        // tick_spacing = 64
        data[41..43].copy_from_slice(&64u16.to_le_bytes());
        // fee_rate = 30 (0.30% = 30 bps)
        data[45..47].copy_from_slice(&30u16.to_le_bytes());
        // liquidity
        data[49..65].copy_from_slice(&1000000u128.to_le_bytes());
        // sqrt_price
        data[65..81].copy_from_slice(&(1u128 << 64).to_le_bytes());
        // tick_current_index = 100
        data[81..85].copy_from_slice(&100i32.to_le_bytes());
        // token_mint_a
        data[101..133].copy_from_slice(&[1u8; 32]);
        // token_vault_a
        data[133..165].copy_from_slice(&[2u8; 32]);
        // token_mint_b
        data[181..213].copy_from_slice(&[3u8; 32]);
        // token_vault_b
        data[213..245].copy_from_slice(&[4u8; 32]);

        let address = Pubkey::new_unique();
        let pool = decode_bytes(&address, &data).unwrap();

        assert_eq!(pool.address, address);
        assert_eq!(pool.dex_type, DexType::OrcaWhirlpool);
        assert_eq!(pool.mint_a, Pubkey::new_from_array([1u8; 32]));
        assert_eq!(pool.mint_b, Pubkey::new_from_array([3u8; 32]));

        match pool.math {
            PoolMath::Concentrated {
                tick_spacing,
                fee_rate,
                tick_current,
                ..
            } => {
                assert_eq!(tick_spacing, 64);
                assert_eq!(fee_rate, 3000); // 30 bps * 100 = 3000 in 1e-6
                assert_eq!(tick_current, 100);
            }
            _ => panic!("Expected Concentrated math"),
        }
    }

    #[test]
    fn test_tick_array_start_index() {
        // tick_spacing=64, ticks_in_array=64*88=5632
        assert_eq!(tick_array_start_index(100, 64), 0);
        assert_eq!(tick_array_start_index(5632, 64), 5632);
        assert_eq!(tick_array_start_index(-1, 64), -5632);
    }
}
