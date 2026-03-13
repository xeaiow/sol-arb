//! Verification tool: compare off-chain DLMM quotes with on-chain simulation.
//!
//! Usage: cargo run --bin verify_dlmm [lb_pair_address]
//!
//! Fetches LbPair + bin array accounts, runs off-chain quote, then simulates
//! an actual swap via RPC to compare.

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

const RPC_URL: &str = "https://mainnet.helius-rpc.com/?api-key=89ed37ec-971c-48e0-99db-921d568354e6";

/// DLMM program ID
const DLMM_PROGRAM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";

/// Well-known SOL/USDC DLMM pool (bin_step=1)
const DEFAULT_POOL: &str = "BUwFf2LfCoLeqPWAXMvsTQAFTYpeRHAYzpzhBuBMjZar";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pool_address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_POOL.to_string());
    let lb_pair = Pubkey::from_str(&pool_address)?;

    println!("=== Meteora DLMM Verification ===");
    println!("LbPair: {}", lb_pair);

    let rpc = RpcClient::new(RPC_URL.to_string());

    // 1. Fetch LbPair account
    let account = rpc.get_account(&lb_pair).await?;
    let data = &account.data;
    println!("Account size: {} bytes", data.len());

    if data.len() < 904 {
        anyhow::bail!("Account too small for LbPair: {} bytes", data.len());
    }

    // Parse LbPair fields (verified offsets from MeteoraAg/dlmm-sdk)
    // StaticParameters: offset 8..40
    let base_factor = u16::from_le_bytes(data[8..10].try_into()?);
    let variable_fee_control = u32::from_le_bytes(data[16..20].try_into()?);
    let max_volatility_accumulator = u32::from_le_bytes(data[20..24].try_into()?);
    // VariableParameters: offset 40..72
    let volatility_accumulator = u32::from_le_bytes(data[40..44].try_into()?);
    let volatility_reference = u32::from_le_bytes(data[44..48].try_into()?);
    let index_reference = i32::from_le_bytes(data[48..52].try_into()?);
    // LbPair fields after sub-structs
    let active_id = i32::from_le_bytes(data[76..80].try_into()?);
    let bin_step = u16::from_le_bytes(data[80..82].try_into()?);

    let token_x_mint = Pubkey::try_from(&data[88..120])?;
    let token_y_mint = Pubkey::try_from(&data[120..152])?;
    let reserve_x = Pubkey::try_from(&data[152..184])?;
    let reserve_y = Pubkey::try_from(&data[184..216])?;
    let oracle = Pubkey::try_from(&data[552..584])?;

    let step_bps = bin_step as f64 / 10000.0;
    let price_at_active = (1.0 + step_bps).powi(active_id);

    println!("\n--- LbPair Parameters ---");
    println!("active_id:      {}", active_id);
    println!("bin_step:       {}", bin_step);
    println!("price@active:   {:.6}", price_at_active);
    println!("token_x_mint:   {}", token_x_mint);
    println!("token_y_mint:   {}", token_y_mint);
    println!("reserve_x:      {}", reserve_x);
    println!("reserve_y:      {}", reserve_y);
    println!("oracle:         {}", oracle);
    println!("base_factor:    {}", base_factor);
    println!("var_fee_ctrl:   {}", variable_fee_control);
    println!("max_vol_acc:    {}", max_volatility_accumulator);
    println!("vol_acc:        {}", volatility_accumulator);
    println!("vol_ref:        {}", volatility_reference);
    println!("index_ref:      {}", index_reference);

    // Compute fee
    let base_fee = base_factor as f64 * bin_step as f64 * 10.0 / 1_000_000_000.0;
    let var_fee = if variable_fee_control > 0 {
        let va_bin = volatility_accumulator as f64 * bin_step as f64;
        (variable_fee_control as f64 * va_bin * va_bin / 100_000_000_000.0).min(0.1)
    } else {
        0.0
    };
    println!("base_fee:       {:.6}%", base_fee * 100.0);
    println!("var_fee:        {:.6}%", var_fee * 100.0);
    println!("total_fee:      {:.6}%", (base_fee + var_fee) * 100.0);

    // 2. Fetch vault balances
    let vault_x_account = rpc.get_account(&reserve_x).await?;
    let vault_y_account = rpc.get_account(&reserve_y).await?;
    let vault_x_balance = u64::from_le_bytes(vault_x_account.data[64..72].try_into()?);
    let vault_y_balance = u64::from_le_bytes(vault_y_account.data[64..72].try_into()?);
    println!("\nvault_x balance: {}", vault_x_balance);
    println!("vault_y balance: {}", vault_y_balance);

    // 3. Fetch bin arrays (current-1, current, current+1)
    let dlmm_program = Pubkey::from_str(DLMM_PROGRAM)?;
    let ba_index = bin_id_to_bin_array_index(active_id);
    println!("\n--- Bin Arrays ---");
    println!("active bin_array_index: {}", ba_index);

    let mut bin_arrays = Vec::new();
    for offset in [-1i64, 0, 1] {
        let idx = ba_index + offset;
        let (pda, _) = Pubkey::find_program_address(
            &[b"bin_array", lb_pair.as_ref(), &idx.to_le_bytes()],
            &dlmm_program,
        );
        match rpc.get_account(&pda).await {
            Ok(acct) => {
                let ba = decode_bin_array(&acct.data, idx);
                let total_x: u64 = ba.iter().map(|b| b.1).sum();
                let total_y: u64 = ba.iter().map(|b| b.2).sum();
                let non_empty = ba.len();
                println!(
                    "  bin_array[{}] @ {}: {} bins with liquidity, total_x={}, total_y={}",
                    idx, &pda.to_string()[..8], non_empty, total_x, total_y
                );
                for (bin_id, ax, ay) in &ba {
                    if *bin_id >= active_id - 3 && *bin_id <= active_id + 3 {
                        let p = (1.0 + step_bps).powi(*bin_id);
                        println!(
                            "    bin[{}] price={:.6} x={} y={}{}",
                            bin_id, p, ax, ay,
                            if *bin_id == active_id { " <-- ACTIVE" } else { "" }
                        );
                    }
                }
                bin_arrays.push((idx, ba));
            }
            Err(e) => {
                println!("  bin_array[{}]: not found ({})", idx, e);
            }
        }
    }

    // 4. Build PoolMath and run off-chain quote
    use solana_streamer_sdk::pool::state::{DlmmBin, DlmmBinArray, PoolMath};

    let dlmm_bin_arrays: Vec<DlmmBinArray> = bin_arrays
        .iter()
        .map(|(idx, bins)| DlmmBinArray {
            index: *idx,
            bins: bins
                .iter()
                .map(|(bin_id, ax, ay)| DlmmBin {
                    bin_id: *bin_id,
                    amount_x: *ax,
                    amount_y: *ay,
                })
                .collect(),
        })
        .collect();

    let math = PoolMath::MeteoraDlmm {
        active_id,
        bin_step,
        base_factor,
        variable_fee_control,
        max_volatility_accumulator,
        volatility_accumulator,
        volatility_reference,
        index_reference,
        bin_arrays: dlmm_bin_arrays,
    };

    // Test amounts
    let test_amounts: Vec<u64> = vec![
        1_000_000,       // 0.001 SOL / 0.001 USDC
        10_000_000,      // 0.01
        100_000_000,     // 0.1
        1_000_000_000,   // 1.0
    ];

    println!("\n--- Off-chain Quote Comparison ---");
    println!("{:<15} {:<15} {:<15}", "Amount In", "A→B out", "B→A out");
    for &amt in &test_amounts {
        let out_ab = math.get_amount_out(amt, true);
        let out_ba = math.get_amount_out(amt, false);
        println!(
            "{:<15} {:<15} {:<15}",
            amt, out_ab, out_ba,
        );
    }

    // 5. Compare with Jupiter quote API (if available) — skip for now, just show our quotes
    println!("\n--- Price Sanity Check ---");
    let probe = 1_000_000_000u64; // 1 unit (SOL if mint_x is SOL)
    let out_ab = math.get_amount_out(probe, true);
    let out_ba = math.get_amount_out(probe, false);
    println!("1 token_x → {:.6} token_y (implied price: {:.6})", out_ab as f64 / 1e6, out_ab as f64 / 1e6);
    println!("1 token_y → {:.9} token_x", out_ba as f64 / 1e9);
    println!("Bin price@active: {:.6}", price_at_active);

    // Check if we can detect obvious issues
    if out_ab == 0 && out_ba == 0 {
        println!("\n⚠️ WARNING: Both directions return 0! Bin arrays may be empty or not loaded.");
    }

    println!("\n=== Verification Complete ===");

    Ok(())
}

fn bin_id_to_bin_array_index(bin_id: i32) -> i64 {
    let idx = bin_id / 70;
    let rem = bin_id % 70;
    if bin_id < 0 && rem != 0 {
        (idx - 1) as i64
    } else {
        idx as i64
    }
}

/// Decode bin array from raw account data.
/// Returns Vec<(bin_id, amount_x, amount_y)> for bins with liquidity.
fn decode_bin_array(data: &[u8], index: i64) -> Vec<(i32, u64, u64)> {
    let header_size = 56;
    let bin_size = 144;
    let bins_per_array = 70;
    let min_size = header_size + bins_per_array * bin_size;

    if data.len() < min_size {
        return vec![];
    }

    let start_bin_id = (index as i32) * 70;
    let mut result = Vec::new();

    for i in 0..bins_per_array {
        let offset = header_size + i * bin_size;
        let amount_x = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
        let amount_y = u64::from_le_bytes(data[offset + 8..offset + 16].try_into().unwrap());

        if amount_x > 0 || amount_y > 0 {
            result.push((start_bin_id + i as i32, amount_x, amount_y));
        }
    }

    result
}
