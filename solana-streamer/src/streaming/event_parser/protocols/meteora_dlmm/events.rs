use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use crate::streaming::event_parser::common::EventMetadata;

/// Meteora DLMM Swap2 Event (from swap2 instruction)
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeteoraDlmmSwap2Event {
    pub metadata: EventMetadata,

    // Instruction parameters
    pub amount_in: u64,
    pub min_amount_out: u64,

    // Accounts from instruction
    pub lb_pair: Pubkey,
    pub bin_array_bitmap_extension: Option<Pubkey>,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub user_token_in: Pubkey,
    pub user_token_out: Pubkey,
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub oracle: Pubkey,
    pub host_fee_in: Option<Pubkey>,
    pub user: Pubkey,
    pub token_x_program: Pubkey,
    pub token_y_program: Pubkey,
    pub event_authority: Pubkey,
    pub program: Pubkey,
}

/// Meteora DLMM LbPair Account Event (from gRPC account subscription)
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeteoraDlmmLbPairAccountEvent {
    pub metadata: EventMetadata,
    pub pubkey: Pubkey,
    pub parameters: LbPairParameters,
    pub v_parameters: LbPairVParameters,
    pub bump_seed: [u8; 1],
    pub bin_step_seed: [u8; 2],
    pub pair_type: u8,
    pub active_id: i32,
    pub bin_step: u16,
    pub status: u8,
    pub require_base_factor_seed: u8,
    pub base_factor_seed: [u8; 2],
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub protocol_fee_x: u64,
    pub protocol_fee_y: u64,
    pub reward_infos: [LbPairRewardInfo; 2],
    pub oracle: Pubkey,
    pub bin_array_bitmap: [u64; 16],
    pub last_updated_at: i64,
    pub whitelisted_wallet: Pubkey,
    pub pre_activation_swap_address: Pubkey,
    pub base_key: Pubkey,
    pub activation_slot: u64,
    pub pre_activation_slot_duration: u64,
    pub lock_duration: u64,
    pub creator: Pubkey,
}

/// LbPair static parameters
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LbPairParameters {
    pub base_factor: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    pub min_bin_id: i32,
    pub max_bin_id: i32,
    pub protocol_share: u16,
}

/// LbPair volatile parameters
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LbPairVParameters {
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub index_reference: i32,
    pub last_update_timestamp: i64,
}

/// LbPair reward info
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LbPairRewardInfo {
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub funder: Pubkey,
    pub reward_duration: u64,
    pub reward_duration_end: u64,
    pub reward_rate: u128,
    pub last_update_time: u64,
    pub cumulative_seconds_with_empty_liquidity_reward: u64,
}

/// LbPair account discriminator
pub const METEORA_DLMM_LB_PAIR_DISCRIMINATOR: &[u8] = &[33, 11, 49, 98, 181, 101, 177, 13];

/// Minimum LbPair account data size
pub const METEORA_DLMM_LB_PAIR_ACCOUNT_MIN_SIZE: usize = 904;

/// Event discriminators
pub mod discriminators {
    // Instruction discriminators
    pub const SWAP2_IX: &[u8] = &[65, 75, 63, 76, 235, 91, 91, 136];

    // Account discriminators
    pub const LB_PAIR_ACCOUNT: &[u8] = &[33, 11, 49, 98, 181, 101, 177, 13];
}
