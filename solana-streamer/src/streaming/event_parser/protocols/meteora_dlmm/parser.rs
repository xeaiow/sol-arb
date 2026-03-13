use crate::streaming::event_parser::{
    common::{read_u64_le, EventMetadata, EventType},
    protocols::meteora_dlmm::{
        discriminators, MeteoraDlmmLbPairAccountEvent, MeteoraDlmmSwap2Event,
        LbPairParameters, LbPairVParameters, LbPairRewardInfo,
        METEORA_DLMM_LB_PAIR_ACCOUNT_MIN_SIZE,
    },
    DexEvent,
};
use solana_sdk::pubkey::Pubkey;

/// Meteora DLMM 程式 ID
pub const METEORA_DLMM_PROGRAM_ID: Pubkey =
    solana_sdk::pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");

/// 解析 Meteora DLMM instruction data
///
/// 根據判別器路由到具體的 instruction 解析函數
pub fn parse_meteora_dlmm_instruction_data(
    discriminator: &[u8],
    data: &[u8],
    accounts: &[Pubkey],
    metadata: EventMetadata,
) -> Option<DexEvent> {
    match discriminator {
        discriminators::SWAP2_IX => parse_swap2_instruction(data, accounts, metadata),
        _ => None,
    }
}

/// 解析 Meteora DLMM inner instruction data
///
/// Meteora DLMM 目前沒有已知的 CPI event log，返回 None
pub fn parse_meteora_dlmm_inner_instruction_data(
    _discriminator: &[u8],
    _data: &[u8],
    _metadata: EventMetadata,
) -> Option<DexEvent> {
    None
}

/// 解析 Meteora DLMM 帳戶資料
///
/// 根據判別器路由到具體的帳戶解析函數
pub fn parse_meteora_dlmm_account_data(
    discriminator: &[u8],
    account: &crate::streaming::grpc::AccountPretty,
    metadata: crate::streaming::event_parser::common::EventMetadata,
) -> Option<crate::streaming::event_parser::DexEvent> {
    match discriminator {
        discriminators::LB_PAIR_ACCOUNT => parse_lb_pair_account(account, metadata),
        _ => None,
    }
}

/// 解析 swap2 指令
/// Swap2 指令帳戶佈局:
/// 0: lb_pair, 1: bin_array_bitmap_extension, 2: reserve_x, 3: reserve_y,
/// 4: user_token_in, 5: user_token_out, 6: token_x_mint, 7: token_y_mint,
/// 8: oracle, 9: host_fee_in, 10: user, 11: token_x_program, 12: token_y_program,
/// 13: event_authority, 14: program
/// 後面可能有多個 bin_array remaining accounts
fn parse_swap2_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    mut metadata: EventMetadata,
) -> Option<DexEvent> {
    metadata.event_type = EventType::MeteoraDlmmSwap2;

    if data.len() < 16 || accounts.len() < 15 {
        return None;
    }

    let amount_in = read_u64_le(data, 0)?;
    let min_amount_out = read_u64_le(data, 8)?;

    Some(DexEvent::MeteoraDlmmSwap2Event(MeteoraDlmmSwap2Event {
        metadata,
        amount_in,
        min_amount_out,
        lb_pair: accounts[0],
        bin_array_bitmap_extension: if accounts[1] == Pubkey::default() {
            None
        } else {
            Some(accounts[1])
        },
        reserve_x: accounts[2],
        reserve_y: accounts[3],
        user_token_in: accounts[4],
        user_token_out: accounts[5],
        token_x_mint: accounts[6],
        token_y_mint: accounts[7],
        oracle: accounts[8],
        host_fee_in: if accounts[9] == Pubkey::default() {
            None
        } else {
            Some(accounts[9])
        },
        user: accounts[10],
        token_x_program: accounts[11],
        token_y_program: accounts[12],
        event_authority: accounts[13],
        program: accounts[14],
    }))
}

/// 解析 LbPair 帳戶資料
fn parse_lb_pair_account(
    account: &crate::streaming::grpc::AccountPretty,
    mut metadata: EventMetadata,
) -> Option<DexEvent> {
    metadata.event_type = EventType::AccountMeteoraDlmmLbPair;

    if account.data.len() < METEORA_DLMM_LB_PAIR_ACCOUNT_MIN_SIZE {
        return None;
    }

    let data = &account.data;
    let mut offset = 8; // skip discriminator

    // StaticParameters (22 bytes)
    let base_factor = u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?);
    offset += 2;
    let filter_period = u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?);
    offset += 2;
    let decay_period = u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?);
    offset += 2;
    let reduction_factor = u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?);
    offset += 2;
    let variable_fee_control = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
    offset += 4;
    let max_volatility_accumulator = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
    offset += 4;
    let min_bin_id = i32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
    offset += 4;
    let max_bin_id = i32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
    offset += 4;
    let protocol_share = u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?);
    offset += 2;

    // Padding (6 bytes)
    offset += 6;

    // VParameters (20 bytes)
    let volatility_accumulator = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
    offset += 4;
    let volatility_reference = u32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
    offset += 4;
    let index_reference = i32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
    offset += 4;
    // padding (4 bytes)
    offset += 4;
    let last_update_timestamp = i64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
    offset += 8;

    // Padding (8 bytes)
    offset += 8;

    // bump_seed (1 byte)
    let bump_seed = [data[offset]];
    offset += 1;

    // bin_step_seed (2 bytes LE)
    let bin_step_seed = [data[offset], data[offset + 1]];
    offset += 2;

    // pair_type (1 byte)
    let pair_type = data[offset];
    offset += 1;

    // active_id (i32)
    let active_id = i32::from_le_bytes(data[offset..offset + 4].try_into().ok()?);
    offset += 4;

    // bin_step (u16)
    let bin_step = u16::from_le_bytes(data[offset..offset + 2].try_into().ok()?);
    offset += 2;

    // status (u8)
    let status = data[offset];
    offset += 1;

    // require_base_factor_seed (u8)
    let require_base_factor_seed = data[offset];
    offset += 1;

    // base_factor_seed (2 bytes)
    let base_factor_seed = [data[offset], data[offset + 1]];
    offset += 2;

    // padding (2 bytes)
    offset += 2;

    // token_x_mint (32 bytes)
    let token_x_mint = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
    offset += 32;

    // token_y_mint (32 bytes)
    let token_y_mint = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
    offset += 32;

    // reserve_x (32 bytes)
    let reserve_x = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
    offset += 32;

    // reserve_y (32 bytes)
    let reserve_y = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
    offset += 32;

    // protocol_fee_x (u64)
    let protocol_fee_x = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
    offset += 8;

    // protocol_fee_y (u64)
    let protocol_fee_y = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
    offset += 8;

    // padding (32 bytes)
    offset += 32;

    // reward_infos (2x RewardInfo)
    let mut reward_infos = [LbPairRewardInfo::default(), LbPairRewardInfo::default()];
    for ri in reward_infos.iter_mut() {
        ri.mint = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
        offset += 32;
        ri.vault = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
        offset += 32;
        ri.funder = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
        offset += 32;
        ri.reward_duration = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
        offset += 8;
        ri.reward_duration_end = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
        offset += 8;
        ri.reward_rate = u128::from_le_bytes(data[offset..offset + 16].try_into().ok()?);
        offset += 16;
        ri.last_update_time = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
        offset += 8;
        ri.cumulative_seconds_with_empty_liquidity_reward =
            u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
        offset += 8;
    }

    // oracle (32 bytes)
    let oracle = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
    offset += 32;

    // bin_array_bitmap (16 x u64 = 128 bytes)
    let mut bin_array_bitmap = [0u64; 16];
    for bm in bin_array_bitmap.iter_mut() {
        *bm = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
        offset += 8;
    }

    // last_updated_at (i64)
    let last_updated_at = i64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
    offset += 8;

    // whitelisted_wallet (32 bytes)
    let whitelisted_wallet = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
    offset += 32;

    // pre_activation_swap_address (32 bytes)
    let pre_activation_swap_address = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
    offset += 32;

    // base_key (32 bytes)
    let base_key = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
    offset += 32;

    // activation_slot (u64)
    let activation_slot = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
    offset += 8;

    // pre_activation_slot_duration (u64)
    let pre_activation_slot_duration = u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?);
    offset += 8;

    // padding (8 bytes)
    offset += 8;

    // lock_duration (u64)
    let lock_duration = if data.len() >= offset + 8 {
        u64::from_le_bytes(data[offset..offset + 8].try_into().ok()?)
    } else {
        0
    };
    offset += 8;

    // creator (32 bytes)
    let creator = if data.len() >= offset + 32 {
        Pubkey::try_from(&data[offset..offset + 32]).ok().unwrap_or_default()
    } else {
        Pubkey::default()
    };

    Some(DexEvent::MeteoraDlmmLbPairAccountEvent(MeteoraDlmmLbPairAccountEvent {
        metadata,
        pubkey: account.pubkey,
        parameters: LbPairParameters {
            base_factor,
            filter_period,
            decay_period,
            reduction_factor,
            variable_fee_control,
            max_volatility_accumulator,
            min_bin_id,
            max_bin_id,
            protocol_share,
        },
        v_parameters: LbPairVParameters {
            volatility_accumulator,
            volatility_reference,
            index_reference,
            last_update_timestamp,
        },
        bump_seed,
        bin_step_seed,
        pair_type,
        active_id,
        bin_step,
        status,
        require_base_factor_seed,
        base_factor_seed,
        token_x_mint,
        token_y_mint,
        reserve_x,
        reserve_y,
        protocol_fee_x,
        protocol_fee_y,
        reward_infos,
        oracle,
        bin_array_bitmap,
        last_updated_at,
        whitelisted_wallet,
        pre_activation_swap_address,
        base_key,
        activation_slot,
        pre_activation_slot_duration,
        lock_duration,
        creator,
    }))
}
