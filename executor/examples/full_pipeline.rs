//! Full pipeline: Stage 1 (PoolStreamer) -> Stage 2 (Engine) -> Stage 3 (Executor)
//!
//! Usage:
//!   CONFIG_PATH=executor/config.toml KEYPAIR_PATH=~/.config/solana/id.json \
//!     RUST_LOG=info cargo run --example full_pipeline
//!
//! Environment variables:
//!   CONFIG_PATH   - path to executor config.toml (default: executor/config.toml)
//!   KEYPAIR_PATH  - path to payer keypair JSON (default: ~/.config/solana/id.json)
//!   RPC_URL       - Solana RPC endpoint (default: https://api.mainnet-beta.solana.com)
//!   GRPC_ENDPOINT - Yellowstone gRPC endpoint (default: https://solana-yellowstone-grpc.publicnode.com:443)
//!   GRPC_TOKEN    - optional gRPC auth token

use std::sync::Arc;

use arb_engine::config::EngineConfig;
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
    env_logger::init();

    // ── Configuration ───────────────────────────────────────────────────
    let config_path = std::env::var("CONFIG_PATH")
        .unwrap_or_else(|_| "executor/config.toml".to_string());
    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());
    let grpc_endpoint = std::env::var("GRPC_ENDPOINT")
        .unwrap_or_else(|_| "https://solana-yellowstone-grpc.publicnode.com:443".to_string());
    let grpc_token = std::env::var("GRPC_TOKEN").ok();

    let keypair_path = std::env::var("KEYPAIR_PATH").unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{}/.config/solana/id.json", home)
    });

    let executor_config = ExecutorConfigFile::load(&config_path)?;
    println!("Config loaded: {}", config_path);

    let payer = read_keypair_file(&keypair_path)
        .map_err(|e| anyhow::anyhow!("Failed to read keypair {}: {}", keypair_path, e))?;
    println!("Payer: {}", solana_sdk::signer::Signer::pubkey(&payer));

    let engine_config = EngineConfig::default();
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
