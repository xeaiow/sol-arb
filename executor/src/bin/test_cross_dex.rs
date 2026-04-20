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
use arb_executor::sender::jito::JitoSender;
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
    let mut state = match dex {
        DexType::PumpSwap => decoder::pumpswap::decode_bytes(&address, &data),
        DexType::RaydiumCpmm => decoder::raydium_cpmm::decode_bytes(&address, &data),
        DexType::MeteoraDammV2 => decoder::meteora_damm_v2::decode_bytes(&address, &data),
        DexType::RaydiumAmmV4 => decoder::raydium_amm_v4::decode_bytes(&address, &data),
        DexType::MeteoraDlmm => decoder::meteora_dlmm::decode_bytes(&address, &data),
        DexType::OrcaWhirlpool => decoder::orca_whirlpool::decode_bytes(&address, &data),
        DexType::RaydiumClmm => decoder::raydium_clmm::decode_bytes(&address, &data),
        _ => None,
    }.ok_or_else(|| anyhow::anyhow!("decode {} failed", name))?;

    // For Whirlpool/CLMM: fetch tick array PDAs and add to extra_accounts
    if dex == DexType::OrcaWhirlpool || dex == DexType::RaydiumClmm {
        if let PoolMath::Concentrated { tick_current, tick_spacing, .. } = &state.math {
            let starts = if dex == DexType::OrcaWhirlpool {
                decoder::orca_whirlpool::tick_array_start_indices(*tick_current, *tick_spacing)
            } else {
                decoder::raydium_clmm::tick_array_start_indices(*tick_current, *tick_spacing)
            };
            // starts = [cur-3, cur-2, cur-1, cur, cur+1, cur+2, cur+3]
            // Store in order: current first, then both directions
            let order = [3, 2, 4, 1, 5, 0, 6];
            let mut loaded = 0;
            for &idx in &order {
                let pda = if dex == DexType::OrcaWhirlpool {
                    decoder::orca_whirlpool::tick_array_pda(&address, starts[idx])
                } else {
                    decoder::raydium_clmm::tick_array_pda(&address, starts[idx])
                };
                if let Some(pda) = pda {
                    if rpc.get_account(&pda).await.is_ok() {
                        state.extra_accounts.push(pda);
                        loaded += 1;
                    }
                }
            }
            println!("  {} tick arrays loaded for {}", loaded, name);
        }
    }

    // Check token-2022
    let token_mint = if state.mint_a.to_string().starts_with("So1111") { state.mint_b } else { state.mint_a };
    let mint_is_2022 = match rpc.get_account(&token_mint).await {
        Ok(a) => a.owner.to_string() == "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
        Err(_) => false,
    };

    Ok(PoolInfo { address, state, dex_name: name, mint_is_2022 })
}

fn build_accounts(pool: &PoolInfo, is_a_to_b: bool) -> Vec<Pubkey> {
    let mut accounts = vec![pool.address];
    if let Some(va) = pool.state.vault_a { accounts.push(va); }
    if let Some(vb) = pool.state.vault_b { accounts.push(vb); }

    let is_concentrated = pool.state.dex_type == DexType::OrcaWhirlpool
        || pool.state.dex_type == DexType::RaydiumClmm;

    if is_concentrated {
        if let PoolMath::Concentrated { tick_current, tick_spacing, .. } = &pool.state.math {
            let is_whirlpool = pool.state.dex_type == DexType::OrcaWhirlpool;
            let ticks_per_array: i32 = if is_whirlpool { 88 } else { 60 };

            // CLMM extra_accounts = [amm_config, observation_key, ta_pda...]
            // Whirlpool extra_accounts = [oracle, ta_pda...]
            let fixed_extras = if is_whirlpool { 1 } else { 2 };
            for ea in pool.state.extra_accounts.iter().take(fixed_extras) {
                accounts.push(*ea);
            }

            let ts = *tick_spacing as i32;
            let ticks_in_array = ts * ticks_per_array;

            // For Whirlpool b_to_a: shift by tick_spacing
            let ref_tick = if is_whirlpool && !is_a_to_b {
                *tick_current + ts
            } else {
                *tick_current
            };
            let current_start = if is_whirlpool {
                decoder::orca_whirlpool::tick_array_start_index(ref_tick, *tick_spacing)
            } else {
                decoder::raydium_clmm::tick_array_start_index(ref_tick, *tick_spacing)
            };

            // Compute 3 tick array PDAs in direction order
            let offsets: [i32; 3] = if is_a_to_b {
                [0, -1, -2] // descending
            } else {
                [0, 1, 2]   // ascending
            };
            for offset in &offsets {
                let start = current_start + offset * ticks_in_array;
                let pda = if is_whirlpool {
                    decoder::orca_whirlpool::tick_array_pda(&pool.address, start)
                } else {
                    decoder::raydium_clmm::tick_array_pda(&pool.address, start)
                };
                if let Some(pda) = pda {
                    accounts.push(pda);
                }
            }
        }
    } else {
        accounts.extend_from_slice(&pool.state.extra_accounts);
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

    let buy_accounts = build_accounts(buy_pool, buy_a_to_b);
    let sell_accounts = build_accounts(sell_pool, sell_a_to_b);

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

    let tx = pair.jito_tx.as_ref().or(pair.swqos_tx.as_ref());
    if let Some(tx) = tx {
        let tx_bytes = bincode::serialize(tx)?;
        println!("  TX: {} bytes, sig={}", tx_bytes.len(), tx.signatures[0]);
        if tx_bytes.len() > 1232 {
            println!("  ❌ TX too large");
            return Ok(());
        }

        let sender = JitoSender::new("https://frankfurt.mainnet.block-engine.jito.wtf".to_string());
        match sender.send_bundle(tx).await {
            Ok(()) => {}
            Err(e) => {
                println!("  Jito failed: {}, trying RPC...", e);
                rpc.send_transaction(tx).await?;
            }
        }

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

    // SOL/USDC pools for cross-DEX tests
    let cpmm_usdc = decode_pool(&rpc, "47hq28mcL7q5GhBg7epyGF2dnuJd4MKFt8QhT7CzYUp4",
        DexType::RaydiumCpmm, "CPMM").await?;
    let dammv2_usdc = decode_pool(&rpc, "B5NKvGBqUUXUxqiAK5yjBzkBgtHX9LzcNU9A8aSxowK",
        DexType::MeteoraDammV2, "DammV2").await?;

    // Whirlpool SOL/USDC pool
    let whirlpool = decode_pool(&rpc, "HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ",
        DexType::OrcaWhirlpool, "Whirlpool").await?;

    // DLMM SOL/USDC pool
    let dlmm_usdc = decode_pool(&rpc, "R5trYLjPStfMRLbS9enkxBcWUCC9NSCEob3RXGWeBdH",
        DexType::MeteoraDlmm, "DLMM-USDC").await;

    // Test 1: Whirlpool buy → DammV2 sell (SOL/USDC)
    println!("\n--- Whirlpool cross-DEX tests ---");
    test_cross(&rpc, &payer, &config, &whirlpool, &dammv2_usdc).await?;

    // Test 2: Whirlpool buy → CPMM sell (SOL/USDC)
    test_cross(&rpc, &payer, &config, &whirlpool, &cpmm_usdc).await?;

    // Test 3: CPMM buy → Whirlpool sell (SOL/USDC) — tests Whirlpool sell (b_to_a)
    test_cross(&rpc, &payer, &config, &cpmm_usdc, &whirlpool).await?;

    // --- CLMM cross-DEX tests ---
    // Raydium CLMM SOL/USDC pool (highest liquidity from Raydium API)
    let clmm = decode_pool(&rpc, "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv",
        DexType::RaydiumClmm, "CLMM").await?;
    println!("\n--- CLMM cross-DEX tests ---");
    // Test 4: CLMM buy → CPMM sell (SOL/USDC)
    test_cross(&rpc, &payer, &config, &clmm, &cpmm_usdc).await?;
    // Test 5: CPMM buy → CLMM sell (SOL/USDC)
    test_cross(&rpc, &payer, &config, &cpmm_usdc, &clmm).await?;

    Ok(())
}
