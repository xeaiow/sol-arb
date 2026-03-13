use crate::streaming::event_parser::{
    common::{read_u64_le, EventMetadata, EventType},
    protocols::orca_whirlpool::{
        discriminators, OrcaWhirlpoolAccountEvent, OrcaWhirlpoolSwapV2Event,
        OrcaWhirlpoolTickArrayAccountEvent, WhirlpoolRewardInfo, Tick,
        ORCA_WHIRLPOOL_ACCOUNT_MIN_SIZE, ORCA_WHIRLPOOL_TICK_ARRAY_ACCOUNT_MIN_SIZE,
        TICK_ARRAY_SIZE, TICK_SIZE,
    },
    DexEvent,
};
use solana_sdk::pubkey::Pubkey;

/// Orca Whirlpool 程式 ID
pub const ORCA_WHIRLPOOL_PROGRAM_ID: Pubkey =
    solana_sdk::pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");

/// 解析 Orca Whirlpool instruction data
pub fn parse_orca_whirlpool_instruction_data(
    discriminator: &[u8],
    data: &[u8],
    accounts: &[Pubkey],
    metadata: EventMetadata,
) -> Option<DexEvent> {
    match discriminator {
        discriminators::SWAP_V2_IX => parse_swap_v2_instruction(data, accounts, metadata),
        _ => None,
    }
}

/// 解析 Orca Whirlpool inner instruction data
///
/// Orca Whirlpool 目前沒有已知的 CPI event log，返回 None
pub fn parse_orca_whirlpool_inner_instruction_data(
    _discriminator: &[u8],
    _data: &[u8],
    _metadata: EventMetadata,
) -> Option<DexEvent> {
    None
}

/// 解析 Orca Whirlpool 帳戶資料
pub fn parse_orca_whirlpool_account_data(
    discriminator: &[u8],
    account: &crate::streaming::grpc::AccountPretty,
    metadata: crate::streaming::event_parser::common::EventMetadata,
) -> Option<crate::streaming::event_parser::DexEvent> {
    match discriminator {
        discriminators::WHIRLPOOL_ACCOUNT => parse_whirlpool_account(account, metadata),
        discriminators::TICK_ARRAY_ACCOUNT => parse_tick_array_account(account, metadata),
        _ => None,
    }
}

/// 解析 SwapV2 指令
/// SwapV2 帳戶佈局 (15 accounts):
/// 0: token_program_a, 1: token_program_b, 2: memo_program, 3: token_authority,
/// 4: whirlpool, 5: token_mint_a, 6: token_mint_b, 7: token_owner_account_a,
/// 8: token_vault_a, 9: token_owner_account_b, 10: token_vault_b,
/// 11: tick_array_0, 12: tick_array_1, 13: tick_array_2, 14: oracle
fn parse_swap_v2_instruction(
    data: &[u8],
    accounts: &[Pubkey],
    mut metadata: EventMetadata,
) -> Option<DexEvent> {
    metadata.event_type = EventType::OrcaWhirlpoolSwapV2;

    // amount(8) + other_amount_threshold(8) + sqrt_price_limit(16) + amount_specified_is_input(1) + a_to_b(1) = 34
    if data.len() < 34 || accounts.len() < 15 {
        return None;
    }

    let amount = read_u64_le(data, 0)?;
    let other_amount_threshold = read_u64_le(data, 8)?;
    let sqrt_price_limit = u128::from_le_bytes(data[16..32].try_into().ok()?);
    let amount_specified_is_input = data[32] != 0;
    let a_to_b = data[33] != 0;

    Some(DexEvent::OrcaWhirlpoolSwapV2Event(OrcaWhirlpoolSwapV2Event {
        metadata,
        amount,
        other_amount_threshold,
        sqrt_price_limit,
        amount_specified_is_input,
        a_to_b,
        token_program_a: accounts[0],
        token_program_b: accounts[1],
        memo_program: accounts[2],
        token_authority: accounts[3],
        whirlpool: accounts[4],
        token_mint_a: accounts[5],
        token_mint_b: accounts[6],
        token_owner_account_a: accounts[7],
        token_vault_a: accounts[8],
        token_owner_account_b: accounts[9],
        token_vault_b: accounts[10],
        tick_array_0: accounts[11],
        tick_array_1: accounts[12],
        tick_array_2: accounts[13],
        oracle: accounts[14],
    }))
}

/// 解析 Whirlpool 帳戶資料
///
/// Layout (after 8-byte Anchor discriminator):
///   whirlpools_config: Pubkey (32)  offset 8
///   whirlpool_bump: [u8; 1]         offset 40
///   tick_spacing: u16               offset 41
///   tick_spacing_seed: [u8; 2]      offset 43
///   fee_rate: u16                   offset 45
///   protocol_fee_rate: u16          offset 47
///   liquidity: u128                 offset 49
///   sqrt_price: u128                offset 65
///   tick_current_index: i32         offset 81
///   protocol_fee_owed_a: u64        offset 85
///   protocol_fee_owed_b: u64        offset 93
///   token_mint_a: Pubkey (32)       offset 101
///   token_vault_a: Pubkey (32)      offset 133
///   fee_growth_global_a: u128       offset 165
///   token_mint_b: Pubkey (32)       offset 181
///   token_vault_b: Pubkey (32)      offset 213
///   fee_growth_global_b: u128       offset 245
///   reward_last_updated_timestamp: u64  offset 261
///   reward_infos: [3]               offset 269 (3 x 128 bytes)
fn parse_whirlpool_account(
    account: &crate::streaming::grpc::AccountPretty,
    mut metadata: EventMetadata,
) -> Option<DexEvent> {
    metadata.event_type = EventType::AccountOrcaWhirlpool;

    if account.data.len() < ORCA_WHIRLPOOL_ACCOUNT_MIN_SIZE {
        return None;
    }

    let data = &account.data;

    let whirlpools_config = Pubkey::try_from(&data[8..40]).ok()?;
    let whirlpool_bump = [data[40]];
    let tick_spacing = u16::from_le_bytes(data[41..43].try_into().ok()?);
    let tick_spacing_seed = [data[43], data[44]];
    let fee_rate = u16::from_le_bytes(data[45..47].try_into().ok()?);
    let protocol_fee_rate = u16::from_le_bytes(data[47..49].try_into().ok()?);
    let liquidity = u128::from_le_bytes(data[49..65].try_into().ok()?);
    let sqrt_price = u128::from_le_bytes(data[65..81].try_into().ok()?);
    let tick_current_index = i32::from_le_bytes(data[81..85].try_into().ok()?);
    let protocol_fee_owed_a = u64::from_le_bytes(data[85..93].try_into().ok()?);
    let protocol_fee_owed_b = u64::from_le_bytes(data[93..101].try_into().ok()?);
    let token_mint_a = Pubkey::try_from(&data[101..133]).ok()?;
    let token_vault_a = Pubkey::try_from(&data[133..165]).ok()?;
    let fee_growth_global_a = u128::from_le_bytes(data[165..181].try_into().ok()?);
    let token_mint_b = Pubkey::try_from(&data[181..213]).ok()?;
    let token_vault_b = Pubkey::try_from(&data[213..245]).ok()?;
    let fee_growth_global_b = u128::from_le_bytes(data[245..261].try_into().ok()?);
    let reward_last_updated_timestamp = u64::from_le_bytes(data[261..269].try_into().ok()?);

    // Parse 3 reward infos (each 128 bytes: mint(32) + vault(32) + authority(32) + emissions(16) + growth(16))
    let mut reward_infos = [
        WhirlpoolRewardInfo::default(),
        WhirlpoolRewardInfo::default(),
        WhirlpoolRewardInfo::default(),
    ];
    let mut offset = 269;
    for ri in reward_infos.iter_mut() {
        if data.len() < offset + 128 {
            break;
        }
        ri.mint = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
        offset += 32;
        ri.vault = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
        offset += 32;
        ri.authority = Pubkey::try_from(&data[offset..offset + 32]).ok()?;
        offset += 32;
        ri.emissions_per_second_x64 = u128::from_le_bytes(data[offset..offset + 16].try_into().ok()?);
        offset += 16;
        ri.growth_global_x64 = u128::from_le_bytes(data[offset..offset + 16].try_into().ok()?);
        offset += 16;
    }

    Some(DexEvent::OrcaWhirlpoolAccountEvent(OrcaWhirlpoolAccountEvent {
        metadata,
        pubkey: account.pubkey,
        whirlpools_config,
        whirlpool_bump,
        tick_spacing,
        tick_spacing_seed,
        fee_rate,
        protocol_fee_rate,
        liquidity,
        sqrt_price,
        tick_current_index,
        protocol_fee_owed_a,
        protocol_fee_owed_b,
        token_mint_a,
        token_vault_a,
        fee_growth_global_a,
        token_mint_b,
        token_vault_b,
        fee_growth_global_b,
        reward_last_updated_timestamp,
        reward_infos,
    }))
}

/// 解析 TickArray 帳戶資料
///
/// Layout (after 8-byte Anchor discriminator):
///   start_tick_index: i32          offset 8
///   ticks: [Tick; 88]              offset 12 (each tick = 113 bytes)
///   whirlpool: Pubkey (32)         offset 12 + 88*113 = 9956
fn parse_tick_array_account(
    account: &crate::streaming::grpc::AccountPretty,
    mut metadata: EventMetadata,
) -> Option<DexEvent> {
    metadata.event_type = EventType::AccountOrcaWhirlpoolTickArray;

    if account.data.len() < ORCA_WHIRLPOOL_TICK_ARRAY_ACCOUNT_MIN_SIZE {
        return None;
    }

    let data = &account.data;

    let start_tick_index = i32::from_le_bytes(data[8..12].try_into().ok()?);

    // Parse 88 ticks, each 113 bytes starting at offset 12
    let mut ticks = Vec::with_capacity(TICK_ARRAY_SIZE);
    let mut offset = 12;
    for _ in 0..TICK_ARRAY_SIZE {
        if data.len() < offset + TICK_SIZE {
            break;
        }
        let initialized = data[offset] != 0;
        offset += 1;
        let liquidity_net = i128::from_le_bytes(data[offset..offset + 16].try_into().ok()?);
        offset += 16;
        let liquidity_gross = u128::from_le_bytes(data[offset..offset + 16].try_into().ok()?);
        offset += 16;
        let fee_growth_outside_a = u128::from_le_bytes(data[offset..offset + 16].try_into().ok()?);
        offset += 16;
        let fee_growth_outside_b = u128::from_le_bytes(data[offset..offset + 16].try_into().ok()?);
        offset += 16;
        let mut reward_growths_outside = [0u128; 3];
        for rg in reward_growths_outside.iter_mut() {
            *rg = u128::from_le_bytes(data[offset..offset + 16].try_into().ok()?);
            offset += 16;
        }
        ticks.push(Tick {
            initialized,
            liquidity_net,
            liquidity_gross,
            fee_growth_outside_a,
            fee_growth_outside_b,
            reward_growths_outside,
        });
    }

    // Whirlpool pubkey at offset 12 + 88 * 113
    let whirlpool_offset = 12 + TICK_ARRAY_SIZE * TICK_SIZE;
    let whirlpool = if data.len() >= whirlpool_offset + 32 {
        Pubkey::try_from(&data[whirlpool_offset..whirlpool_offset + 32]).ok()?
    } else {
        Pubkey::default()
    };

    Some(DexEvent::OrcaWhirlpoolTickArrayAccountEvent(OrcaWhirlpoolTickArrayAccountEvent {
        metadata,
        pubkey: account.pubkey,
        start_tick_index,
        ticks,
        whirlpool,
    }))
}
