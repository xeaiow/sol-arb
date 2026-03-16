//! Test cross-DEX arb: buy on DEX A, sell on DEX B.
//! Finds tokens that exist on 2 DEXes and tests the CPI roundtrip.
//!
//! Usage: cd executor && TEST_MODE=1 RUST_LOG=warn cargo run --release --bin test_cross_dex

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
use solana_streamer_sdk::pool::state::{DexType, PoolMath, PoolState};

const RPC: &str = "http://45.157.234.194:8899";
const WSOL: &str = "So11111111111111111111111111111111111111112";

struct PoolInfo {
    address: Pubkey,
    state: PoolState,
    dex_name: &'static str,
    mint_is_2022: bool,
}

async fn decode_pool(rpc: &RpcClient, addr: &str, dex: DexType, name: &'static str) -> anyhow::Result<PoolInfo> {
    let address: Pubkey = addr.parse()?;
    let data = rpc.get_account_data(&address).await?;
    let state = match dex {
        DexType::PumpSwap => decoder::pumpswap::decode_bytes(&address, &data),
        DexType::RaydiumCpmm => decoder::raydium_cpmm::decode_bytes(&address, &data),
        DexType::MeteoraDammV2 => decoder::meteora_damm_v2::decode_bytes(&address, &data),
        DexType::RaydiumAmmV4 => decoder::raydium_amm_v4::decode_bytes(&address, &data),
        DexType::MeteoraDlmm => decoder::meteora_dlmm::decode_bytes(&address, &data),
        _ => None,
    }.ok_or_else(|| anyhow::anyhow!("decode {} failed", name))?;

    // Check token-2022
    let token_mint = if state.mint_a.to_string().starts_with("So1111") { state.mint_b } else { state.mint_a };
    let mint_is_2022 = match rpc.get_account(&token_mint).await {
        Ok(a) => a.owner.to_string() == "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
        Err(_) => false,
    };

    Ok(PoolInfo { address, state, dex_name: name, mint_is_2022 })
}

fn build_accounts(pool: &PoolInfo) -> Vec<Pubkey> {
    let mut accounts = vec![pool.address];
    if let Some(va) = pool.state.vault_a { accounts.push(va); }
    if let Some(vb) = pool.state.vault_b { accounts.push(vb); }
    accounts.extend_from_slice(&pool.state.extra_accounts);

    // DLMM: fetch bin arrays inline
    if pool.state.dex_type == DexType::MeteoraDlmm {
        if let PoolMath::MeteoraDlmm { active_id, .. } = &pool.state.math {
            let pdas = decoder::meteora_dlmm::bin_array_pdas_for_swap(&pool.address, *active_id);
            // extra_accounts already has [oracle, bin_pda1, bin_pda2, bin_pda3]
            // accounts = [pool, vault_a, vault_b, oracle, bin1, bin2, bin3]
        }
    }
    accounts
}

async fn test_cross(
    rpc: &RpcClient,
    payer: &solana_sdk::signer::keypair::Keypair,
    config: &ExecutorConfigFile,
    buy_pool: &PoolInfo,
    sell_pool: &PoolInfo,
) -> anyhow::Result<()> {
    let wsol: Pubkey = WSOL.parse()?;
    let input = 100_000u64; // 0.0001 SOL

    println!("\n=== {} buy → {} sell ===", buy_pool.dex_name, sell_pool.dex_name);
    println!("  Token: {}", if buy_pool.state.mint_a.to_string().starts_with("So1111") {
        buy_pool.state.mint_b
    } else {
        buy_pool.state.mint_a
    });

    // Determine direction for each pool
    let buy_sol_is_a = buy_pool.state.mint_a.to_string().starts_with("So1111");
    let sell_sol_is_a = sell_pool.state.mint_a.to_string().starts_with("So1111");
    let buy_a_to_b = buy_sol_is_a;   // SOL→token
    let sell_a_to_b = !sell_sol_is_a; // token→SOL

    let buy_accounts = build_accounts(buy_pool);
    let sell_accounts = build_accounts(sell_pool);

    let (buy_mint_a_2022, buy_mint_b_2022) = if buy_sol_is_a {
        (false, buy_pool.mint_is_2022)
    } else {
        (buy_pool.mint_is_2022, false)
    };
    let (sell_mint_a_2022, sell_mint_b_2022) = if sell_sol_is_a {
        (false, sell_pool.mint_is_2022)
    } else {
        (sell_pool.mint_is_2022, false)
    };

    let mut hops = ArrayVec::new();
    hops.push(Hop { pool_index: 0, is_a_to_b: buy_a_to_b });
    hops.push(Hop { pool_index: 1, is_a_to_b: sell_a_to_b });

    let opp = Opportunity {
        route: Route { hops, base_mint: wsol },
        amount_in: input,
        expected_profit: 1000,
        pool_snapshots: vec![
            PoolSnapshot {
                address: buy_pool.address,
                dex_type: buy_pool.state.dex_type,
                mint_a: buy_pool.state.mint_a,
                mint_b: buy_pool.state.mint_b,
                is_a_to_b: buy_a_to_b,
                mint_a_is_2022: buy_mint_a_2022,
                mint_b_is_2022: buy_mint_b_2022,
                accounts: buy_accounts,
            },
            PoolSnapshot {
                address: sell_pool.address,
                dex_type: sell_pool.state.dex_type,
                mint_a: sell_pool.state.mint_a,
                mint_b: sell_pool.state.mint_b,
                is_a_to_b: sell_a_to_b,
                mint_a_is_2022: sell_mint_a_2022,
                mint_b_is_2022: sell_mint_b_2022,
                accounts: sell_accounts,
            },
        ],
        slot: 0,
    };

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
            println!("  ❌ TX too large");
            return Ok(());
        }

        let ast_cfg = config.astralane.as_ref().unwrap();
        let sender = AstralaneSender::new(ast_cfg.endpoints[0].clone(), ast_cfg.api_key.clone());
        sender.send_transaction(tx).await?;

        tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        match rpc.get_signature_status(&tx.signatures[0]).await? {
            Some(Ok(())) => println!("  ✅ SUCCESS: {}", tx.signatures[0]),
            Some(Err(e)) => println!("  ❌ FAILED: {:?}", e),
            None => println!("  ❌ Not found on-chain"),
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
    let payer = read_keypair_file("./id.json").map_err(|e| anyhow::anyhow!("keypair: {}", e))?;
    let rpc = RpcClient::new(RPC.to_string());

    println!("Payer: {}", payer.pubkey());

    // Use FDQ77aHD token — has both PumpSwap and DLMM pools (confirmed working)
    let pumpswap = decode_pool(&rpc, "ED41PwcJhsPgbUHQb4LZJbWzFXtcEC6RAherWC2YgEU3",
        DexType::PumpSwap, "PumpSwap").await?;
    let dlmm = decode_pool(&rpc, "6Jq5BtZ6ExjBgY4PU7jnVWgV9jzbQuWGQZVrvNvDbABP",
        DexType::MeteoraDlmm, "DLMM").await?;

    // Test 1: DLMM buy → PumpSwap sell (already confirmed working)
    test_cross(&rpc, &payer, &config, &dlmm, &pumpswap).await?;

    // Test 2: PumpSwap buy → DLMM sell (reverse direction)
    test_cross(&rpc, &payer, &config, &pumpswap, &dlmm).await?;

    // Now find CPMM and DammV2 pools for the SAME token to test more combos
    // Use a different token that has CPMM + DammV2
    // SOL/USDC exists on both CPMM and DammV2
    let cpmm = decode_pool(&rpc, "1JsUxxEZcFCob7z2Tt16cFBoyAuAzemJL6MCcLWpdnW",
        DexType::RaydiumCpmm, "CPMM").await?;

    // Find a DammV2 pool with same token as CPMM
    let cpmm_token = if cpmm.state.mint_a.to_string().starts_with("So1111") {
        cpmm.state.mint_b
    } else {
        cpmm.state.mint_a
    };
    println!("\nCPMM token: {}", cpmm_token);

    // Test 3: CPMM buy → CPMM sell (same pool roundtrip, already tested)
    // Skip — already confirmed

    // Test 4: Find a DammV2 + AMM V4 combo
    // AMM V4 SOL/USDC pool
    let ammv4 = decode_pool(&rpc, "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2",
        DexType::RaydiumAmmV4, "AMM V4").await?;

    // DammV2 SOL/USDC
    let dammv2 = decode_pool(&rpc, "DhNy8zGbcLAszBZ9n9XyJkPvjT2CUgqWD758hSaKZXzb",
        DexType::MeteoraDammV2, "DammV2").await?;

    // Check if they share a token (both SOL paired)
    let ammv4_token = if ammv4.state.mint_a.to_string().starts_with("So1111") {
        ammv4.state.mint_b
    } else {
        ammv4.state.mint_a
    };
    let dammv2_token = if dammv2.state.mint_a.to_string().starts_with("So1111") {
        dammv2.state.mint_b
    } else {
        dammv2.state.mint_a
    };

    if ammv4_token == dammv2_token {
        // Test 4: AMM V4 buy → DammV2 sell
        test_cross(&rpc, &payer, &config, &ammv4, &dammv2).await?;
        // Test 5: DammV2 buy → AMM V4 sell
        test_cross(&rpc, &payer, &config, &dammv2, &ammv4).await?;
    } else {
        println!("\nAMM V4 token ({}) != DammV2 token ({}), skipping cross test", ammv4_token, dammv2_token);
        // Do same-token roundtrip instead
        // Test 4: AMM V4 buy → AMM V4 sell
        test_cross(&rpc, &payer, &config, &ammv4, &ammv4).await?;
        // Test 5: DammV2 buy → DammV2 sell
        test_cross(&rpc, &payer, &config, &dammv2, &dammv2).await?;
    }

    // Test 6: CPMM buy → CPMM sell (different token)
    test_cross(&rpc, &payer, &config, &cpmm, &cpmm).await?;

    Ok(())
}
