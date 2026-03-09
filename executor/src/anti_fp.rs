use rand::Rng;
use solana_sdk::pubkey::Pubkey;
use std::sync::LazyLock;

/// Jito tip accounts — pre-parsed at startup
static JITO_TIP_PUBKEYS: LazyLock<[Pubkey; 8]> = LazyLock::new(|| {
    [
        solana_sdk::pubkey!("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5"),
        solana_sdk::pubkey!("HFqU5x63VTqvQss8hp11i4bVqkfRtQ7NmXwkiNPLz4xG"),
        solana_sdk::pubkey!("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY"),
        solana_sdk::pubkey!("ADaUMid9yfUytqMBgopwjb2DTLSLo4G9hp12gJZTm1Xw"),
        solana_sdk::pubkey!("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
        solana_sdk::pubkey!("ADuUkR4vqLUMWXxW9gh6D6L8pMSga2WWP4N4G2Cj6ixc"),
        solana_sdk::pubkey!("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL6JR3"),
        solana_sdk::pubkey!("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"),
    ]
});

pub fn random_tip_account() -> Pubkey {
    let mut rng = rand::thread_rng();
    let idx = rng.gen_range(0..8);
    JITO_TIP_PUBKEYS[idx]
}

pub fn random_fee_collector(collectors: &[Pubkey]) -> Pubkey {
    if collectors.is_empty() {
        // Fallback: use first Jito tip account as fee collector
        return JITO_TIP_PUBKEYS[0];
    }
    let mut rng = rand::thread_rng();
    let idx = rng.gen_range(0..collectors.len());
    collectors[idx]
}

pub fn jittered_cu(base_cu: u32, jitter_range: u32) -> u32 {
    let mut rng = rand::thread_rng();
    base_cu + rng.gen_range(0..jitter_range)
}

pub fn estimate_cu(dex_types: &[u8]) -> u32 {
    let mut cu: u32 = 100; // program overhead
    for dex in dex_types {
        cu += match dex {
            0 => 35_000, // RaydiumAmmV4
            1 => 35_000, // RaydiumCpmm
            2 => 80_000, // RaydiumClmm
            3 => 30_000, // PumpFun
            4 => 35_000, // PumpSwap
            5 => 30_000, // Bonk
            6 => 45_000, // MeteoraDammV2
            _ => 50_000, // unknown fallback
        };
    }
    cu
}
