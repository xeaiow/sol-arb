use std::sync::Arc;
use std::time::Duration;

use solana_streamer_sdk::pool::state::PoolMath;
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
    let grpc_endpoint = std::env::var("GRPC_ENDPOINT")
        .unwrap_or_else(|_| "https://solana-yellowstone-grpc.publicnode.com:443".to_string());
    let grpc_token = std::env::var("GRPC_TOKEN").ok();
    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

    // Create pool streamer
    let config = PoolStreamerConfig {
        rpc_url,
        update_channel_size: 4096,
    };
    let (pool_streamer, mut update_rx) = PoolStreamer::new(config);
    let pool_streamer = Arc::new(pool_streamer);

    // Spawn update consumer (Stage 2 would consume this channel)
    tokio::spawn(async move {
        while let Some(update) = update_rx.recv().await {
            match &update.math {
                PoolMath::ConstantProduct {
                    reserve_a,
                    reserve_b,
                    fee_numerator,
                    fee_denominator,
                } => {
                    println!(
                        "[slot {}] Pool {} ({:?}) ConstantProduct: reserve_a={}, reserve_b={}, fee={}/{}",
                        update.slot, update.pool_address, update.dex_type,
                        reserve_a, reserve_b, fee_numerator, fee_denominator
                    );
                }
                PoolMath::BondingCurve {
                    virtual_token_reserves,
                    virtual_sol_reserves,
                    ..
                } => {
                    println!(
                        "[slot {}] Pool {} ({:?}) BondingCurve: token={}, sol={}",
                        update.slot, update.pool_address, update.dex_type,
                        virtual_token_reserves, virtual_sol_reserves
                    );
                }
                PoolMath::Concentrated {
                    sqrt_price_x64,
                    liquidity,
                    tick_current,
                    ..
                } => {
                    println!(
                        "[slot {}] Pool {} ({:?}) CLMM: sqrt_price={}, liquidity={}, tick={}",
                        update.slot, update.pool_address, update.dex_type,
                        sqrt_price_x64, liquidity, tick_current
                    );
                }
                PoolMath::MeteoraDlmm {
                    active_id,
                    bin_step,
                    ref bin_arrays,
                    ..
                } => {
                    println!(
                        "[slot {}] Pool {} ({:?}) DLMM: active_id={}, bin_step={}, bin_arrays={}",
                        update.slot, update.pool_address, update.dex_type,
                        active_id, bin_step, bin_arrays.len()
                    );
                }
            }
        }
    });

    // Create gRPC client
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

    // Account filter: listen to accounts owned by DEX programs
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

    // Periodically log pool count
    let streamer_ref = pool_streamer.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let new_accounts = streamer_ref.drain_pending_subscriptions().await;
            println!(
                "Pools tracked: {}, new accounts pending subscription: {}",
                streamer_ref.pool_count(),
                new_accounts.len()
            );
        }
    });

    println!("Pool streamer running. Discovering pools from transaction events...");
    println!("Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    println!(
        "Shutting down. Discovered {} pools total.",
        pool_streamer.pool_count()
    );

    Ok(())
}
