use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, DlmmBin, DlmmBinArray, PoolMath, PoolState};
use crate::streaming::event_parser::protocols::meteora_dlmm::events::MeteoraDlmmLbPairAccountEvent;

/// Meteora DLMM program ID
pub const METEORA_DLMM_PROGRAM: Pubkey = solana_sdk::pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");

/// Number of bins per bin array
const BINS_PER_ARRAY: i32 = 70;

/// Size of each bin in the bin array account (bytes)
const BIN_SIZE: usize = 144;

/// Header size before bins start in bin array account:
/// discriminator(8) + index(8) + version(1) + padding(7) + lb_pair(32) = 56
const BIN_ARRAY_HEADER_SIZE: usize = 56;

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
pub fn bin_array_pdas_for_swap(lb_pair: &Pubkey, active_id: i32) -> [Pubkey; 3] {
    let idx = bin_id_to_bin_array_index(active_id);
    [
        bin_array_pda(lb_pair, idx - 1),
        bin_array_pda(lb_pair, idx),
        bin_array_pda(lb_pair, idx + 1),
    ]
}

/// Parse DLMM fee parameters common to both decode() and decode_bytes().
struct DlmmFeeParams {
    active_id: i32,
    bin_step: u16,
    base_factor: u16,
    variable_fee_control: u32,
    max_volatility_accumulator: u32,
    volatility_accumulator: u32,
    volatility_reference: u32,
    index_reference: i32,
}

/// Parse fee parameters from raw LbPair account bytes.
///
/// LbPair layout (after 8-byte Anchor discriminator):
///   parameters.base_factor: u16     offset 8
///   variable_fee_control: u32       offset 16
///   max_volatility_accumulator: u32 offset 20
///   v_parameters.volatility_accumulator: u32 offset 40
///   v_parameters.volatility_reference: u32   offset 44
///   v_parameters.index_reference: i32        offset 48
///   active_id: i32                  offset 84
///   bin_step: u16                   offset 88
fn parse_fee_params(data: &[u8]) -> Option<DlmmFeeParams> {
    if data.len() < 90 {
        return None;
    }
    Some(DlmmFeeParams {
        base_factor: u16::from_le_bytes(data[8..10].try_into().ok()?),
        variable_fee_control: u32::from_le_bytes(data[16..20].try_into().ok()?),
        max_volatility_accumulator: u32::from_le_bytes(data[20..24].try_into().ok()?),
        volatility_accumulator: u32::from_le_bytes(data[40..44].try_into().ok()?),
        volatility_reference: u32::from_le_bytes(data[44..48].try_into().ok()?),
        index_reference: i32::from_le_bytes(data[48..52].try_into().ok()?),
        active_id: i32::from_le_bytes(data[84..88].try_into().ok()?),
        bin_step: u16::from_le_bytes(data[88..90].try_into().ok()?),
    })
}

/// Build PoolMath::MeteoraDlmm from fee params (bin_arrays initially empty).
fn math_from_params(params: &DlmmFeeParams) -> PoolMath {
    PoolMath::MeteoraDlmm {
        active_id: params.active_id,
        bin_step: params.bin_step,
        base_factor: params.base_factor,
        variable_fee_control: params.variable_fee_control,
        max_volatility_accumulator: params.max_volatility_accumulator,
        volatility_accumulator: params.volatility_accumulator,
        volatility_reference: params.volatility_reference,
        index_reference: params.index_reference,
        bin_arrays: vec![], // filled by streamer after fetch
    }
}

/// Decode from a parsed account event.
pub fn decode(event: &MeteoraDlmmLbPairAccountEvent) -> Option<PoolState> {
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
        math: PoolMath::MeteoraDlmm {
            active_id: event.active_id,
            bin_step: event.bin_step,
            base_factor: event.parameters.base_factor,
            variable_fee_control: event.parameters.variable_fee_control,
            max_volatility_accumulator: event.parameters.max_volatility_accumulator,
            volatility_accumulator: event.v_parameters.volatility_accumulator,
            volatility_reference: event.v_parameters.volatility_reference,
            index_reference: event.v_parameters.index_reference,
            bin_arrays: vec![],
        },
        last_updated_slot: event.metadata.slot,
    })
}

/// Decode from raw account bytes.
///
/// LbPair layout (after 8-byte Anchor discriminator):
///   parameters.base_factor: u16     offset 8
///   variable_fee_control: u32       offset 16
///   max_volatility_accumulator: u32 offset 20
///   v_parameters.volatility_accumulator: u32 offset 40
///   v_parameters.volatility_reference: u32   offset 44
///   v_parameters.index_reference: i32        offset 48
///   active_id: i32                  offset 84
///   bin_step: u16                   offset 88
///   status: u8                      offset 90
///   token_x_mint: Pubkey (32)       offset 96
///   token_y_mint: Pubkey (32)       offset 128
///   reserve_x: Pubkey (32)          offset 160
///   reserve_y: Pubkey (32)          offset 192
///   oracle: Pubkey (32)             offset 560
///
/// Minimum: 904 bytes total
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < 904 {
        return None;
    }

    let params = parse_fee_params(data)?;

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
            let bin_pdas = bin_array_pdas_for_swap(address, params.active_id);
            vec![oracle, bin_pdas[0], bin_pdas[1], bin_pdas[2]]
        },
        math: math_from_params(&params),
        last_updated_slot: 0,
    })
}

/// Decode a bin array account into DlmmBinArray.
///
/// Bin array layout:
///   discriminator: 8 bytes
///   index: i64 (8 bytes)
///   version: u8 (1 byte)
///   padding: 7 bytes
///   lb_pair: Pubkey (32 bytes)
///   --- total header: 56 bytes ---
///   bins[0..70]: 70 × 144 bytes each
///     amount_x: u64 (8 bytes)
///     amount_y: u64 (8 bytes)
///     ... (remaining 128 bytes per bin not needed)
///
/// Minimum size: 56 + 70 × 144 = 10136 bytes
pub fn decode_bin_array(data: &[u8]) -> Option<DlmmBinArray> {
    let min_size = BIN_ARRAY_HEADER_SIZE + BINS_PER_ARRAY as usize * BIN_SIZE;
    if data.len() < min_size {
        return None;
    }

    let index = i64::from_le_bytes(data[8..16].try_into().ok()?);

    let mut bins = Vec::new();
    let start_bin_id = (index as i32) * BINS_PER_ARRAY;

    for i in 0..BINS_PER_ARRAY as usize {
        let offset = BIN_ARRAY_HEADER_SIZE + i * BIN_SIZE;
        let amount_x = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
        let amount_y = u64::from_le_bytes(data[offset + 8..offset + 16].try_into().ok()?);

        // Only include bins with liquidity
        if amount_x > 0 || amount_y > 0 {
            bins.push(DlmmBin {
                bin_id: start_bin_id + i as i32,
                amount_x,
                amount_y,
            });
        }
    }

    Some(DlmmBinArray { index, bins })
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
            PoolMath::MeteoraDlmm {
                active_id,
                bin_step,
                base_factor,
                ref bin_arrays,
                ..
            } => {
                assert_eq!(active_id, 0);
                assert_eq!(bin_step, 10);
                assert_eq!(base_factor, 10000);
                assert!(bin_arrays.is_empty()); // filled later by streamer
            }
            _ => panic!("Expected MeteoraDlmm math"),
        }
    }

    #[test]
    fn test_decode_bytes_too_short() {
        let data = vec![0u8; 100];
        let address = Pubkey::new_unique();
        assert!(decode_bytes(&address, &data).is_none());
    }

    #[test]
    fn test_decode_bin_array_basic() {
        let min_size = BIN_ARRAY_HEADER_SIZE + BINS_PER_ARRAY as usize * BIN_SIZE;
        let mut data = vec![0u8; min_size];

        // discriminator (8 bytes)
        data[0..8].copy_from_slice(&[0u8; 8]);
        // index = 5 (i64)
        data[8..16].copy_from_slice(&5i64.to_le_bytes());

        // Set bin at position 0 with liquidity
        let bin0_offset = BIN_ARRAY_HEADER_SIZE;
        data[bin0_offset..bin0_offset + 8].copy_from_slice(&1000u64.to_le_bytes()); // amount_x
        data[bin0_offset + 8..bin0_offset + 16].copy_from_slice(&2000u64.to_le_bytes()); // amount_y

        // Set bin at position 3 with only Y liquidity
        let bin3_offset = BIN_ARRAY_HEADER_SIZE + 3 * BIN_SIZE;
        data[bin3_offset..bin3_offset + 8].copy_from_slice(&0u64.to_le_bytes()); // amount_x = 0
        data[bin3_offset + 8..bin3_offset + 16].copy_from_slice(&5000u64.to_le_bytes()); // amount_y

        let ba = decode_bin_array(&data).unwrap();
        assert_eq!(ba.index, 5);
        assert_eq!(ba.bins.len(), 2); // only bins with liquidity

        // bin_id = 5 * 70 + 0 = 350
        assert_eq!(ba.bins[0].bin_id, 350);
        assert_eq!(ba.bins[0].amount_x, 1000);
        assert_eq!(ba.bins[0].amount_y, 2000);

        // bin_id = 5 * 70 + 3 = 353
        assert_eq!(ba.bins[1].bin_id, 353);
        assert_eq!(ba.bins[1].amount_x, 0);
        assert_eq!(ba.bins[1].amount_y, 5000);
    }

    #[test]
    fn test_decode_bin_array_too_short() {
        let data = vec![0u8; 100];
        assert!(decode_bin_array(&data).is_none());
    }

    #[test]
    fn test_decode_bin_array_empty() {
        let min_size = BIN_ARRAY_HEADER_SIZE + BINS_PER_ARRAY as usize * BIN_SIZE;
        let mut data = vec![0u8; min_size];
        data[8..16].copy_from_slice(&0i64.to_le_bytes()); // index = 0

        let ba = decode_bin_array(&data).unwrap();
        assert_eq!(ba.index, 0);
        assert!(ba.bins.is_empty()); // no liquidity
    }

    #[test]
    fn test_bin_id_to_bin_array_index() {
        assert_eq!(bin_id_to_bin_array_index(0), 0);
        assert_eq!(bin_id_to_bin_array_index(69), 0);
        assert_eq!(bin_id_to_bin_array_index(70), 1);
        assert_eq!(bin_id_to_bin_array_index(139), 1);
        assert_eq!(bin_id_to_bin_array_index(140), 2);
        assert_eq!(bin_id_to_bin_array_index(-1), -1);
        assert_eq!(bin_id_to_bin_array_index(-70), -1);
        assert_eq!(bin_id_to_bin_array_index(-71), -2);
    }
}
