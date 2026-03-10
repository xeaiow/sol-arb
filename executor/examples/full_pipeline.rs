//! Full pipeline: Stage 1 (PoolStreamer) -> Stage 2 (Engine) -> Stage 3 (Executor)
//!
//! Usage:
//!   RUST_LOG=info cargo run --release --example full_pipeline
//!
//! All settings are read from config.toml (default: config.toml).
//! Override config path: CONFIG_PATH=path/to/config.toml

use std::sync::Arc;

use arb_engine::engine::Engine;
use arb_executor::config::ExecutorConfigFile;
use arb_executor::executor::Executor;
use solana_sdk::signer::keypair::read_keypair_file;
use solana_streamer_sdk::pool::streamer::{PoolStreamer, PoolStreamerConfig};
use solana_streamer_sdk::streaming::event_parser::protocols::{
    bonk::parser::BONK_PROGRAM_ID, meteora_damm_v2::parser::METEORA_DAMM_V2_PROGRAM_ID,
    pumpfun::parser::PUMPFUN_PROGRAM_ID, pumpswap::parser::PUMPSWAP_PROGRAM_ID,
    raydium_amm_v4::parser::RAYDIUM_AMM_V4_PROGRAM_ID,
    raydium_clmm::parser::RAYDIUM_CLMM_PROGRAM_ID,
    raydium_cpmm::parser::RAYDIUM_CPMM_PROGRAM_ID,
};
use solana_streamer_sdk::streaming::event_parser::Protocol;
use solana_streamer_sdk::streaming::yellowstone_grpc::{
    AccountFilter, TransactionFilter, YellowstoneGrpc,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Load config ──────────────────────────────────────────────────────
    let config_path = std::env::var("CONFIG_PATH")
        .unwrap_or_else(|_| "config.toml".to_string());
    let executor_config = ExecutorConfigFile::load(&config_path)?;

    // 從 config 設定 log level (若環境變數未設定)
    if std::env::var("RUST_LOG").is_err() {
        let level = executor_config.general.as_ref()
            .and_then(|g| g.log_level.clone())
            .unwrap_or_else(|| "info".to_string());
        std::env::set_var("RUST_LOG", &level);
    }
    env_logger::init();

    println!("Config loaded: {}", config_path);

    // ── Resolve general settings (config -> env -> defaults) ─────────────
    let general = executor_config.general.as_ref();

    let keypair_path = general.and_then(|g| g.keypair_path.clone())
        .or_else(|| std::env::var("KEYPAIR_PATH").ok())
        .unwrap_or_else(|| "~/.config/solana/id.json".to_string());
    // 展開 ~ 為 $HOME
    let keypair_path = if keypair_path.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{}{}", home, &keypair_path[1..])
    } else {
        keypair_path
    };

    let rpc_url = general.and_then(|g| g.rpc_url.clone())
        .or_else(|| std::env::var("RPC_URL").ok())
        .unwrap_or_else(|| "https://api.mainnet-beta.solana.com".to_string());

    let grpc_endpoint = general.and_then(|g| g.grpc_endpoint.clone())
        .or_else(|| std::env::var("GRPC_ENDPOINT").ok())
        .unwrap_or_else(|| "https://solana-yellowstone-grpc.publicnode.com:443".to_string());

    let grpc_token = general.and_then(|g| g.grpc_token.clone())
        .or_else(|| std::env::var("GRPC_TOKEN").ok())
        .filter(|s| !s.is_empty());

    let payer = read_keypair_file(&keypair_path)
        .map_err(|e| anyhow::anyhow!("Failed to read keypair {}: {}", keypair_path, e))?;
    println!("Payer: {}", solana_sdk::signer::Signer::pubkey(&payer));

    let engine_config = executor_config.engine_config();
    println!(
        "Engine: max_hops={}, min_profit={} lamports",
        engine_config.max_hops, engine_config.min_profit_lamports,
    );

    // ── Stage 1: PoolStreamer ───────────────────────────────────────────
    let streamer_config = PoolStreamerConfig {
        rpc_url: rpc_url.clone(),
        ..Default::default()
    };
    let (pool_streamer, update_rx) = PoolStreamer::new(streamer_config);
    let pool_streamer = Arc::new(pool_streamer);
    println!("Stage 1 (PoolStreamer) ready");

    // ── Stage 2: Engine ─────────────────────────────────────────────────
    let (engine, opp_rx) = Engine::new(engine_config, update_rx);
    println!("Stage 2 (Engine) ready");

    // ── Stage 3: Executor ───────────────────────────────────────────────
    let executor = Executor::new(executor_config, opp_rx, payer, &rpc_url).await?;
    println!("Stage 3 (Executor) ready");

    // ── Start gRPC subscription ─────────────────────────────────────────
    let grpc = Arc::new(YellowstoneGrpc::new(grpc_endpoint, grpc_token)?);

    let protocols = vec![
        Protocol::PumpFun,
        Protocol::PumpSwap,
        Protocol::Bonk,
        Protocol::RaydiumCpmm,
        Protocol::RaydiumClmm,
        Protocol::RaydiumAmmV4,
        Protocol::MeteoraDammV2,
    ];

    let account_include = vec![
        PUMPFUN_PROGRAM_ID.to_string(),
        PUMPSWAP_PROGRAM_ID.to_string(),
        BONK_PROGRAM_ID.to_string(),
        RAYDIUM_CPMM_PROGRAM_ID.to_string(),
        RAYDIUM_CLMM_PROGRAM_ID.to_string(),
        RAYDIUM_AMM_V4_PROGRAM_ID.to_string(),
        METEORA_DAMM_V2_PROGRAM_ID.to_string(),
    ];

    let transaction_filter = TransactionFilter {
        account_include: account_include.clone(),
        account_exclude: vec![],
        account_required: vec![],
    };

    let account_filter = AccountFilter {
        account: vec![],
        owner: account_include,
        filters: vec![],
    };

    // Wire gRPC events → PoolStreamer
    let streamer = pool_streamer.clone();
    grpc.subscribe_events_immediate(
        protocols,
        None,
        vec![transaction_filter],
        vec![account_filter],
        None,
        None,
        move |event| {
            let streamer = streamer.clone();
            tokio::spawn(async move {
                streamer.on_event(event).await;
            });
        },
    )
    .await?;

    println!("\n=== Pipeline running. Warming up... ===");
    println!("(Press Ctrl-C to stop)\n");

    // ── Run all stages concurrently ─────────────────────────────────────
    tokio::select! {
        _ = engine.run() => {
            println!("Engine exited");
        }
        _ = executor.run() => {
            println!("Executor exited");
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\nShutting down...");
        }
    }

    Ok(())
}
