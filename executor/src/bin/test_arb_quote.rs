//! Test: replicate the reference arb transaction's quote calculation.
//!
//! Reference tx: 4asHwZ2FeRg67ury736nqs6PFsetDyqoJhdyqtASKjYAG5TWQCs4JnMwZk8wHnTE8kvCApQ3YSwN7cYem1c8YR36
//! Route: PumpSwap(GVQgwAg8) buy SOL→E93JuGQc token → DLMM(3EMXSmVS) sell token→SOL
//!
//! Usage: cd executor && cargo run --release --bin test_arb_quote

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;

use solana_streamer_sdk::pool::decoder;

const RPC: &str = "http://45.157.234.194:8899";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let rpc = RpcClient::new(RPC.to_string());

    let pumpswap_pool: Pubkey = "GVQgwAg8HqhMVud7U7x3XgTkp7A1J6CS4B1eoPsFwKLQ".parse()?;
    let dlmm_pool: Pubkey = "3EMXSmVSfaKPn6LVPHZVFn1s5ufvsB4R27wG6oS4bC92".parse()?;

    // === Decode PumpSwap pool ===
    println!("=== PumpSwap Pool ===");
    let ps_data = rpc.get_account_data(&pumpswap_pool).await?;
    let ps_state = decoder::pumpswap::decode_bytes(&pumpswap_pool, &ps_data)
        .ok_or_else(|| anyhow::anyhow!("Failed to decode PumpSwap pool"))?;
    println!("  mint_a (base): {}", ps_state.mint_a);
    println!("  mint_b (quote/SOL): {}", ps_state.mint_b);
    println!("  math: {:?}", ps_state.math);

    // Fetch vault balances
    let ps_vault_a = ps_state.vault_a.unwrap();
    let ps_vault_b = ps_state.vault_b.unwrap();
    let ps_va_data = rpc.get_account_data(&ps_vault_a).await?;
    let ps_vb_data = rpc.get_account_data(&ps_vault_b).await?;
    let ps_balance_a = u64::from_le_bytes(ps_va_data[64..72].try_into()?);
    let ps_balance_b = u64::from_le_bytes(ps_vb_data[64..72].try_into()?);
    println!("  vault_a balance (token): {}", ps_balance_a);
    println!("  vault_b balance (SOL):   {}", ps_balance_b);

    // Build math with actual reserves and correct fee (PumpSwap total = 25 bps)
    let ps_math = solana_streamer_sdk::pool::state::PoolMath::ConstantProduct {
        reserve_a: ps_balance_a,
        reserve_b: ps_balance_b,
        fee_numerator: 25,
        fee_denominator: 10000,
    };
    println!("  math (with reserves): {:?}", ps_math);

    // === Decode DLMM pool ===
    println!("\n=== DLMM Pool ===");
    let dlmm_data = rpc.get_account_data(&dlmm_pool).await?;
    let mut dlmm_state = decoder::meteora_dlmm::decode_bytes(&dlmm_pool, &dlmm_data)
        .ok_or_else(|| anyhow::anyhow!("Failed to decode DLMM pool"))?;
    println!("  mint_a (token_x): {}", dlmm_state.mint_a);
    println!("  mint_b (token_y/SOL): {}", dlmm_state.mint_b);

    // Fetch bin arrays
    let active_id = match &dlmm_state.math {
        solana_streamer_sdk::pool::state::PoolMath::MeteoraDlmm { active_id, .. } => *active_id,
        _ => return Err(anyhow::anyhow!("not DLMM")),
    };
    println!("  active_id: {}", active_id);

    let bin_pdas = decoder::meteora_dlmm::bin_array_pdas_for_swap(&dlmm_pool, active_id);
    let mut bin_arrays = Vec::new();
    for pda in &bin_pdas {
        match rpc.get_account_data(pda).await {
            Ok(data) => {
                if let Some(ba) = decoder::meteora_dlmm::decode_bin_array(&data) {
                    let bins_with_liq = ba.bins.iter().filter(|b| b.amount_x > 0 || b.amount_y > 0).count();
                    println!("  bin_array {}: index={}, bins_with_liq={}", pda, ba.index, bins_with_liq);
                    bin_arrays.push(ba);
                }
            }
            Err(e) => println!("  bin_array {} not found: {}", pda, e),
        }
    }

    // Update DLMM math with bin arrays
    if let solana_streamer_sdk::pool::state::PoolMath::MeteoraDlmm { bin_arrays: ref mut ba, .. } = dlmm_state.math {
        *ba = bin_arrays;
    }

    // === Simulate the arb route ===
    println!("\n=== Quote Simulation ===");

    // Test amounts: 0.5, 1.0, 1.5 SOL
    for sol_amount in [0.5, 1.0, 1.5] {
        let input_lamports = (sol_amount * 1e9) as u64;

        // Hop 1: PumpSwap buy (SOL → token, is_a_to_b = false since mint_b = SOL)
        let tokens_out = ps_math.get_amount_out(input_lamports, false);

        // Hop 2: DLMM sell (token → SOL, is_a_to_b depends on which side is SOL)
        // DLMM: mint_a = token, mint_b = SOL → selling token means a_to_b = true
        let sol_out = dlmm_state.math.get_amount_out(tokens_out, true);

        let profit = sol_out as i64 - input_lamports as i64;
        println!("  {:.1} SOL → {} tokens → {} SOL | profit = {} lamports ({:.6} SOL)",
            sol_amount, tokens_out, sol_out, profit, profit as f64 / 1e9);
    }

    // Also test reverse: DLMM buy, PumpSwap sell
    println!("\n=== Reverse Route ===");
    for sol_amount in [0.5, 1.0, 1.5] {
        let input_lamports = (sol_amount * 1e9) as u64;

        // Hop 1: DLMM buy (SOL → token, b_to_a, is_a_to_b = false)
        let tokens_out = dlmm_state.math.get_amount_out(input_lamports, false);

        // Hop 2: PumpSwap sell (token → SOL, a_to_b = true)
        let sol_out = ps_math.get_amount_out(tokens_out, true);

        let profit = sol_out as i64 - input_lamports as i64;
        println!("  {:.1} SOL → {} tokens → {} SOL | profit = {} lamports ({:.6} SOL)",
            sol_amount, tokens_out, sol_out, profit, profit as f64 / 1e9);
    }

    Ok(())
}
