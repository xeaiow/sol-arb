use solana_sdk::pubkey::Pubkey;

use crate::pool::state::{DexType, PoolMath, PoolState};
use crate::streaming::event_parser::protocols::pumpfun::events::PumpFunBondingCurveAccountEvent;
use crate::streaming::event_parser::protocols::pumpfun::types::{
    bonding_curve_decode, BondingCurve, BONDING_CURVE_SIZE,
};

/// WSOL mint address
const WSOL: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");

/// Decode from a parsed account event. Returns None if bonding curve is complete.
pub fn decode(event: &PumpFunBondingCurveAccountEvent) -> Option<PoolState> {
    let bc = &event.bonding_curve;
    if bc.complete {
        return None;
    }
    Some(from_bonding_curve(&event.pubkey, bc, event.metadata.slot))
}

/// Decode from raw account bytes (skip 8-byte discriminator). Returns None if complete.
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < BONDING_CURVE_SIZE + 8 {
        return None;
    }
    let bc = bonding_curve_decode(&data[8..BONDING_CURVE_SIZE + 8])?;
    if bc.complete {
        return None;
    }
    Some(from_bonding_curve(address, &bc, 0))
}

fn from_bonding_curve(address: &Pubkey, bc: &BondingCurve, slot: u64) -> PoolState {
    PoolState {
        address: *address,
        dex_type: DexType::PumpFun,
        mint_a: Pubkey::default(), // token mint resolved externally
        mint_b: WSOL,
        vault_a: None,
        vault_b: None,
        mint_a_is_2022: false,
        mint_b_is_2022: false,
        extra_accounts: vec![bc.creator],
        math: PoolMath::BondingCurve {
            virtual_token_reserves: bc.virtual_token_reserves,
            virtual_sol_reserves: bc.virtual_sol_reserves,
            real_token_reserves: bc.real_token_reserves,
            real_sol_reserves: bc.real_sol_reserves,
            complete: bc.complete,
        },
        last_updated_slot: slot,
    }
}
