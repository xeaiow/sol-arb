//! Test script: getProgramAccounts for each DEX with correct filters.
//!
//! Usage:
//!   cargo run --release --bin test_gpa

use std::time::Instant;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcProgramAccountsConfig, RpcAccountInfoConfig};
use solana_client::rpc_filter::{RpcFilterType, Memcmp};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::pubkey;
use solana_sdk::pubkey::Pubkey;

const RPC_URL: &str = "https://beta.helius-rpc.com/?api-key=89ed37ec-971c-48e0-99db-921d568354e6";
const WSOL: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

struct DexFilter {
    name: &'static str,
    program_id: Pubkey,
    data_size: Option<u64>,
    /// 8-byte Anchor discriminator (None for non-Anchor programs)
    discriminator: Option<[u8; 8]>,
    /// (offset_mint_a, offset_mint_b)
    mint_offsets: (usize, usize),
}

fn dex_list() -> Vec<DexFilter> {
    vec![
        DexFilter {
            name: "Raydium CPMM",
            program_id: pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C"),
            data_size: Some(637),  // 629 + 8
            discriminator: Some([0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46]),
            mint_offsets: (168, 200),  // token_0_mint, token_1_mint
        },
        DexFilter {
            name: "Raydium CLMM",
            program_id: pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK"),
            data_size: Some(1544),  // 1536 + 8
            discriminator: None,
            mint_offsets: (73, 105),  // token_mint0, token_mint1
        },
        DexFilter {
            name: "Meteora DLMM",
            program_id: pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo"),
            data_size: Some(904),  // from IDL
            discriminator: None,
            mint_offsets: (88, 120),  // token_x_mint, token_y_mint
        },
        DexFilter {
            name: "Meteora DAMM V2",
            program_id: pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG"),
            data_size: None,  // unknown exact size
            discriminator: Some([0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc]),
            mint_offsets: (168, 200),  // token_a_mint, token_b_mint
        },
        DexFilter {
            name: "Orca Whirlpool",
            program_id: pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc"),
            data_size: Some(653),  // from IDL
            discriminator: None,
            mint_offsets: (101, 181),  // token_mint_a, token_mint_b
        },
    ]
}

async fn query(
    rpc: &RpcClient,
    program_id: &Pubkey,
    filters: Vec<RpcFilterType>,
    label: &str,
) -> Option<usize> {
    let config = RpcProgramAccountsConfig {
        filters: Some(filters),
        account_config: RpcAccountInfoConfig {
            encoding: Some(solana_account_decoder::UiAccountEncoding::Base64),
            data_slice: Some(solana_account_decoder::UiDataSliceConfig {
                offset: 0,
                length: 0,
            }),
            commitment: Some(CommitmentConfig::confirmed()),
            ..Default::default()
        },
        ..Default::default()
    };

    let t = Instant::now();
    match rpc.get_program_accounts_with_config(program_id, config).await {
        Ok(accounts) => {
            println!("  {}: {} ({:.1}s)", label, accounts.len(), t.elapsed().as_secs_f64());
            Some(accounts.len())
        }
        Err(e) => {
            let msg = e.to_string();
            let short = if msg.len() > 80 { &msg[..80] } else { &msg };
            println!("  {}: ERROR ({:.1}s) — {}", label, t.elapsed().as_secs_f64(), short);
            None
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed());

    println!("=== getProgramAccounts Test (with discriminator + dataSize) ===\n");

    let mut grand_total = 0usize;

    for dex in dex_list() {
        println!("--- {} ({}) ---", dex.name, dex.program_id);

        // Build base filters: dataSize and/or discriminator
        let mut base = Vec::new();
        if let Some(size) = dex.data_size {
            base.push(RpcFilterType::DataSize(size));
        }
        if let Some(disc) = dex.discriminator {
            base.push(RpcFilterType::Memcmp(
                Memcmp::new_raw_bytes(0, disc.to_vec()),
            ));
        }

        // SOL as mint_b
        let mut fb = base.clone();
        fb.push(RpcFilterType::Memcmp(
            Memcmp::new_raw_bytes(dex.mint_offsets.1, WSOL.to_bytes().to_vec()),
        ));
        let count_b = query(&rpc, &dex.program_id, fb, "SOL as B").await.unwrap_or(0);

        // SOL as mint_a
        let mut fa = base.clone();
        fa.push(RpcFilterType::Memcmp(
            Memcmp::new_raw_bytes(dex.mint_offsets.0, WSOL.to_bytes().to_vec()),
        ));
        let count_a = query(&rpc, &dex.program_id, fa, "SOL as A").await.unwrap_or(0);

        let total = count_a + count_b;
        grand_total += total;
        println!("  TOTAL SOL pairs: ~{}", total);
        println!();
    }

    println!("=== Grand total SOL-paired pools: ~{} ===", grand_total);
    Ok(())
}
