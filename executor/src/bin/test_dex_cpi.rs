//! Test each DEX's CPI through our arb program with a tiny amount (0.0001 SOL).
//! Usage: cd executor && TEST_MODE=1 RUST_LOG=info cargo run --release --bin test_dex_cpi -- <dex>
//! Where <dex> = cpmm | clmm | whirlpool | dammv2 | pumpfun | ammv4

use std::sync::Arc;
use arrayvec::ArrayVec;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::keypair::read_keypair_file;
use solana_sdk::signer::Signer;

use arb_engine::opportunity::{Hop, Opportunity, PoolSnapshot, Route};
use arb_executor::config::ExecutorConfigFile;
use arb_executor::tx_builder::TxBuilder;
use arb_executor::sender::astralane::AstralaneSender;
use solana_streamer_sdk::pool::decoder;
use solana_streamer_sdk::pool::state::{DexType, PoolMath};

const RPC: &str = "http://45.157.234.194:8899";
const WSOL: &str = "So11111111111111111111111111111111111111112";

/// Build a 1-hop roundtrip: buy on target DEX (SOL→token), then sell back (token→SOL).
/// This tests the CPI for that DEX.
async fn test_single_dex(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
    config: &ExecutorConfigFile,
    pool_address: &str,
    dex_type: DexType,
    dex_name: &str,
) -> anyhow::Result<()> {
    let pool_addr: Pubkey = pool_address.parse()?;
    let wsol: Pubkey = WSOL.parse()?;

    println!("\n=== Testing {} ===", dex_name);
    println!("Pool: {}", pool_addr);

    // Decode pool
    let pool_data = rpc.get_account_data(&pool_addr).await?;
    let pool_state = match dex_type {
        DexType::RaydiumCpmm => decoder::raydium_cpmm::decode_bytes(&pool_addr, &pool_data),
        DexType::RaydiumClmm => decoder::raydium_clmm::decode_bytes(&pool_addr, &pool_data),
        DexType::OrcaWhirlpool => decoder::orca_whirlpool::decode_bytes(&pool_addr, &pool_data),
        DexType::MeteoraDammV2 => decoder::meteora_damm_v2::decode_bytes(&pool_addr, &pool_data),
        DexType::PumpFun => decoder::pumpfun::decode_bytes(&pool_addr, &pool_data),
        DexType::RaydiumAmmV4 => decoder::raydium_amm_v4::decode_bytes(&pool_addr, &pool_data),
        _ => None,
    }.ok_or_else(|| anyhow::anyhow!("Failed to decode {} pool", dex_name))?;

    println!("  mint_a: {}", pool_state.mint_a);
    println!("  mint_b: {}", pool_state.mint_b);

    // Determine direction: we want SOL in, token out
    let sol_is_a = pool_state.mint_a.to_string().starts_with("So1111");
    let sol_is_b = pool_state.mint_b.to_string().starts_with("So1111");
    if !sol_is_a && !sol_is_b {
        println!("  ⚠️ Neither mint is SOL, skipping");
        return Ok(());
    }
    // Buy = SOL → token
    let is_a_to_b_buy = sol_is_a; // if SOL is mint_a, buying means a→b

    // Build accounts (scanner style)
    let mut accounts = vec![pool_addr];
    if let Some(va) = pool_state.vault_a { accounts.push(va); }
    if let Some(vb) = pool_state.vault_b { accounts.push(vb); }
    accounts.extend_from_slice(&pool_state.extra_accounts);

    // Detect Token-2022
    let token_mint = if sol_is_a { pool_state.mint_b } else { pool_state.mint_a };
    let mint_is_2022 = match rpc.get_account(&token_mint).await {
        Ok(acct) => acct.owner.to_string() == "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
        Err(_) => false,
    };
    let (mint_a_is_2022, mint_b_is_2022) = if sol_is_a {
        (false, mint_is_2022)
    } else {
        (mint_is_2022, false)
    };

    println!("  is_a_to_b (buy): {}, token_2022: {}", is_a_to_b_buy, mint_is_2022);

    // For CLMM/Whirlpool: fetch tick arrays and add to accounts
    if dex_type == DexType::RaydiumClmm {
        if let PoolMath::Concentrated { tick_current, tick_spacing, .. } = &pool_state.math {
            let starts = decoder::raydium_clmm::tick_array_start_indices(*tick_current, *tick_spacing);
            for start in starts.iter().take(3) {
                if let Some(pda) = decoder::raydium_clmm::tick_array_pda(&pool_addr, *start) {
                    if rpc.get_account(&pda).await.is_ok() {
                        accounts.push(pda);
                    }
                }
            }
            println!("  Loaded {} tick arrays", accounts.len() - 3 - pool_state.extra_accounts.len());
        }
    }
    if dex_type == DexType::OrcaWhirlpool {
        if let PoolMath::Concentrated { tick_current, tick_spacing, .. } = &pool_state.math {
            let starts = decoder::orca_whirlpool::tick_array_start_indices(*tick_current, *tick_spacing);
            for start in starts.iter().take(3) {
                if let Some(pda) = decoder::orca_whirlpool::tick_array_pda(&pool_addr, *start) {
                    if rpc.get_account(&pda).await.is_ok() {
                        accounts.push(pda);
                    }
                }
            }
            println!("  Loaded {} tick arrays", accounts.len() - 3 - pool_state.extra_accounts.len());
        }
    }

    // Build 2-hop roundtrip: buy then sell on same pool (TEST MODE)
    let input = 100_000u64; // 0.0001 SOL

    let mut hops = ArrayVec::new();
    hops.push(Hop { pool_index: 0, is_a_to_b: is_a_to_b_buy });  // buy: SOL → token
    hops.push(Hop { pool_index: 1, is_a_to_b: !is_a_to_b_buy }); // sell: token → SOL

    let snapshot = PoolSnapshot {
        address: pool_addr,
        dex_type,
        mint_a: pool_state.mint_a,
        mint_b: pool_state.mint_b,
        is_a_to_b: is_a_to_b_buy,
        mint_a_is_2022,
        mint_b_is_2022,
        accounts: accounts.clone(),
    };
    let snapshot2 = PoolSnapshot {
        is_a_to_b: !is_a_to_b_buy,
        ..snapshot.clone()
    };

    let opp = Opportunity {
        route: Route { hops, base_mint: wsol },
        amount_in: input,
        expected_profit: 1000,
        pool_snapshots: vec![snapshot, snapshot2],
        slot: 0,
    };

    // Build TX
    let mut tx_builder = TxBuilder::from_config(config, payer.pubkey());
    tx_builder.test_mode = true;

    let alt_address: Pubkey = config.executor.alt_address.parse()?;
    let alt = arb_executor::alt::Tier0Alt::load(rpc, alt_address).await?;
    tx_builder.set_alt(Arc::new(alt));

    let blockhash = rpc.get_latest_blockhash().await?;
    let pair = tx_builder.build(&opp, payer, blockhash);

    if let Some(ref tx) = pair.swqos_tx {
        let tx_bytes = bincode::serialize(tx)?;
        println!("  TX: {} bytes, sig={}", tx_bytes.len(), tx.signatures[0]);

        if tx_bytes.len() > 1232 {
            println!("  ❌ TX too large ({} > 1232)", tx_bytes.len());
            return Ok(());
        }

        // Send
        let ast_cfg = config.astralane.as_ref().unwrap();
        let sender = AstralaneSender::new(
            ast_cfg.endpoints[0].clone(),
            ast_cfg.api_key.clone(),
        );
        match sender.send_transaction(tx).await {
            Ok(_) => println!("  ✅ Sent via Astralane"),
            Err(e) => println!("  ❌ Send failed: {}", e),
        }

        // Wait and check
        println!("  Waiting 15s...");
        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        match rpc.get_signature_status(&tx.signatures[0]).await? {
            Some(Ok(())) => println!("  ✅ {} CPI SUCCESS!", dex_name),
            Some(Err(e)) => println!("  ❌ {} CPI FAILED: {:?}", dex_name, e),
            None => println!("  ❌ {} not found on-chain", dex_name),
        }
    } else {
        println!("  ❌ No TX built");
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    env_logger::init();

    let config = ExecutorConfigFile::load("config.toml")?;
    let payer = read_keypair_file("./id.json")
        .map_err(|e| anyhow::anyhow!("keypair: {}", e))?;
    let rpc = RpcClient::new(RPC.to_string());

    println!("Payer: {}", payer.pubkey());

    let args: Vec<String> = std::env::args().collect();
    let dex = args.get(1).map(|s| s.as_str()).unwrap_or("all");

    // Known pools for testing (SOL pairs, active)
    // These need to be found/verified for each DEX
    match dex {
        "cpmm" => {
            test_single_dex(&rpc, &payer, &config,
                "1JsUxxEZcFCob7z2Tt16cFBoyAuAzemJL6MCcLWpdnW",
                DexType::RaydiumCpmm, "Raydium CPMM").await?;
        }
        "clmm" => {
            test_single_dex(&rpc, &payer, &config,
                "7s57Jpn4QDUYnsKan5JNaUTML93pGbkjxp4591CMeCfU",
                DexType::RaydiumClmm, "Raydium CLMM").await?;
        }
        "whirlpool" => {
            test_single_dex(&rpc, &payer, &config,
                "HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ", // SOL/USDC Whirlpool
                DexType::OrcaWhirlpool, "Orca Whirlpool").await?;
        }
        "dammv2" => {
            test_single_dex(&rpc, &payer, &config,
                "DhNy8zGbcLAszBZ9n9XyJkPvjT2CUgqWD758hSaKZXzb",
                DexType::MeteoraDammV2, "Meteora DammV2").await?;
        }
        "ammv4" => {
            // Find AMM V4 SOL pool (owner = 675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8)
            test_single_dex(&rpc, &payer, &config,
                "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2", // SOL/USDC AMM V4
                DexType::RaydiumAmmV4, "Raydium AMM V4").await?;
        }
        _ => {
            println!("Usage: test_dex_cpi <cpmm|clmm|whirlpool|dammv2|ammv4>");
            println!("  Tests a single DEX CPI through our arb program");
        }
    }

    Ok(())
}
