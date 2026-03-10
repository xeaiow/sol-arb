use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, PoolMath, PoolState};

/// Meteora DAMM v2 has no existing account event parser, so there is no
/// `decode()` from events. Only raw byte decoding is supported.

/// Decode from raw account bytes.
///
/// Layout (after 8-byte Anchor discriminator):
///   lp_mint:       Pubkey (32)
///   token_a_mint:  Pubkey (32)
///   token_b_mint:  Pubkey (32)
///   a_vault:       Pubkey (32)
///   b_vault:       Pubkey (32)
///
/// Total minimum: 8 + 5*32 = 168 bytes
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    const MIN_SIZE: usize = 8 + 32 * 5;
    if data.len() < MIN_SIZE {
        return None;
    }

    let offset = 8; // skip discriminator
    // skip lp_mint (32 bytes)
    let token_a_mint = Pubkey::try_from(&data[offset + 32..offset + 64]).ok()?;
    let token_b_mint = Pubkey::try_from(&data[offset + 64..offset + 96]).ok()?;
    let a_vault = Pubkey::try_from(&data[offset + 96..offset + 128]).ok()?;
    let b_vault = Pubkey::try_from(&data[offset + 128..offset + 160]).ok()?;

    Some(PoolState {
        address: *address,
        dex_type: DexType::MeteoraDammV2,
        mint_a: token_a_mint,
        mint_b: token_b_mint,
        vault_a: Some(a_vault),
        vault_b: Some(b_vault),
        mint_a_is_2022: false,
        mint_b_is_2022: false,
        extra_accounts: vec![],
        math: PoolMath::ConstantProduct {
            reserve_a: 0,
            reserve_b: 0,
            fee_numerator: 25,
            fee_denominator: 10000,
        },
        last_updated_slot: 0,
    })
}
