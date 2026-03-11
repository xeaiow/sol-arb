pub mod bonk;
pub mod meteora_damm_v2;
pub mod pumpfun;
pub mod pumpswap;
pub mod raydium_amm_v4;
pub mod raydium_clmm;
pub mod raydium_cpmm;

use solana_sdk::pubkey::Pubkey;

use crate::streaming::event_parser::DexEvent;

use super::state::{DexType, PoolState};

/// Extract PoolState from a DexEvent account event
pub fn pool_state_from_event(event: &DexEvent) -> Option<PoolState> {
    match event {
        DexEvent::RaydiumAmmV4AmmInfoAccountEvent(e) => raydium_amm_v4::decode(e),
        DexEvent::RaydiumCpmmPoolStateAccountEvent(e) => raydium_cpmm::decode(e),
        DexEvent::RaydiumClmmPoolStateAccountEvent(e) => raydium_clmm::decode(e),
        DexEvent::PumpFunBondingCurveAccountEvent(e) => pumpfun::decode(e),
        DexEvent::PumpSwapPoolAccountEvent(e) => pumpswap::decode(e),
        DexEvent::BonkPoolStateAccountEvent(e) => bonk::decode(e),
        DexEvent::MeteoraDammV2PoolStateAccountEvent(e) => meteora_damm_v2::decode(e),
        _ => None,
    }
}

/// Extract PoolState from raw bytes (for RPC getAccountInfo)
pub fn pool_state_from_bytes(
    dex_type: DexType,
    address: &Pubkey,
    data: &[u8],
) -> Option<PoolState> {
    match dex_type {
        DexType::RaydiumAmmV4 => raydium_amm_v4::decode_bytes(address, data),
        DexType::RaydiumCpmm => raydium_cpmm::decode_bytes(address, data),
        DexType::RaydiumClmm => raydium_clmm::decode_bytes(address, data),
        DexType::PumpFun => pumpfun::decode_bytes(address, data),
        DexType::PumpSwap => pumpswap::decode_bytes(address, data),
        DexType::Bonk => bonk::decode_bytes(address, data),
        DexType::MeteoraDammV2 => meteora_damm_v2::decode_bytes(address, data),
    }
}
