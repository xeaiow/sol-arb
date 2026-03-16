//! Test: send a real PumpSwap+DLMM arb transaction (like the reference tx).
//!
//! Fetches current pool state, builds opportunity, builds tx, sends via Astralane.
//! Uses TEST_MODE so on-chain profit check is skipped (won't lose money even if unprofitable).
//!
//! Usage: cd executor && TEST_MODE=1 cargo run --release --bin test_send_arb

use std::sync::Arc;

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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let _ = rustls::crypto::ring::default_provider().install_default();

    let config = ExecutorConfigFile::load("config.toml")?;
    let payer = read_keypair_file("./id.json")
        .map_err(|e| anyhow::anyhow!("keypair: {}", e))?;
    println!("Payer: {}", payer.pubkey());

    let rpc = RpcClient::new(RPC.to_string());

    // === Pool addresses ===
    let pumpswap_pool: Pubkey = "GVQgwAg8HqhMVud7U7x3XgTkp7A1J6CS4B1eoPsFwKLQ".parse()?;
    let dlmm_pool: Pubkey = "3EMXSmVSfaKPn6LVPHZVFn1s5ufvsB4R27wG6oS4bC92".parse()?;

    // === Decode PumpSwap pool ===
    let ps_data = rpc.get_account_data(&pumpswap_pool).await?;
    let ps_state = decoder::pumpswap::decode_bytes(&pumpswap_pool, &ps_data)
        .ok_or_else(|| anyhow::anyhow!("decode PumpSwap failed"))?;

    // Fetch vault balances
    let ps_vault_a = ps_state.vault_a.unwrap();
    let ps_vault_b = ps_state.vault_b.unwrap();
    let ps_va_data = rpc.get_account_data(&ps_vault_a).await?;
    let ps_vb_data = rpc.get_account_data(&ps_vault_b).await?;
    let ps_balance_a = u64::from_le_bytes(ps_va_data[64..72].try_into()?);
    let ps_balance_b = u64::from_le_bytes(ps_vb_data[64..72].try_into()?);
    println!("PumpSwap: token_reserve={}, sol_reserve={} ({:.4} SOL)",
        ps_balance_a, ps_balance_b, ps_balance_b as f64 / 1e9);

    // Build PumpSwap accounts: [pool, vault_a, vault_b, coin_creator]
    let ps_accounts: Vec<Pubkey> = {
        let mut v = vec![pumpswap_pool, ps_vault_a, ps_vault_b];
        v.extend_from_slice(&ps_state.extra_accounts); // [coin_creator]
        v
    };

    // === Decode DLMM pool ===
    let dlmm_data = rpc.get_account_data(&dlmm_pool).await?;
    let mut dlmm_state = decoder::meteora_dlmm::decode_bytes(&dlmm_pool, &dlmm_data)
        .ok_or_else(|| anyhow::anyhow!("decode DLMM failed"))?;

    let active_id = match &dlmm_state.math {
        PoolMath::MeteoraDlmm { active_id, .. } => *active_id,
        _ => return Err(anyhow::anyhow!("not DLMM")),
    };

    // Fetch bin arrays
    let bin_pdas = decoder::meteora_dlmm::bin_array_pdas_for_swap(&dlmm_pool, active_id);
    let mut bin_arrays = Vec::new();
    let mut existing_pdas = Vec::new();
    for pda in &bin_pdas {
        match rpc.get_account_data(pda).await {
            Ok(data) => {
                if let Some(ba) = decoder::meteora_dlmm::decode_bin_array(&data) {
                    println!("DLMM bin_array {}: index={}", &pda.to_string()[..8], ba.index);
                    bin_arrays.push(ba);
                    existing_pdas.push(*pda);
                }
            }
            Err(_) => {}
        }
    }

    // Update DLMM math with bin arrays
    if let PoolMath::MeteoraDlmm { bin_arrays: ref mut ba, .. } = dlmm_state.math {
        *ba = bin_arrays;
    }

    // Build DLMM accounts: [pool, reserve_x, reserve_y, oracle, bin_pda1, bin_pda2, ...]
    let dlmm_accounts: Vec<Pubkey> = {
        let mut v = vec![dlmm_pool];
        if let Some(va) = dlmm_state.vault_a { v.push(va); }
        if let Some(vb) = dlmm_state.vault_b { v.push(vb); }
        // extra_accounts: [oracle, bin_pda1, bin_pda2, bin_pda3]
        if !dlmm_state.extra_accounts.is_empty() {
            v.push(dlmm_state.extra_accounts[0]); // oracle
        }
        v.extend(&existing_pdas); // bin array PDAs
        v
    };

    println!("DLMM: {} accounts, {} bin arrays", dlmm_accounts.len(), existing_pdas.len());

    // === Build quote ===
    // PumpSwap buy: SOL → token (is_a_to_b = false, since mint_b = SOL)
    let ps_math = PoolMath::ConstantProduct {
        reserve_a: ps_balance_a,
        reserve_b: ps_balance_b,
        fee_numerator: 25,
        fee_denominator: 10000,
    };

    // Try very small amount to avoid PumpSwap overflow
    let input = 100_000u64; // 0.0001 SOL
    let hop1_out = ps_math.get_amount_out(input, false); // SOL → token
    let hop2_out = dlmm_state.math.get_amount_out(hop1_out, true); // token → SOL
    let profit = hop2_out as i64 - input as i64;

    println!("\nQuote: {:.4} SOL → {} tokens → {:.6} SOL | profit = {} lamports",
        input as f64 / 1e9, hop1_out, hop2_out as f64 / 1e9, profit);

    if hop1_out == 0 || hop2_out == 0 {
        println!("Quote returned 0 — can't build tx");
        return Ok(());
    }

    // === Build Opportunity ===
    let wsol: Pubkey = WSOL.parse()?;
    let mut hops = arrayvec::ArrayVec::new();
    hops.push(Hop { pool_index: 0, is_a_to_b: false }); // PumpSwap buy
    hops.push(Hop { pool_index: 1, is_a_to_b: true });  // DLMM sell

    let opp = Opportunity {
        route: Route { hops, base_mint: wsol },
        amount_in: input,
        expected_profit: if profit > 0 { profit as u64 } else { 1000 }, // min 1000 for test
        pool_snapshots: vec![
            PoolSnapshot {
                address: pumpswap_pool,
                dex_type: DexType::PumpSwap,
                mint_a: ps_state.mint_a,
                mint_b: ps_state.mint_b,
                is_a_to_b: false,
                mint_a_is_2022: true, // pump tokens are Token-2022
                mint_b_is_2022: false,
                accounts: ps_accounts,
            },
            PoolSnapshot {
                address: dlmm_pool,
                dex_type: DexType::MeteoraDlmm,
                mint_a: dlmm_state.mint_a,
                mint_b: dlmm_state.mint_b,
                is_a_to_b: true,
                mint_a_is_2022: true, // pump tokens are Token-2022
                mint_b_is_2022: false,
                accounts: dlmm_accounts,
            },
        ],
        slot: 0,
    };

    // === Build TX ===
    let mut tx_builder = TxBuilder::from_config(&config, payer.pubkey());
    // Force test mode: skip on-chain profit verification
    tx_builder.test_mode = true;
    // Note: marginfi_state is None by default → no flashloan instructions

    // Load ALT for transaction compression
    let alt_address: Pubkey = config.executor.alt_address.parse()?;
    let alt = arb_executor::alt::Tier0Alt::load(&rpc, alt_address).await?;
    tx_builder.set_alt(Arc::new(alt));
    println!("ALT loaded: {}", alt_address);

    let blockhash = rpc.get_latest_blockhash().await?;
    let pair = tx_builder.build(&opp, &payer, blockhash);

    if let Some(ref tx) = pair.swqos_tx {
        let tx_bytes = bincode::serialize(tx)?;
        println!("\nSwQoS TX: {} bytes, sig={}", tx_bytes.len(), tx.signatures[0]);

        if tx_bytes.len() > 1232 {
            println!("❌ TX too large ({} > 1232 bytes)", tx_bytes.len());
        } else {
            // Send via Astralane
            let ast_cfg = config.astralane.as_ref().unwrap();
            let sender = AstralaneSender::new(
                ast_cfg.endpoints[0].clone(),
                ast_cfg.api_key.clone(),
            );
            match sender.send_transaction(tx).await {
                Ok(_) => println!("✅ Sent via Astralane: {}", ast_cfg.endpoints[0]),
                Err(e) => println!("❌ Send failed: {}", e),
            }

            // Wait and check
            println!("\nWaiting 10s...");
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            match rpc.get_signature_status(&tx.signatures[0]).await? {
                Some(Ok(())) => println!("✅ CONFIRMED on-chain!"),
                Some(Err(e)) => println!("❌ On-chain FAILED: {:?}", e),
                None => println!("❌ Not found on-chain after 10s"),
            }
        }
    } else {
        println!("❌ No SwQoS TX built");
    }

    if let Some(ref tx) = pair.jito_tx {
        println!("Jito TX: {} bytes", bincode::serialize(tx)?.len());
    }

    Ok(())
}
