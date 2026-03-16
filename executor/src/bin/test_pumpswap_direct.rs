//! Test: send a PumpSwap sell DIRECTLY (no arb program), to isolate the Overflow issue.
//!
//! If this succeeds → problem is in our arb program's CPI.
//! If this fails → problem is in PumpSwap itself or our account construction.
//!
//! Usage: cd executor && RUST_LOG=info cargo run --release --bin test_pumpswap_direct

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    message::{self, VersionedMessage},
    pubkey::Pubkey,
    signer::{keypair::read_keypair_file, Signer},
    transaction::VersionedTransaction,
};
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_commitment_config::CommitmentConfig;

use solana_streamer_sdk::pool::decoder;
use solana_streamer_sdk::pool::state::PoolMath;

const RPC: &str = "http://45.157.234.194:8899";

fn derive_ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    let ata_prog: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();
    let (ata, _) = Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ata_prog,
    );
    ata
}

fn create_ata_idempotent_ix(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Instruction {
    let ata_prog: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse().unwrap();
    let ata = derive_ata(owner, mint, token_program);
    let system: Pubkey = "11111111111111111111111111111111".parse().unwrap();
    Instruction {
        program_id: ata_prog,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1], // CreateIdempotent
    }
}
const PUMPSWAP: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const PUMPSWAP_FEE: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
const PUMPSWAP_GLOBAL: &str = "ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    env_logger::init();

    let payer = read_keypair_file("./id.json")
        .map_err(|e| anyhow::anyhow!("keypair: {}", e))?;
    let rpc = RpcClient::new_with_commitment(RPC.to_string(), CommitmentConfig::confirmed());
    println!("Payer: {}", payer.pubkey());

    // Use FDQ77 pool (same as reference tx)
    let pool_addr: Pubkey = "ED41PwcJhsPgbUHQb4LZJbWzFXtcEC6RAherWC2YgEU3".parse()?;
    let pumpswap_prog: Pubkey = PUMPSWAP.parse()?;
    let fee_prog: Pubkey = PUMPSWAP_FEE.parse()?;
    let global_config: Pubkey = PUMPSWAP_GLOBAL.parse()?;
    let system: Pubkey = "11111111111111111111111111111111".parse()?;
    let ata_prog: Pubkey = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL".parse()?;
    let spl_token: Pubkey = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA".parse()?;
    let token_2022: Pubkey = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".parse()?;

    // Decode pool
    let pool_data = rpc.get_account_data(&pool_addr).await?;
    let ps = decoder::pumpswap::decode_bytes(&pool_addr, &pool_data)
        .ok_or_else(|| anyhow::anyhow!("decode failed"))?;

    let base_mint = ps.mint_a; // token (Token-2022)
    let quote_mint = ps.mint_b; // SOL
    let pool_base_vault = ps.vault_a.unwrap();
    let pool_quote_vault = ps.vault_b.unwrap();
    let coin_creator = ps.extra_accounts[0];

    println!("Pool: {}", pool_addr);
    println!("Base mint (token): {}", base_mint);
    println!("Quote mint (SOL): {}", quote_mint);
    println!("Coin creator: {}", coin_creator);

    // Derive accounts
    let base_token_prog = token_2022; // pump tokens are Token-2022
    let quote_token_prog = spl_token;

    let user_base_ata = derive_ata(&payer.pubkey(), &base_mint, &base_token_prog);
    let user_quote_ata = derive_ata(&payer.pubkey(), &quote_mint, &quote_token_prog);

    // Protocol fee recipient
    let protocol_fee_recipient: Pubkey = "62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV".parse()?;
    let protocol_fee_ata = derive_ata(&protocol_fee_recipient, &quote_mint, &quote_token_prog);

    // Event authority
    let (event_authority, _) = Pubkey::find_program_address(
        &[b"__event_authority"], &pumpswap_prog,
    );

    // Coin creator vault
    let (creator_vault_authority, _) = Pubkey::find_program_address(
        &[b"creator_vault", coin_creator.as_ref()], &pumpswap_prog,
    );
    let creator_vault_ata = derive_ata(&creator_vault_authority, &quote_mint, &quote_token_prog);

    // Fee config
    let (fee_config, _) = Pubkey::find_program_address(
        &[b"fee_config", pumpswap_prog.as_ref()], &fee_prog,
    );

    println!("\nDerived accounts:");
    println!("  user_base_ata: {}", user_base_ata);
    println!("  user_quote_ata: {}", user_quote_ata);
    println!("  event_authority: {}", event_authority);
    println!("  creator_vault_authority: {}", creator_vault_authority);
    println!("  creator_vault_ata: {}", creator_vault_ata);
    println!("  fee_config: {}", fee_config);

    // First: buy a small amount of tokens via DLMM or directly
    // For simplicity, we'll check if we already have tokens
    let user_base_balance = match rpc.get_account(&user_base_ata).await {
        Ok(acct) => {
            if acct.data.len() >= 72 {
                u64::from_le_bytes(acct.data[64..72].try_into().unwrap_or([0;8]))
            } else { 0 }
        }
        Err(_) => 0,
    };
    println!("\nUser token balance: {} (raw)", user_base_balance);

    if user_base_balance == 0 {
        println!("No tokens to sell. Need to buy some first.");
        println!("Sending a PumpSwap buy_exact_quote_in for 0.001 SOL...");

        // Buy first
        let buy_disc: [u8; 8] = [198, 46, 21, 82, 180, 217, 232, 112];
        let buy_amount: u64 = 1_000_000; // 0.001 SOL
        let mut buy_data = vec![0u8; 25];
        buy_data[0..8].copy_from_slice(&buy_disc);
        buy_data[8..16].copy_from_slice(&buy_amount.to_le_bytes());
        buy_data[16..24].copy_from_slice(&1u64.to_le_bytes()); // min_base = 1
        buy_data[24] = 1; // track_volume = true

        // Volume accumulators
        let (global_vol, _) = Pubkey::find_program_address(
            &[b"global_volume_accumulator"], &pumpswap_prog,
        );
        let (user_vol, _) = Pubkey::find_program_address(
            &[b"user_volume_accumulator", payer.pubkey().as_ref()], &pumpswap_prog,
        );
        // pool_v2 PDA — REQUIRED, fixes Overflow (6023)
        let (pool_v2, _) = Pubkey::find_program_address(
            &[b"pool-v2", base_mint.as_ref()], &pumpswap_prog,
        );

        // Create ATA if needed
        let create_ata_ix = create_ata_idempotent_ix(&payer.pubkey(), &payer.pubkey(), &base_mint, &base_token_prog);

        let buy_ix = Instruction {
            program_id: pumpswap_prog,
            accounts: vec![
                AccountMeta::new(pool_addr, false),
                AccountMeta::new(payer.pubkey(), true),
                AccountMeta::new_readonly(global_config, false),
                AccountMeta::new_readonly(base_mint, false),
                AccountMeta::new_readonly(quote_mint, false),
                AccountMeta::new(user_base_ata, false),
                AccountMeta::new(user_quote_ata, false),
                AccountMeta::new(pool_base_vault, false),
                AccountMeta::new(pool_quote_vault, false),
                AccountMeta::new(protocol_fee_recipient, false),
                AccountMeta::new(protocol_fee_ata, false),
                AccountMeta::new_readonly(base_token_prog, false),
                AccountMeta::new_readonly(quote_token_prog, false),
                AccountMeta::new_readonly(system, false),
                AccountMeta::new_readonly(ata_prog, false),
                AccountMeta::new_readonly(event_authority, false),
                AccountMeta::new_readonly(pumpswap_prog, false),
                AccountMeta::new(creator_vault_ata, false),
                AccountMeta::new_readonly(creator_vault_authority, false),
                // remaining accounts
                AccountMeta::new(global_vol, false),
                AccountMeta::new(user_vol, false),
                AccountMeta::new_readonly(fee_config, false),
                AccountMeta::new_readonly(fee_prog, false),
                AccountMeta::new_readonly(pool_v2, false),  // pool_v2 PDA — fixes Overflow
            ],
            data: buy_data,
        };

        let cu_ixs = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(200_000),
            ComputeBudgetInstruction::set_compute_unit_price(100_000),
        ];

        let bh = rpc.get_latest_blockhash().await?;
        let mut ixs = cu_ixs;
        ixs.push(create_ata_ix);
        ixs.push(buy_ix);

        let v0_msg = message::v0::Message::try_compile(
            &payer.pubkey(), &ixs, &[], bh,
        )?;
        let tx = VersionedTransaction::try_new(
            VersionedMessage::V0(v0_msg), &[&payer],
        )?;
        let tx_bytes = bincode::serialize(&tx)?;
        println!("Buy TX: {} bytes, sig={}", tx_bytes.len(), tx.signatures[0]);

        // Send via RPC (not Astralane)
        match rpc.send_and_confirm_transaction(&solana_sdk::transaction::Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(200_000),
                ComputeBudgetInstruction::set_compute_unit_price(100_000),
                create_ata_idempotent_ix(&payer.pubkey(), &payer.pubkey(), &base_mint, &base_token_prog),
                Instruction {
                    program_id: pumpswap_prog,
                    accounts: vec![
                        AccountMeta::new(pool_addr, false),
                        AccountMeta::new(payer.pubkey(), true),
                        AccountMeta::new_readonly(global_config, false),
                        AccountMeta::new_readonly(base_mint, false),
                        AccountMeta::new_readonly(quote_mint, false),
                        AccountMeta::new(user_base_ata, false),
                        AccountMeta::new(user_quote_ata, false),
                        AccountMeta::new(pool_base_vault, false),
                        AccountMeta::new(pool_quote_vault, false),
                        AccountMeta::new(protocol_fee_recipient, false),
                        AccountMeta::new(protocol_fee_ata, false),
                        AccountMeta::new_readonly(base_token_prog, false),
                        AccountMeta::new_readonly(quote_token_prog, false),
                        AccountMeta::new_readonly(system, false),
                        AccountMeta::new_readonly(ata_prog, false),
                        AccountMeta::new_readonly(event_authority, false),
                        AccountMeta::new_readonly(pumpswap_prog, false),
                        AccountMeta::new(creator_vault_ata, false),
                        AccountMeta::new_readonly(creator_vault_authority, false),
                        AccountMeta::new(global_vol, false),
                        AccountMeta::new(user_vol, false),
                        AccountMeta::new_readonly(fee_config, false),
                        AccountMeta::new_readonly(fee_prog, false),
                        AccountMeta::new_readonly(pool_v2, false),
                    ],
                    data: {
                        let mut d = vec![0u8; 25];
                        d[0..8].copy_from_slice(&[198, 46, 21, 82, 180, 217, 232, 112]);
                        d[8..16].copy_from_slice(&1_000_000u64.to_le_bytes());
                        d[16..24].copy_from_slice(&1u64.to_le_bytes());
                        d[24] = 1;
                        d
                    },
                },
            ], Some(&payer.pubkey()), &[&payer], bh,
        )).await {
            Ok(sig) => println!("✅ Buy succeeded: {}", sig),
            Err(e) => println!("❌ Buy failed: {}", e),
        }
    } else {
        println!("Already have {} tokens, skipping buy", user_base_balance);
    }

    // Now sell whatever tokens we have
    let sell_amount = match rpc.get_account(&user_base_ata).await {
        Ok(acct) if acct.data.len() >= 72 => {
            u64::from_le_bytes(acct.data[64..72].try_into().unwrap_or([0;8]))
        }
        _ => 0,
    };

    if sell_amount == 0 {
        println!("No tokens to sell after buy attempt");
        return Ok(());
    }

    // pool_v2 PDA — required for all PumpSwap instructions
    let (pool_v2, _) = Pubkey::find_program_address(
        &[b"pool-v2", base_mint.as_ref()], &pumpswap_prog,
    );

    println!("\nSelling {} tokens directly via PumpSwap...", sell_amount);

    let sell_disc: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];
    let mut sell_data = vec![0u8; 16]; // 8 disc + 8 base_amount_in (no min_quote for direct)
    sell_data[0..8].copy_from_slice(&sell_disc);
    // Wait, sell has 2 args: base_amount_in + min_quote_amount_out
    let mut sell_data = vec![0u8; 24];
    sell_data[0..8].copy_from_slice(&sell_disc);
    sell_data[8..16].copy_from_slice(&sell_amount.to_le_bytes());
    sell_data[16..24].copy_from_slice(&1u64.to_le_bytes()); // min_quote = 1

    let sell_ix = Instruction {
        program_id: pumpswap_prog,
        accounts: vec![
            AccountMeta::new(pool_addr, false),
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new_readonly(global_config, false),
            AccountMeta::new_readonly(base_mint, false),
            AccountMeta::new_readonly(quote_mint, false),
            AccountMeta::new(user_base_ata, false),
            AccountMeta::new(user_quote_ata, false),
            AccountMeta::new(pool_base_vault, false),
            AccountMeta::new(pool_quote_vault, false),
            AccountMeta::new_readonly(protocol_fee_recipient, false),
            AccountMeta::new(protocol_fee_ata, false),
            AccountMeta::new_readonly(base_token_prog, false),
            AccountMeta::new_readonly(quote_token_prog, false),
            AccountMeta::new_readonly(system, false),
            AccountMeta::new_readonly(ata_prog, false),
            AccountMeta::new_readonly(event_authority, false),
            AccountMeta::new_readonly(pumpswap_prog, false),
            AccountMeta::new(creator_vault_ata, false),
            AccountMeta::new_readonly(creator_vault_authority, false),
            // remaining: fee_config, fee_program, pool_v2
            AccountMeta::new_readonly(fee_config, false),
            AccountMeta::new_readonly(fee_prog, false),
            AccountMeta::new_readonly(pool_v2, false),  // pool_v2 PDA — fixes Overflow
        ],
        data: sell_data,
    };

    let bh = rpc.get_latest_blockhash().await?;
    let sell_tx = solana_sdk::transaction::Transaction::new_signed_with_payer(
        &[
            ComputeBudgetInstruction::set_compute_unit_limit(200_000),
            ComputeBudgetInstruction::set_compute_unit_price(100_000),
            sell_ix,
        ],
        Some(&payer.pubkey()),
        &[&payer],
        bh,
    );
    println!("Sell TX sig: {}", sell_tx.signatures[0]);

    match rpc.send_and_confirm_transaction(&sell_tx).await {
        Ok(sig) => println!("✅ SELL SUCCEEDED: {}", sig),
        Err(e) => println!("❌ SELL FAILED: {}", e),
    }

    Ok(())
}
