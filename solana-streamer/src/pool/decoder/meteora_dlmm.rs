use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, PoolMath, PoolState};
use crate::streaming::event_parser::protocols::meteora_dlmm::events::MeteoraDlmmLbPairAccountEvent;

/// Meteora DLMM program ID
const METEORA_DLMM_PROGRAM: Pubkey = solana_sdk::pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");

/// Number of bins per bin array
const BINS_PER_ARRAY: i32 = 70;

/// Compute the bin array index that contains a given bin_id.
/// Uses floor division (rounds toward negative infinity for negative bin_ids).
pub fn bin_id_to_bin_array_index(bin_id: i32) -> i64 {
    let idx = bin_id / BINS_PER_ARRAY;
    let rem = bin_id % BINS_PER_ARRAY;
    if bin_id < 0 && rem != 0 {
        (idx - 1) as i64
    } else {
        idx as i64
    }
}

/// Derive the PDA for a DLMM bin array account.
/// Seeds: ["bin_array", lb_pair, index.to_le_bytes()[0..8]]
pub fn bin_array_pda(lb_pair: &Pubkey, index: i64) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(
        &[b"bin_array", lb_pair.as_ref(), &index.to_le_bytes()],
        &METEORA_DLMM_PROGRAM,
    );
    pda
}

/// Compute 3 bin array PDAs needed for swap: current-1, current, current+1.
fn bin_array_pdas_for_swap(lb_pair: &Pubkey, active_id: i32) -> [Pubkey; 3] {
    let idx = bin_id_to_bin_array_index(active_id);
    [
        bin_array_pda(lb_pair, idx - 1),
        bin_array_pda(lb_pair, idx),
        bin_array_pda(lb_pair, idx + 1),
    ]
}

/// Decode from a parsed account event.
///
/// DLMM uses bin-based constant-sum within each bin. For off-chain quoting we
/// approximate as ConstantProduct with vault balances (filled in later via
/// gRPC vault subscription). The fee is computed from base_factor and bin_step:
///   base_fee = base_factor * bin_step * 10 / 1e9
/// We store this as fee_numerator / fee_denominator.
pub fn decode(event: &MeteoraDlmmLbPairAccountEvent) -> Option<PoolState> {
    // base_fee = base_factor * bin_step * 10 (in units of 1e-9)
    let base_fee = event.parameters.base_factor as u64 * event.bin_step as u64 * 10;

    Some(PoolState {
        address: event.pubkey,
        dex_type: DexType::MeteoraDlmm,
        mint_a: event.token_x_mint,
        mint_b: event.token_y_mint,
        vault_a: Some(event.reserve_x),
        vault_b: Some(event.reserve_y),
        mint_a_is_2022: false, // patched in streamer if needed
        mint_b_is_2022: false,
        extra_accounts: {
            let bin_pdas = bin_array_pdas_for_swap(&event.pubkey, event.active_id);
            vec![event.oracle, bin_pdas[0], bin_pdas[1], bin_pdas[2]]
        },
        math: PoolMath::ConstantProduct {
            reserve_a: 0, // filled by vault balance updates
            reserve_b: 0,
            fee_numerator: base_fee,
            fee_denominator: 1_000_000_000,
        },
        last_updated_slot: event.metadata.slot,
    })
}

/// Decode from raw account bytes.
///
/// LbPair layout (after 8-byte Anchor discriminator):
///   parameters.base_factor: u16     offset 8
///   ...
///   active_id: i32                  offset 84
///   bin_step: u16                   offset 88
///   status: u8                      offset 90
///   ...
///   token_x_mint: Pubkey (32)       offset 96
///   token_y_mint: Pubkey (32)       offset 128
///   reserve_x: Pubkey (32)          offset 160
///   reserve_y: Pubkey (32)          offset 192
///   ...
///   oracle: Pubkey (32)             offset 560
///
/// Minimum: 904 bytes total
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < 904 {
        return None;
    }

    // base_factor at offset 8 (u16 LE)
    let base_factor = u16::from_le_bytes(data[8..10].try_into().ok()?) as u64;
    // bin_step at offset 88 (u16 LE)
    let bin_step = u16::from_le_bytes(data[88..90].try_into().ok()?) as u64;

    let base_fee = base_factor * bin_step * 10;

    // active_id at offset 84 (i32 LE)
    let active_id = i32::from_le_bytes(data[84..88].try_into().ok()?);

    let token_x_mint = Pubkey::try_from(&data[96..128]).ok()?;
    let token_y_mint = Pubkey::try_from(&data[128..160]).ok()?;
    let reserve_x = Pubkey::try_from(&data[160..192]).ok()?;
    let reserve_y = Pubkey::try_from(&data[192..224]).ok()?;

    // oracle at offset 560
    let oracle = if data.len() >= 592 {
        Pubkey::try_from(&data[560..592]).ok()?
    } else {
        Pubkey::default()
    };

    Some(PoolState {
        address: *address,
        dex_type: DexType::MeteoraDlmm,
        mint_a: token_x_mint,
        mint_b: token_y_mint,
        vault_a: Some(reserve_x),
        vault_b: Some(reserve_y),
        mint_a_is_2022: false,
        mint_b_is_2022: false,
        extra_accounts: {
            let bin_pdas = bin_array_pdas_for_swap(address, active_id);
            vec![oracle, bin_pdas[0], bin_pdas[1], bin_pdas[2]]
        },
        math: PoolMath::ConstantProduct {
            reserve_a: 0,
            reserve_b: 0,
            fee_numerator: base_fee,
            fee_denominator: 1_000_000_000,
        },
        last_updated_slot: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_bytes_basic() {
        let mut data = vec![0u8; 904];
        // discriminator
        data[0..8].copy_from_slice(&[33, 11, 49, 98, 181, 101, 177, 13]);
        // base_factor = 10000
        data[8..10].copy_from_slice(&10000u16.to_le_bytes());
        // bin_step = 10
        data[88..90].copy_from_slice(&10u16.to_le_bytes());
        // token_x_mint
        data[96..128].copy_from_slice(&[1u8; 32]);
        // token_y_mint
        data[128..160].copy_from_slice(&[2u8; 32]);
        // reserve_x
        data[160..192].copy_from_slice(&[3u8; 32]);
        // reserve_y
        data[192..224].copy_from_slice(&[4u8; 32]);

        let address = Pubkey::new_unique();
        let pool = decode_bytes(&address, &data).unwrap();

        assert_eq!(pool.address, address);
        assert_eq!(pool.dex_type, DexType::MeteoraDlmm);
        assert_eq!(pool.mint_a, Pubkey::new_from_array([1u8; 32]));
        assert_eq!(pool.mint_b, Pubkey::new_from_array([2u8; 32]));
        assert_eq!(pool.vault_a, Some(Pubkey::new_from_array([3u8; 32])));
        assert_eq!(pool.vault_b, Some(Pubkey::new_from_array([4u8; 32])));

        match pool.math {
            PoolMath::ConstantProduct {
                reserve_a,
                reserve_b,
                fee_numerator,
                fee_denominator,
            } => {
                assert_eq!(reserve_a, 0);
                assert_eq!(reserve_b, 0);
                // base_fee = 10000 * 10 * 10 = 1_000_000
                assert_eq!(fee_numerator, 1_000_000);
                assert_eq!(fee_denominator, 1_000_000_000);
            }
            _ => panic!("Expected ConstantProduct math"),
        }
    }

    #[test]
    fn test_decode_bytes_too_short() {
        let data = vec![0u8; 100];
        let address = Pubkey::new_unique();
        assert!(decode_bytes(&address, &data).is_none());
    }
}
