//! Full pipeline example: Stage 1 (PoolStreamer) -> Stage 2 (Engine) -> Stage 3 (Executor)
//!
//! Demonstrates how the three stages connect together:
//!   1. PoolStreamer produces PoolUpdate events from on-chain data
//!   2. Engine consumes PoolUpdates, discovers arbitrage opportunities
//!   3. Executor receives opportunities, builds & sends transactions
//!
//! Usage:
//!   CONFIG_PATH=executor/config.toml RPC_URL=https://api.mainnet-beta.solana.com \
//!     cargo run --example full_pipeline
//!
//! Note: This example requires a valid config file and RPC endpoint.
//! Without a live gRPC feed it will start all stages but receive no events.

use arb_engine::config::EngineConfig;
use arb_engine::engine::Engine;
use arb_executor::config::ExecutorConfigFile;
use arb_executor::executor::Executor;
use solana_sdk::signer::keypair::Keypair;
use solana_streamer_sdk::pool::streamer::{PoolStreamer, PoolStreamerConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // ── Configuration ───────────────────────────────────────────────────
    let config_path = std::env::var("CONFIG_PATH")
        .unwrap_or_else(|_| "executor/config.toml".to_string());
    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

    let executor_config = ExecutorConfigFile::load(&config_path)?;
    println!("Loaded executor config from: {}", config_path);

    let engine_config = EngineConfig::default();
    println!(
        "Engine config: max_hops={}, min_profit={} lamports",
        engine_config.max_hops, engine_config.min_profit_lamports,
    );

    // ── Stage 1: PoolStreamer ───────────────────────────────────────────
    // Produces PoolUpdate events from on-chain account data.
    let streamer_config = PoolStreamerConfig {
        rpc_url: rpc_url.clone(),
        ..Default::default()
    };
    let (_pool_streamer, update_rx) = PoolStreamer::new(streamer_config);
    println!("Stage 1 (PoolStreamer) initialized");

    // In production, PoolStreamer::on_event() is called from a gRPC subscription loop:
    //
    //   tokio::spawn(async move {
    //       // subscribe to Yellowstone gRPC for DexEvents
    //       while let Some(event) = grpc_stream.next().await {
    //           pool_streamer.on_event(event).await;
    //       }
    //   });

    // ── Stage 2: Engine ─────────────────────────────────────────────────
    // Consumes PoolUpdates, builds an arbitrage graph, and emits Opportunities.
    let (engine, opp_rx) = Engine::new(engine_config, update_rx);
    println!("Stage 2 (Engine) initialized");

    // ── Stage 3: Executor ───────────────────────────────────────────────
    // Receives Opportunities, builds transactions, and sends them via
    // Jito bundles / Flashblock / direct RPC.
    let payer = Keypair::new(); // In production, load from file or env
    let executor = Executor::new(executor_config, opp_rx, payer, &rpc_url).await?;
    println!("Stage 3 (Executor) initialized — all connections pre-established");

    // ── Run ─────────────────────────────────────────────────────────────
    println!("\nPipeline ready. Running all stages concurrently...");
    println!("(Press Ctrl-C to stop)\n");

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
