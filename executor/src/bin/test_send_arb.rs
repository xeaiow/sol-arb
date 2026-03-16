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

    // === Pool addresses (from reference tx Cd7uHS82...) ===
    // FDQ77aHDgV6o token — smaller reserves, no u64 overflow
    let pumpswap_pool: Pubkey = "ED41PwcJhsPgbUHQb4LZJbWzFXtcEC6RAherWC2YgEU3".parse()?;
    let dlmm_pool: Pubkey = "6Jq5BtZ6ExjBgY4PU7jnVWgV9jzbQuWGQZVrvNvDbABP".parse()?;

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

    // === Initialize user_volume_accumulator if needed ===
    {
        let (user_vol_acc, _) = Pubkey::find_program_address(
            &[b"user_volume_accumulator", payer.pubkey().as_ref()],
            &"pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA".parse::<Pubkey>()?,
        );
        let exists = rpc.get_account(&user_vol_acc).await.is_ok();
        if !exists {
            println!("Initializing user_volume_accumulator...");
            let pumpswap_prog: Pubkey = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA".parse()?;
            let (event_auth, _) = Pubkey::find_program_address(
                &[b"__event_authority"], &pumpswap_prog,
            );
            // discriminator for init_user_volume_accumulator
            let disc: [u8; 8] = [94, 6, 202, 115, 255, 96, 232, 183];
            let ix = solana_sdk::instruction::Instruction {
                program_id: pumpswap_prog,
                accounts: vec![
                    solana_sdk::instruction::AccountMeta::new(payer.pubkey(), true),      // payer
                    solana_sdk::instruction::AccountMeta::new_readonly(payer.pubkey(), false), // user
                    solana_sdk::instruction::AccountMeta::new(user_vol_acc, false),        // PDA
                    solana_sdk::instruction::AccountMeta::new_readonly(
                        "11111111111111111111111111111111".parse::<Pubkey>()?, false),     // system
                    solana_sdk::instruction::AccountMeta::new_readonly(event_auth, false), // event_authority
                    solana_sdk::instruction::AccountMeta::new_readonly(pumpswap_prog, false), // program
                ],
                data: disc.to_vec(),
            };
            let bh = rpc.get_latest_blockhash().await?;
            let tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
                &[ix], Some(&payer.pubkey()), &[&payer], bh,
            );
            match rpc.send_and_confirm_transaction(&tx).await {
                Ok(sig) => println!("  ✅ Initialized: {}", sig),
                Err(e) => println!("  ❌ Init failed: {}", e),
            }
        } else {
            println!("user_volume_accumulator already exists");
        }
    }

    // === Debug: verify coin_creator PDA ===
    {
        let coin_creator = ps_state.extra_accounts.first().copied().unwrap_or_default();
        let pumpswap_prog: Pubkey = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA".parse()?;
        let spl_token: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse()?;
        let (authority, _) = Pubkey::find_program_address(
            &[b"creator_vault", coin_creator.as_ref()], &pumpswap_prog,
        );
        let ata_prog: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse()?;
        let (ata, _) = Pubkey::find_program_address(
            &[authority.as_ref(), spl_token.as_ref(), ps_state.mint_b.as_ref()], &ata_prog,
        );
        println!("coin_creator: {}", coin_creator);
        println!("vault_authority PDA: {}", authority);
        println!("vault_ata: {}", ata);
        println!("Expected authority: 6woDuTPMCtNVxv6ox6ZPnYKNaa2o3rQ4rkHDLSdnXJvA");
        println!("Expected ata: 5MtD9ezEw5nznz8SxrYEju2Aenoh8e5haWHM2oKS9nRz");
    }

    // === Build quote ===
    // Correct direction: DLMM buy (SOL → token) then PumpSwap sell (token → SOL)
    // DLMM: mint_a = token, mint_b = SOL → buy token = is_a_to_b = false (SOL in, token out)
    // PumpSwap: mint_a = token, mint_b = SOL → sell token = is_a_to_b = true (token in, SOL out)
    let ps_math = PoolMath::ConstantProduct {
        reserve_a: ps_balance_a,
        reserve_b: ps_balance_b,
        fee_numerator: 25,
        fee_denominator: 10000,
    };

    let input = 10_000_000u64; // 0.01 SOL

    // Direction 1: DLMM buy → PumpSwap sell (correct, matching reference tx)
    let hop1_out_dlmm = dlmm_state.math.get_amount_out(input, false); // SOL → token via DLMM
    let hop2_out_ps = ps_math.get_amount_out(hop1_out_dlmm, true);     // token → SOL via PumpSwap sell
    let profit1 = hop2_out_ps as i64 - input as i64;

    println!("\n[Route A] DLMM buy → PumpSwap sell:");
    println!("  {:.4} SOL → {} tokens → {:.6} SOL | profit = {} lamports",
        input as f64 / 1e9, hop1_out_dlmm, hop2_out_ps as f64 / 1e9, profit1);

    // Direction 2: PumpSwap buy → DLMM sell (our old direction, causes Overflow)
    let hop1_out_ps = ps_math.get_amount_out(input, false); // SOL → token via PumpSwap buy
    let hop2_out_dlmm = dlmm_state.math.get_amount_out(hop1_out_ps, true); // token → SOL via DLMM
    let profit2 = hop2_out_dlmm as i64 - input as i64;

    println!("[Route B] PumpSwap buy → DLMM sell:");
    println!("  {:.4} SOL → {} tokens → {:.6} SOL | profit = {} lamports",
        input as f64 / 1e9, hop1_out_ps, hop2_out_dlmm as f64 / 1e9, profit2);

    // Use Route A (DLMM buy → PumpSwap sell) — no Overflow
    let hop1_out = hop1_out_dlmm;
    let hop2_out = hop2_out_ps;
    let profit = profit1;

    if hop1_out == 0 || hop2_out == 0 {
        println!("Quote returned 0 — can't build tx");
        return Ok(());
    }

    // === Build Opportunity ===
    let wsol: Pubkey = WSOL.parse()?;
    let mut hops = arrayvec::ArrayVec::new();
    hops.push(Hop { pool_index: 0, is_a_to_b: false }); // DLMM buy (SOL → token)
    hops.push(Hop { pool_index: 1, is_a_to_b: true });  // PumpSwap sell (token → SOL)

    let opp = Opportunity {
        route: Route { hops, base_mint: wsol },
        amount_in: input,
        expected_profit: if profit > 0 { profit as u64 } else { 1000 }, // min 1000 for test
        pool_snapshots: vec![
            // Hop 1: DLMM buy
            PoolSnapshot {
                address: dlmm_pool,
                dex_type: DexType::MeteoraDlmm,
                mint_a: dlmm_state.mint_a,
                mint_b: dlmm_state.mint_b,
                is_a_to_b: false,  // SOL → token (buy)
                mint_a_is_2022: true, // pump tokens are Token-2022
                mint_b_is_2022: false,
                accounts: dlmm_accounts,
            },
            // Hop 2: PumpSwap sell
            PoolSnapshot {
                address: pumpswap_pool,
                dex_type: DexType::PumpSwap,
                mint_a: ps_state.mint_a,
                mint_b: ps_state.mint_b,
                is_a_to_b: true,   // token → SOL (sell)
                mint_a_is_2022: true, // pump tokens are Token-2022
                mint_b_is_2022: false,
                accounts: ps_accounts,
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
