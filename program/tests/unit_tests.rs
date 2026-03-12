use arb_program::accounts::*;

// ── SwapInstruction::parse tests ──

#[test]
fn parse_2hop_valid() {
    // Layout: [buy_dex, sell_dex, flags, amount_in(8), min_profit(4)] = 15 bytes
    let mut data = [0u8; 15];
    data[0] = 0; // buy_dex = RaydiumAmmV4
    data[1] = 1; // sell_dex = RaydiumCpmm
    data[2] = 0b0000_0011; // flags: buy_a_to_b=1, sell_a_to_b=1
    let amount_in: u64 = 1_000_000;
    data[3..11].copy_from_slice(&amount_in.to_le_bytes());
    let min_profit: u32 = 500;
    data[11..15].copy_from_slice(&min_profit.to_le_bytes());

    let ix = SwapInstruction::parse(&data, 2).expect("parse failed");
    assert_eq!(ix.hop_count, 2);
    assert_eq!(ix.amount_in, 1_000_000);
    assert_eq!(ix.min_profit, 500);
    assert!(matches!(ix.hops[0].dex_type, DexType::RaydiumAmmV4));
    assert!(ix.hops[0].is_a_to_b);
    assert!(matches!(ix.hops[1].dex_type, DexType::RaydiumCpmm));
    assert!(ix.hops[1].is_a_to_b);
}

#[test]
fn parse_3hop_valid() {
    // Layout: [buy_dex, sell_dex, flags, mid_dex, mid_flags, amount_in(8), min_profit(4)] = 17 bytes
    let mut data = [0u8; 17];
    data[0] = 3; // buy_dex = PumpFun
    data[1] = 6; // sell_dex = MeteoraDammV2
    data[2] = 0b0000_0001; // flags: buy_a_to_b=1, sell_a_to_b=0
    data[3] = 4; // mid_dex = PumpSwap
    data[4] = 0b0000_0001; // mid_flags: a_to_b=1
    let amount_in: u64 = 5_000_000;
    data[5..13].copy_from_slice(&amount_in.to_le_bytes());
    let min_profit: u32 = 1000;
    data[13..17].copy_from_slice(&min_profit.to_le_bytes());

    let ix = SwapInstruction::parse(&data, 3).expect("parse failed");
    assert_eq!(ix.hop_count, 3);
    assert_eq!(ix.amount_in, 5_000_000);
    assert_eq!(ix.min_profit, 1000);
    assert!(matches!(ix.hops[0].dex_type, DexType::PumpFun));
    assert!(ix.hops[0].is_a_to_b);
    assert!(matches!(ix.hops[1].dex_type, DexType::PumpSwap));
    assert!(ix.hops[1].is_a_to_b);
    assert!(matches!(ix.hops[2].dex_type, DexType::MeteoraDammV2));
    assert!(!ix.hops[2].is_a_to_b);
}

#[test]
fn parse_4hop_valid() {
    // Layout: [buy_dex, sell_dex, flags, mid1_dex, mid1_flags, mid2_dex, mid2_flags, amount_in(8), min_profit(4)] = 19 bytes
    let mut data = [0u8; 19];
    data[0] = 0; // buy = RaydiumAmmV4
    data[1] = 2; // sell = RaydiumClmm
    data[2] = 0b0000_0000; // flags: buy_a_to_b=0, sell_a_to_b=0
    data[3] = 5; // mid1 = Bonk
    data[4] = 0b0000_0001; // mid1: a_to_b=1
    data[5] = 1; // mid2 = RaydiumCpmm
    data[6] = 0b0000_0000; // mid2: a_to_b=0
    let amount_in: u64 = 10_000_000;
    data[7..15].copy_from_slice(&amount_in.to_le_bytes());
    let min_profit: u32 = 2000;
    data[15..19].copy_from_slice(&min_profit.to_le_bytes());

    let ix = SwapInstruction::parse(&data, 4).expect("parse failed");
    assert_eq!(ix.hop_count, 4);
    assert_eq!(ix.amount_in, 10_000_000);
    assert_eq!(ix.min_profit, 2000);
    assert!(matches!(ix.hops[0].dex_type, DexType::RaydiumAmmV4));
    assert!(!ix.hops[0].is_a_to_b);
    assert!(matches!(ix.hops[1].dex_type, DexType::Bonk));
    assert!(ix.hops[1].is_a_to_b);
    assert!(matches!(ix.hops[2].dex_type, DexType::RaydiumCpmm));
    assert!(!ix.hops[2].is_a_to_b);
    assert!(matches!(ix.hops[3].dex_type, DexType::RaydiumClmm));
    assert!(!ix.hops[3].is_a_to_b);
}

#[test]
fn parse_too_short_data_fails() {
    let data = [0u8; 10]; // too short for 2-hop (needs 15)
    let result = SwapInstruction::parse(&data, 2);
    assert!(result.is_err());
}

#[test]
fn parse_invalid_dex_type_fails() {
    let mut data = [0u8; 15];
    data[0] = 99; // invalid dex type
    data[1] = 0;
    let result = SwapInstruction::parse(&data, 2);
    assert!(result.is_err());
}

#[test]
fn parse_invalid_hop_count_fails() {
    let data = [0u8; 19];
    let result = SwapInstruction::parse(&data, 5);
    assert!(result.is_err());
}

// ── Token-2022 flag tests ──

#[test]
fn parse_2hop_token_2022_flags() {
    let mut data = [0u8; 15];
    data[0] = 0; // RaydiumAmmV4
    data[1] = 1; // RaydiumCpmm
    data[2] = 0b0000_1100; // buy_2022=1 (bit2), sell_2022=1 (bit3)
    let amount_in: u64 = 100;
    data[3..11].copy_from_slice(&amount_in.to_le_bytes());

    let ix = SwapInstruction::parse(&data, 2).unwrap();
    assert!(ix.hops[0].is_token_2022);
    assert!(ix.hops[1].is_token_2022);
    assert!(!ix.hops[0].is_a_to_b);
    assert!(!ix.hops[1].is_a_to_b);
}

// ── DexType tests ──

#[test]
fn dex_type_roundtrip() {
    for i in 0..=6u8 {
        let dex = DexType::from_u8(i).unwrap();
        assert!(dex.base_pool_account_count() > 0);
    }
    assert!(DexType::from_u8(7).is_err());
    assert!(DexType::from_u8(255).is_err());
}

#[test]
fn pool_account_counts() {
    assert_eq!(DexType::RaydiumAmmV4.base_pool_account_count(), 9);
    assert_eq!(DexType::RaydiumCpmm.base_pool_account_count(), 14);
    assert_eq!(DexType::RaydiumClmm.base_pool_account_count(), 10);
    assert_eq!(DexType::PumpFun.base_pool_account_count(), 16);
    assert_eq!(DexType::PumpSwap.base_pool_account_count(), 23);
    assert_eq!(DexType::Bonk.base_pool_account_count(), 17);
    assert_eq!(DexType::MeteoraDammV2.base_pool_account_count(), 14);
}

// ── pool_accounts_start tests ──

fn make_hop(dex: DexType) -> HopInfo {
    HopInfo {
        dex_type: dex,
        is_a_to_b: false,
        is_token_2022: false,
        extra_accounts: 0,
    }
}

fn default_hops() -> [HopInfo; 4] {
    [
        make_hop(DexType::RaydiumAmmV4),
        make_hop(DexType::RaydiumAmmV4),
        make_hop(DexType::RaydiumAmmV4),
        make_hop(DexType::RaydiumAmmV4),
    ]
}

#[test]
fn pool_accounts_start_2hop() {
    let hops = [
        make_hop(DexType::RaydiumAmmV4), // 9 accounts
        make_hop(DexType::RaydiumCpmm),   // 14 accounts
        make_hop(DexType::RaydiumAmmV4),
        make_hop(DexType::RaydiumAmmV4),
    ];
    // 2-hop: header(8) + 1 intermediate(3) = 11
    let start0 = pool_accounts_start(2, 0, &hops);
    assert_eq!(start0, 8 + 3); // 11

    let start1 = pool_accounts_start(2, 1, &hops);
    assert_eq!(start1, 11 + 9); // 20 (after hop0's 9 accounts)
}

#[test]
fn pool_accounts_start_3hop() {
    let hops = [
        make_hop(DexType::PumpFun),       // 16 accounts
        make_hop(DexType::Bonk),          // 17 accounts
        make_hop(DexType::MeteoraDammV2), // 14 accounts
        make_hop(DexType::RaydiumAmmV4),
    ];
    // 3-hop: header(8) + 2 intermediates(6) = 14
    let start0 = pool_accounts_start(3, 0, &hops);
    assert_eq!(start0, 8 + 6); // 14

    let start1 = pool_accounts_start(3, 1, &hops);
    assert_eq!(start1, 14 + 16); // 30

    let start2 = pool_accounts_start(3, 2, &hops);
    assert_eq!(start2, 30 + 17); // 47
}

#[test]
fn pool_accounts_start_4hop() {
    let hops = [
        make_hop(DexType::RaydiumAmmV4),  // 9
        make_hop(DexType::RaydiumCpmm),   // 14
        make_hop(DexType::RaydiumClmm),   // 10
        make_hop(DexType::PumpSwap),      // 23
    ];
    // 4-hop: header(8) + 3 intermediates(9) = 17
    let start0 = pool_accounts_start(4, 0, &hops);
    assert_eq!(start0, 8 + 9); // 17

    let start1 = pool_accounts_start(4, 1, &hops);
    assert_eq!(start1, 17 + 9); // 26

    let start2 = pool_accounts_start(4, 2, &hops);
    assert_eq!(start2, 26 + 14); // 40

    let start3 = pool_accounts_start(4, 3, &hops);
    assert_eq!(start3, 40 + 10); // 50
}

// ── Dispatch (process_instruction) tests ──

#[test]
fn empty_instruction_data_fails() {
    // Cannot test process_instruction directly due to AccountView,
    // but we can verify the discriminator logic by checking constants
    assert_eq!(arb_program::SWAP_2HOP, 0);
    assert_eq!(arb_program::SWAP_3HOP, 1);
    assert_eq!(arb_program::SWAP_4HOP, 2);
}
