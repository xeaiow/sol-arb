use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ExecutorConfigFile {
    pub general: Option<GeneralConfig>,
    pub executor: ExecutorConfig,
    pub engine: Option<EngineConfigFile>,
    pub jito: Option<JitoConfig>,
    pub flashblock: Option<FlashblockConfig>,
    pub astralane: Option<AstralaneConfig>,
}

#[derive(Debug, Deserialize)]
pub struct GeneralConfig {
    pub keypair_path: Option<String>,
    pub rpc_url: Option<String>,
    pub grpc_endpoint: Option<String>,
    pub grpc_token: Option<String>,
    pub log_level: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EngineConfigFile {
    pub max_hops: Option<u8>,
    pub warmup_secs: Option<u64>,
    pub warmup_pool_count: Option<usize>,
    pub full_scan_interval_secs: Option<u64>,
    pub min_reserve_sol: Option<f64>,
    pub max_hop_fee: Option<f64>,
    pub min_profit_sol: Option<f64>,
    pub max_input_sol: Option<f64>,
    pub ternary_iterations: Option<u32>,
    pub probe_amount_sol: Option<f64>,
    pub max_profit_ratio: Option<f64>,
    pub base_mints: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct ExecutorConfig {
    pub flashloan_enabled: bool,
    pub program_id: String,
    pub alt_address: String,
    pub fallback_rpc_url: Option<String>,
    pub anti_fingerprint: AntiFingerprint,
}

#[derive(Debug, Deserialize)]
pub struct AntiFingerprint {
    pub cu_jitter_range: u32,
    pub fee_collectors_sol: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct JitoConfig {
    pub enabled: bool,
    pub block_engine_urls: Vec<String>,
    pub tip_percentage: u32,
    pub min_tip_lamports: u64,
    pub min_operator_profit_lamports: u64,
}

#[derive(Debug, Deserialize)]
pub struct FlashblockConfig {
    pub enabled: bool,
    pub api_key: String,
    pub cu_price_percentage: u32,
    /// Fixed priority fee in lamports. When set, overrides cu_price_percentage.
    pub priority_fee_lamports: Option<u64>,
    pub endpoints: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AstralaneConfig {
    pub enabled: bool,
    pub api_key: String,
    pub cu_price_percentage: u32,
    /// Fixed priority fee in lamports. When set, overrides cu_price_percentage.
    pub priority_fee_lamports: Option<u64>,
    pub endpoints: Vec<String>,
}

/// Convert SOL (f64) to lamports (u64)
fn sol_to_lamports(sol: f64) -> u64 {
    (sol * 1_000_000_000.0) as u64
}

impl ExecutorConfigFile {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }

    /// Build an EngineConfig from [engine] section, falling back to defaults.
    pub fn engine_config(&self) -> arb_engine::config::EngineConfig {
        let defaults = arb_engine::config::EngineConfig::default();
        let Some(ec) = &self.engine else {
            return defaults;
        };

        let base_mints = ec.base_mints.as_ref().map(|mints| {
            mints.iter()
                .filter_map(|s| s.parse::<solana_sdk::pubkey::Pubkey>().ok())
                .collect()
        }).unwrap_or(defaults.base_mints);

        arb_engine::config::EngineConfig {
            base_mints,
            max_hops: ec.max_hops.unwrap_or(defaults.max_hops),
            warmup_secs: ec.warmup_secs.unwrap_or(defaults.warmup_secs),
            warmup_pool_count: ec.warmup_pool_count.unwrap_or(defaults.warmup_pool_count),
            full_scan_interval_secs: ec.full_scan_interval_secs.unwrap_or(defaults.full_scan_interval_secs),
            min_reserve_lamports: ec.min_reserve_sol.map(sol_to_lamports).unwrap_or(defaults.min_reserve_lamports),
            max_hop_fee: ec.max_hop_fee.unwrap_or(defaults.max_hop_fee),
            min_profit_lamports: ec.min_profit_sol.map(sol_to_lamports).unwrap_or(defaults.min_profit_lamports),
            max_input_lamports: ec.max_input_sol.map(sol_to_lamports).unwrap_or(defaults.max_input_lamports),
            ternary_iterations: ec.ternary_iterations.unwrap_or(defaults.ternary_iterations),
            probe_amount_lamports: ec.probe_amount_sol.map(sol_to_lamports).unwrap_or(defaults.probe_amount_lamports),
            max_profit_ratio: ec.max_profit_ratio.unwrap_or(defaults.max_profit_ratio),
        }
    }
}
