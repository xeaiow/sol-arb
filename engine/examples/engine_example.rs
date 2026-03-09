use std::sync::Arc;
use arb_engine::config::EngineConfig;
use arb_engine::engine::Engine;

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

    let grpc_endpoint = std::env::var("GRPC_ENDPOINT")
        .unwrap_or_else(|_| "https://solana-yellowstone-grpc.publicnode.com:443".to_string());
    let grpc_token = std::env::var("GRPC_TOKEN").ok();
    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

    // Stage 1: Pool Streamer
    let pool_config = PoolStreamerConfig {
        rpc_url,
        update_channel_size: 4096,
    };
    let (pool_streamer, update_rx) = PoolStreamer::new(pool_config);
    let pool_streamer = Arc::new(pool_streamer);

    // Stage 2: Route Engine
    let engine_config = EngineConfig::default();
    let (engine, mut opp_rx) = Engine::new(engine_config, update_rx);

    // Spawn opportunity consumer
    tokio::spawn(async move {
        while let Some(opp) = opp_rx.recv().await {
            println!(
                "[slot {}] OPPORTUNITY: {} hops, amount_in={}, profit={} lamports",
                opp.slot,
                opp.route.hops.len(),
                opp.amount_in,
                opp.expected_profit,
            );
            for (i, snap) in opp.pool_snapshots.iter().enumerate() {
                println!(
                    "  hop {}: {} ({:?}) {}",
                    i + 1,
                    snap.address,
                    snap.dex_type,
                    if snap.is_a_to_b { "A->B" } else { "B->A" },
                );
            }
        }
    });

    // Spawn engine
    tokio::spawn(async move {
        engine.run().await;
    });

    // Start gRPC subscription
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

    println!("Engine running. Warming up...");
    tokio::signal::ctrl_c().await?;
    println!("Shutting down.");

    Ok(())
}
