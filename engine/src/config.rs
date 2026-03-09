use solana_sdk::pubkey::Pubkey;

pub const WSOL_MINT: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
pub const USDC_MINT: Pubkey = solana_sdk::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USD1_MINT: Pubkey = solana_sdk::pubkey!("Ue2JCRrno2aAWT7gmjEwpsK5HQGM1ZMqSDZvMNo3S5e");

pub struct EngineConfig {
    pub base_mints: Vec<Pubkey>,
    pub max_hops: u8,
    pub warmup_secs: u64,
    pub warmup_pool_count: usize,
    pub full_scan_interval_secs: u64,
    pub min_reserve_lamports: u64,
    pub max_hop_fee: f64,
    pub min_profit_lamports: u64,
    pub max_input_lamports: u64,
    pub ternary_iterations: u32,
    pub probe_amount_lamports: u64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            base_mints: vec![WSOL_MINT, USDC_MINT, USD1_MINT],
            max_hops: 4,
            warmup_secs: 30,
            warmup_pool_count: 1000,
            full_scan_interval_secs: 5,
            min_reserve_lamports: 10_000_000_000,
            max_hop_fee: 0.02,
            min_profit_lamports: 1_000_000,
            max_input_lamports: 100_000_000_000,
            ternary_iterations: 10,
            probe_amount_lamports: 1_000_000_000,
        }
    }
}
