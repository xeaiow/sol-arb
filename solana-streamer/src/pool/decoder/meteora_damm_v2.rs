use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, PoolMath, PoolState};
use crate::streaming::event_parser::protocols::meteora_damm_v2::events::MeteoraDammV2PoolStateAccountEvent;

/// Fee denominator for Meteora DAMM V2 (cliff_fee_numerator / FEE_DENOMINATOR = fee rate)
const FEE_DENOMINATOR: u64 = 1_000_000_000;

/// Decode from a parsed account event
pub fn decode(event: &MeteoraDammV2PoolStateAccountEvent) -> Option<PoolState> {
    Some(PoolState {
        address: event.pubkey,
        dex_type: DexType::MeteoraDammV2,
        mint_a: event.token_a_mint,
        mint_b: event.token_b_mint,
        vault_a: Some(event.token_a_vault),
        vault_b: Some(event.token_b_vault),
        mint_a_is_2022: (event.token_a_flag & 1) != 0,
        mint_b_is_2022: (event.token_b_flag & 1) != 0,
        extra_accounts: vec![],
        math: PoolMath::ConstantProduct {
            reserve_a: 0,
            reserve_b: 0,
            fee_numerator: event.cliff_fee_numerator,
            fee_denominator: FEE_DENOMINATOR,
        },
        last_updated_slot: event.metadata.slot,
    })
}

/// Decode from raw account bytes.
///
/// Layout (after 8-byte Anchor discriminator):
///   pool_fees:     PoolFeesStruct (160 bytes)
///     - base_fee_info.data[0..8] = cliff_fee_numerator (u64 LE)
///   token_a_mint:  Pubkey (32)  offset 168
///   token_b_mint:  Pubkey (32)  offset 200
///   token_a_vault: Pubkey (32)  offset 232
///   token_b_vault: Pubkey (32)  offset 264
///   ...
///   pool_status:   u8           offset 481
///   token_a_flag:  u8           offset 482
///   token_b_flag:  u8           offset 483
///
/// Total minimum: 8 + 484 = 492 bytes (up to token_b_flag)
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < 484 {
        return None;
    }

    // cliff_fee_numerator: first 8 bytes of base_fee_info.data (offset 8)
    let cliff_fee_numerator = u64::from_le_bytes(data[8..16].try_into().ok()?);

    let token_a_mint = Pubkey::try_from(&data[168..200]).ok()?;
    let token_b_mint = Pubkey::try_from(&data[200..232]).ok()?;
    let a_vault = Pubkey::try_from(&data[232..264]).ok()?;
    let b_vault = Pubkey::try_from(&data[264..296]).ok()?;

    let token_a_flag = data[482];
    let token_b_flag = data[483];

    Some(PoolState {
        address: *address,
        dex_type: DexType::MeteoraDammV2,
        mint_a: token_a_mint,
        mint_b: token_b_mint,
        vault_a: Some(a_vault),
        vault_b: Some(b_vault),
        mint_a_is_2022: (token_a_flag & 1) != 0,
        mint_b_is_2022: (token_b_flag & 1) != 0,
        extra_accounts: vec![],
        math: PoolMath::ConstantProduct {
            reserve_a: 0,
            reserve_b: 0,
            fee_numerator: cliff_fee_numerator,
            fee_denominator: FEE_DENOMINATOR,
        },
        last_updated_slot: 0,
    })
}
