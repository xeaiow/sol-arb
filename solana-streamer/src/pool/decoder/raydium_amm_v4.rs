use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, PoolMath, PoolState};
use crate::streaming::event_parser::protocols::raydium_amm_v4::events::RaydiumAmmV4AmmInfoAccountEvent;
use crate::streaming::event_parser::protocols::raydium_amm_v4::types::{amm_info_decode, AmmInfo};

/// Decode from a parsed account event
pub fn decode(event: &RaydiumAmmV4AmmInfoAccountEvent) -> Option<PoolState> {
    let info = &event.amm_info;
    Some(from_amm_info(&event.pubkey, info, event.metadata.slot))
}

/// Decode from raw account bytes (no discriminator for Raydium AMM V4)
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    let info = amm_info_decode(data)?;
    Some(from_amm_info(address, &info, 0))
}

fn from_amm_info(address: &Pubkey, info: &AmmInfo, slot: u64) -> PoolState {
    PoolState {
        address: *address,
        dex_type: DexType::RaydiumAmmV4,
        mint_a: info.coin_mint,
        mint_b: info.pc_mint,
        vault_a: Some(info.token_coin),
        vault_b: Some(info.token_pc),
        mint_a_is_2022: false,
        mint_b_is_2022: false,
        extra_accounts: vec![],
        math: PoolMath::ConstantProduct {
            reserve_a: 0,
            reserve_b: 0,
            fee_numerator: info.fees.swap_fee_numerator,
            fee_denominator: info.fees.swap_fee_denominator,
        },
        last_updated_slot: slot,
    }
}
