use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, PoolMath, PoolState};
use crate::streaming::event_parser::protocols::meteora_damm_v2::events::MeteoraDammV2PoolStateAccountEvent;

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
        math: PoolMath::DammV2Concentrated {
            sqrt_price_x64: event.sqrt_price,
            sqrt_min_price_x64: event.sqrt_min_price,
            sqrt_max_price_x64: event.sqrt_max_price,
            liquidity: event.liquidity,
            fee_numerator: event.cliff_fee_numerator,
            collect_fee_mode: event.collect_fee_mode,
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
///   liquidity:     u128         offset 368 (alignment-padded)
///   sqrt_min_price:u128         offset 424
///   sqrt_max_price:u128         offset 440
///   sqrt_price:    u128         offset 456
///   pool_status:   u8           offset 481
///   token_a_flag:  u8           offset 482
///   token_b_flag:  u8           offset 483
///   collect_fee_mode: u8        offset 484
///
/// Total minimum: 485 bytes
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < 485 {
        return None;
    }

    // cliff_fee_numerator: first 8 bytes of base_fee_info.data (offset 8)
    let cliff_fee_numerator = u64::from_le_bytes(data[8..16].try_into().ok()?);

    let token_a_mint = Pubkey::try_from(&data[168..200]).ok()?;
    let token_b_mint = Pubkey::try_from(&data[200..232]).ok()?;
    let a_vault = Pubkey::try_from(&data[232..264]).ok()?;
    let b_vault = Pubkey::try_from(&data[264..296]).ok()?;

    // CL fields
    let liquidity = u128::from_le_bytes(data[368..384].try_into().ok()?);
    let sqrt_min_price = u128::from_le_bytes(data[424..440].try_into().ok()?);
    let sqrt_max_price = u128::from_le_bytes(data[440..456].try_into().ok()?);
    let sqrt_price = u128::from_le_bytes(data[456..472].try_into().ok()?);

    let token_a_flag = data[482];
    let token_b_flag = data[483];
    let collect_fee_mode = data[484];

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
        math: PoolMath::DammV2Concentrated {
            sqrt_price_x64: sqrt_price,
            sqrt_min_price_x64: sqrt_min_price,
            sqrt_max_price_x64: sqrt_max_price,
            liquidity,
            fee_numerator: cliff_fee_numerator,
            collect_fee_mode,
        },
        last_updated_slot: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_pool_data() -> Vec<u8> {
        let mut data = vec![0u8; 1112];
        // discriminator
        data[0..8].copy_from_slice(&[241, 154, 109, 4, 17, 177, 109, 188]);
        // cliff_fee_numerator = 2_500_000 (0.25%)
        data[8..16].copy_from_slice(&2_500_000u64.to_le_bytes());
        // token_a_mint
        data[168..200].copy_from_slice(&[1u8; 32]);
        // token_b_mint
        data[200..232].copy_from_slice(&[2u8; 32]);
        // token_a_vault
        data[232..264].copy_from_slice(&[3u8; 32]);
        // token_b_vault
        data[264..296].copy_from_slice(&[4u8; 32]);
        // liquidity (u128 at offset 368)
        data[368..384].copy_from_slice(&1_000_000_000u128.to_le_bytes());
        // sqrt_min_price (u128 at offset 424)
        data[424..440].copy_from_slice(&100u128.to_le_bytes());
        // sqrt_max_price (u128 at offset 440)
        data[440..456].copy_from_slice(&999_999_999u128.to_le_bytes());
        // sqrt_price (u128 at offset 456)
        data[456..472].copy_from_slice(&500_000_000u128.to_le_bytes());
        // pool_status = 0 (enabled)
        data[481] = 0;
        // token_a_flag = 0 (SPL Token)
        data[482] = 0;
        // token_b_flag = 1 (Token-2022)
        data[483] = 1;
        // collect_fee_mode = 0 (BothToken)
        data[484] = 0;
        data
    }

    #[test]
    fn test_decode_bytes_extracts_fields() {
        let data = make_test_pool_data();
        let address = Pubkey::new_unique();
        let pool = decode_bytes(&address, &data).unwrap();

        assert_eq!(pool.address, address);
        assert_eq!(pool.dex_type, DexType::MeteoraDammV2);
        assert_eq!(pool.mint_a, Pubkey::new_from_array([1u8; 32]));
        assert_eq!(pool.mint_b, Pubkey::new_from_array([2u8; 32]));
        assert_eq!(pool.vault_a, Some(Pubkey::new_from_array([3u8; 32])));
        assert_eq!(pool.vault_b, Some(Pubkey::new_from_array([4u8; 32])));
        assert!(!pool.mint_a_is_2022);
        assert!(pool.mint_b_is_2022);

        match pool.math {
            PoolMath::DammV2Concentrated {
                sqrt_price_x64,
                sqrt_min_price_x64,
                sqrt_max_price_x64,
                liquidity,
                fee_numerator,
                collect_fee_mode,
            } => {
                assert_eq!(sqrt_price_x64, 500_000_000);
                assert_eq!(sqrt_min_price_x64, 100);
                assert_eq!(sqrt_max_price_x64, 999_999_999);
                assert_eq!(liquidity, 1_000_000_000);
                assert_eq!(fee_numerator, 2_500_000);
                assert_eq!(collect_fee_mode, 0);
            }
            _ => panic!("Expected DammV2Concentrated math"),
        }
    }

    #[test]
    fn test_decode_bytes_too_short() {
        let data = vec![0u8; 100];
        let address = Pubkey::new_unique();
        assert!(decode_bytes(&address, &data).is_none());
    }
}
