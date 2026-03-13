use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;

use crate::streaming::event_parser::common::EventMetadata;

/// Orca Whirlpool SwapV2 Event (from swap_v2 instruction)
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrcaWhirlpoolSwapV2Event {
    pub metadata: EventMetadata,

    // Instruction parameters
    pub amount: u64,
    pub other_amount_threshold: u64,
    pub sqrt_price_limit: u128,
    pub amount_specified_is_input: bool,
    pub a_to_b: bool,

    // Accounts from instruction
    pub token_program_a: Pubkey,
    pub token_program_b: Pubkey,
    pub memo_program: Pubkey,
    pub token_authority: Pubkey,
    pub whirlpool: Pubkey,
    pub token_mint_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_owner_account_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_owner_account_b: Pubkey,
    pub token_vault_b: Pubkey,
    pub tick_array_0: Pubkey,
    pub tick_array_1: Pubkey,
    pub tick_array_2: Pubkey,
    pub oracle: Pubkey,
}

/// Orca Whirlpool Account Event (from gRPC account subscription)
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrcaWhirlpoolAccountEvent {
    pub metadata: EventMetadata,
    pub pubkey: Pubkey,
    pub whirlpools_config: Pubkey,
    pub whirlpool_bump: [u8; 1],
    pub tick_spacing: u16,
    pub tick_spacing_seed: [u8; 2],
    pub fee_rate: u16,
    pub protocol_fee_rate: u16,
    pub liquidity: u128,
    pub sqrt_price: u128,
    pub tick_current_index: i32,
    pub protocol_fee_owed_a: u64,
    pub protocol_fee_owed_b: u64,
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub fee_growth_global_a: u128,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
    pub fee_growth_global_b: u128,
    pub reward_last_updated_timestamp: u64,
    pub reward_infos: [WhirlpoolRewardInfo; 3],
}

/// Whirlpool reward info
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WhirlpoolRewardInfo {
    pub mint: Pubkey,
    pub vault: Pubkey,
    pub authority: Pubkey,
    pub emissions_per_second_x64: u128,
    pub growth_global_x64: u128,
}

/// Orca Whirlpool TickArray Account Event (from gRPC account subscription)
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrcaWhirlpoolTickArrayAccountEvent {
    pub metadata: EventMetadata,
    pub pubkey: Pubkey,
    pub start_tick_index: i32,
    pub ticks: Vec<Tick>,
    pub whirlpool: Pubkey,
}

/// Single tick data
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tick {
    pub initialized: bool,
    pub liquidity_net: i128,
    pub liquidity_gross: u128,
    pub fee_growth_outside_a: u128,
    pub fee_growth_outside_b: u128,
    pub reward_growths_outside: [u128; 3],
}

/// Whirlpool account discriminator
pub const ORCA_WHIRLPOOL_DISCRIMINATOR: &[u8] = &[63, 149, 209, 12, 225, 140, 156, 104];

/// Minimum Whirlpool account data size
pub const ORCA_WHIRLPOOL_ACCOUNT_MIN_SIZE: usize = 653;

/// TickArray account discriminator
pub const ORCA_WHIRLPOOL_TICK_ARRAY_DISCRIMINATOR: &[u8] = &[69, 97, 189, 190, 110, 7, 169, 12];

/// Minimum TickArray account data size (8 disc + 4 start_tick_index + 88 ticks * 88 bytes + 32 whirlpool)
/// 8 disc + 4 start_tick_index + 88*113 ticks + 32 whirlpool = 9988
pub const ORCA_WHIRLPOOL_TICK_ARRAY_ACCOUNT_MIN_SIZE: usize = 9988;

/// Number of ticks per tick array
pub const TICK_ARRAY_SIZE: usize = 88;

/// Size of a single tick in bytes (1 + 16 + 16 + 16 + 16 + 48 = 113 bytes, but Orca uses 88)
pub const TICK_SIZE: usize = 113;

/// Event discriminators
pub mod discriminators {
    // Instruction discriminators
    pub const SWAP_V2_IX: &[u8] = &[43, 4, 237, 11, 26, 201, 109, 55];

    // Account discriminators
    pub const WHIRLPOOL_ACCOUNT: &[u8] = &[63, 149, 209, 12, 225, 140, 156, 104];
    pub const TICK_ARRAY_ACCOUNT: &[u8] = &[69, 97, 189, 190, 110, 7, 169, 12];
}
