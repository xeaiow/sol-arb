use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ExecutorConfigFile {
    pub executor: ExecutorConfig,
    pub jito: Option<JitoConfig>,
    pub flashblock: Option<FlashblockConfig>,
    pub astralane: Option<AstralaneConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ExecutorConfig {
    pub flashloan_enabled: bool,
    pub program_id: String,
    pub alt_address: String,
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
    pub endpoints: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AstralaneConfig {
    pub enabled: bool,
    pub api_key: String,
    pub cu_price_percentage: u32,
    pub endpoints: Vec<String>,
}

impl ExecutorConfigFile {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }
}
