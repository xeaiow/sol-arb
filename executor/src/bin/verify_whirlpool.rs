//! Verification tool: compare off-chain Whirlpool quotes with on-chain simulation.
//!
//! Usage: cargo run --bin verify_whirlpool [whirlpool_address]
//!
//! Fetches Whirlpool account + tick array accounts, runs off-chain quote using
//! the CURRENT decoder logic (which has a suspected tick index bug), then shows
//! the decoded tick indices so we can verify correctness.
//!
//! The tool demonstrates the bug by:
//! 1. Decoding tick arrays with the current logic (start + i)
//! 2. Decoding tick arrays with the CORRECT logic (start + i * tick_spacing)
//! 3. Computing quotes with both and comparing

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

/// RPC endpoint
const RPC_URL: &str = "https://beta.helius-rpc.com/?api-key=89ed37ec-971c-48e0-99db-921d568354e6";

/// Orca Whirlpool program ID
const WHIRLPOOL_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";

/// Well-known SOL/USDC Whirlpool (tick_spacing=64)
const DEFAULT_POOL: &str = "HJPjoWUrhoZzkNfRpHuieeFk9AnbVuuYGBePmsSzXEat";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pool_address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_POOL.to_string());
    let whirlpool = Pubkey::from_str(&pool_address)?;
    let whirlpool_program = Pubkey::from_str(WHIRLPOOL_PROGRAM)?;

    println!("=== Orca Whirlpool Tick Index Verification ===");
    println!("Whirlpool: {}", whirlpool);

    let rpc = RpcClient::new(RPC_URL.to_string());

    // 1. Fetch Whirlpool account
    let account = rpc.get_account(&whirlpool).await?;
    let data = &account.data;
    println!("Account size: {} bytes", data.len());

    if data.len() < 653 {
        anyhow::bail!("Account too small for Whirlpool: {} bytes", data.len());
    }

    let tick_spacing = u16::from_le_bytes(data[41..43].try_into()?);
    let fee_rate = u16::from_le_bytes(data[45..47].try_into()?);
    let liquidity = u128::from_le_bytes(data[49..65].try_into()?);
    let sqrt_price_x64 = u128::from_le_bytes(data[65..81].try_into()?);
    let tick_current = i32::from_le_bytes(data[81..85].try_into()?);
    let token_mint_a = Pubkey::try_from(&data[101..133])?;
    let token_mint_b = Pubkey::try_from(&data[181..213])?;

    let q64: f64 = (1u128 << 64) as f64;
    let sqrt_price_f64 = sqrt_price_x64 as f64 / q64;
    let price = sqrt_price_f64 * sqrt_price_f64;

    println!("\n--- Whirlpool Parameters ---");
    println!("tick_spacing:    {}", tick_spacing);
    println!("fee_rate:        {} bps ({:.2}%)", fee_rate, fee_rate as f64 / 100.0);
    println!("liquidity:       {}", liquidity);
    println!("sqrt_price_x64:  {}", sqrt_price_x64);
    println!("sqrt_price:      {:.10}", sqrt_price_f64);
    println!("price:           {:.6} (B per A)", price);
    println!("tick_current:    {}", tick_current);
    println!("token_mint_a:    {}", token_mint_a);
    println!("token_mint_b:    {}", token_mint_b);

    // Verify: tick_current should match sqrt_price
    let expected_tick = (price.ln() / 1.0001_f64.ln()).round() as i32;
    println!("expected_tick:   {} (from price)", expected_tick);
    println!("tick drift:      {} ticks", (tick_current - expected_tick).abs());

    // 2. Fetch tick arrays (current ± 3)
    let ticks_in_array = tick_spacing as i32 * 88;
    let current_start = {
        let mut s = tick_current / ticks_in_array * ticks_in_array;
        if tick_current < 0 && tick_current % ticks_in_array != 0 {
            s -= ticks_in_array;
        }
        s
    };

    println!("\n--- Tick Arrays ---");
    println!("ticks_in_array:  {} (tick_spacing={} × 88)", ticks_in_array, tick_spacing);
    println!("current_start:   {}", current_start);

    let mut all_ticks_buggy: Vec<(i32, i128)> = Vec::new();
    let mut all_ticks_fixed: Vec<(i32, i128)> = Vec::new();

    for offset in -3i32..=3 {
        let start_index = current_start + offset * ticks_in_array;
        let start_str = start_index.to_string();
        let (pda, _) = Pubkey::find_program_address(
            &[b"tick_array", whirlpool.as_ref(), start_str.as_bytes()],
            &whirlpool_program,
        );

        match rpc.get_account_data(&pda).await {
            Ok(ta_data) => {
                // Orca tick arrays: 8 disc + 4 start_index + 88*113 ticks + 32 whirlpool = 9988
                if ta_data.len() < 9988 {
                    println!("  tick_array[{}]: too small ({} bytes)", start_index, ta_data.len());
                    continue;
                }
                let ta_start = i32::from_le_bytes(ta_data[8..12].try_into()?);
                assert_eq!(ta_start, start_index, "start_tick_index mismatch!");

                let mut count_initialized = 0u32;
                let mut byte_offset = 12;
                for i in 0..88usize {
                    if ta_data.len() < byte_offset + 113 {
                        break;
                    }
                    let initialized = ta_data[byte_offset] != 0;
                    byte_offset += 1;
                    let liquidity_net =
                        i128::from_le_bytes(ta_data[byte_offset..byte_offset + 16].try_into()?);
                    byte_offset += 16;
                    let liquidity_gross =
                        u128::from_le_bytes(ta_data[byte_offset..byte_offset + 16].try_into()?);
                    byte_offset += 16;
                    // Skip remaining 80 bytes
                    byte_offset += 80;

                    if initialized && liquidity_gross > 0 {
                        count_initialized += 1;

                        // BUGGY: start + i (what current code does)
                        let buggy_index = start_index + i as i32;
                        all_ticks_buggy.push((buggy_index, liquidity_net));

                        // FIXED: start + i * tick_spacing
                        let fixed_index = start_index + (i as i32 * tick_spacing as i32);
                        all_ticks_fixed.push((fixed_index, liquidity_net));
                    }
                }
                println!(
                    "  tick_array[{}] @ {}: {} initialized ticks",
                    start_index,
                    &pda.to_string()[..8],
                    count_initialized,
                );
            }
            Err(e) => {
                println!("  tick_array[{}]: not found ({})", start_index, e);
            }
        }
    }

    // 3. Show tick index comparison
    println!("\n--- Tick Index Comparison (first 20 ticks) ---");
    println!(
        "{:<8} {:<12} {:<12} {:<15} {:<15}",
        "Slot#", "BUGGY idx", "FIXED idx", "BUGGY sqrt_p", "FIXED sqrt_p"
    );
    let display_count = all_ticks_buggy.len().min(20);
    for i in 0..display_count {
        let (buggy_idx, _) = all_ticks_buggy[i];
        let (fixed_idx, _) = all_ticks_fixed[i];
        let buggy_sqrt = tick_index_to_sqrt_price(buggy_idx);
        let fixed_sqrt = tick_index_to_sqrt_price(fixed_idx);
        println!(
            "{:<8} {:<12} {:<12} {:<15.10} {:<15.10}",
            i, buggy_idx, fixed_idx, buggy_sqrt, fixed_sqrt,
        );
    }

    if all_ticks_buggy.is_empty() {
        println!("  (no initialized ticks found)");
        println!("\n=== Verification Complete (no ticks to compare) ===");
        return Ok(());
    }

    // 4. Compute quotes with both tick index schemes
    let fee_rate_1e6 = fee_rate as u32 * 100;

    let test_amounts: Vec<u64> = vec![
        10_000_000,       // 0.01 SOL
        100_000_000,      // 0.1 SOL
        1_000_000_000,    // 1 SOL
        5_000_000_000,    // 5 SOL
        50_000_000_000,   // 50 SOL
        500_000_000_000,  // 500 SOL
    ];

    println!("\n--- Quote Comparison: A→B (sell token_a) ---");
    println!(
        "{:<15} {:<18} {:<18} {:<10}",
        "Amount In", "BUGGY quote", "FIXED quote", "Diff %"
    );
    for &amt in &test_amounts {
        let buggy_out = clmm_quote(
            amt, true, sqrt_price_x64, liquidity, tick_current, fee_rate_1e6, &all_ticks_buggy,
        );
        let fixed_out = clmm_quote(
            amt, true, sqrt_price_x64, liquidity, tick_current, fee_rate_1e6, &all_ticks_fixed,
        );
        let diff = if fixed_out > 0 {
            ((buggy_out as f64 - fixed_out as f64) / fixed_out as f64) * 100.0
        } else {
            f64::NAN
        };
        println!(
            "{:<15} {:<18} {:<18} {:<+10.2}%",
            amt, buggy_out, fixed_out, diff,
        );
    }

    println!("\n--- Quote Comparison: B→A (sell token_b) ---");
    println!(
        "{:<15} {:<18} {:<18} {:<10}",
        "Amount In", "BUGGY quote", "FIXED quote", "Diff %"
    );
    for &amt in &test_amounts {
        let buggy_out = clmm_quote(
            amt, false, sqrt_price_x64, liquidity, tick_current, fee_rate_1e6, &all_ticks_buggy,
        );
        let fixed_out = clmm_quote(
            amt, false, sqrt_price_x64, liquidity, tick_current, fee_rate_1e6, &all_ticks_fixed,
        );
        let diff = if fixed_out > 0 {
            ((buggy_out as f64 - fixed_out as f64) / fixed_out as f64) * 100.0
        } else {
            f64::NAN
        };
        println!(
            "{:<15} {:<18} {:<18} {:<+10.2}%",
            amt, buggy_out, fixed_out, diff,
        );
    }

    // 5. Sanity check: does tick_current make sense with the FIXED tick indices?
    println!("\n--- Sanity Check ---");
    let nearest_below: Option<&(i32, i128)> = all_ticks_fixed
        .iter()
        .filter(|(idx, _)| *idx <= tick_current)
        .last();
    let nearest_above: Option<&(i32, i128)> = all_ticks_fixed
        .iter()
        .filter(|(idx, _)| *idx > tick_current)
        .next();
    if let Some((idx, _)) = nearest_below {
        println!("nearest tick below tick_current({}): {}", tick_current, idx);
    }
    if let Some((idx, _)) = nearest_above {
        println!("nearest tick above tick_current({}): {}", tick_current, idx);
    }

    let nearest_below_buggy: Option<&(i32, i128)> = all_ticks_buggy
        .iter()
        .filter(|(idx, _)| *idx <= tick_current)
        .last();
    if let Some((idx, _)) = nearest_below_buggy {
        let buggy_price = tick_index_to_sqrt_price(*idx).powi(2);
        println!(
            "BUGGY nearest below: {} → price {:.6} (real price: {:.6})",
            idx, buggy_price, price,
        );
    }

    // ======================================================================
    // Phase 2: Full Pipeline Verification
    // Use the actual production decoder + PoolMath::Concentrated to verify
    // that the FIXED decoder produces correct quotes through the real pipeline.
    // ======================================================================
    println!("\n=== Phase 2: Full Pipeline Verification (production decoder) ===");

    use solana_streamer_sdk::pool::decoder::orca_whirlpool as wp_decoder;
    use solana_streamer_sdk::pool::state::PoolMath;

    // Decode pool state using production decoder
    let mut pool_state = wp_decoder::decode_bytes(&whirlpool, data)
        .expect("Failed to decode whirlpool with production decoder");

    println!("dex_type:        {:?}", pool_state.dex_type);
    if let PoolMath::Concentrated { tick_spacing: ts, fee_rate: fr, tick_current: tc, .. } = &pool_state.math {
        println!("tick_spacing:    {}", ts);
        println!("fee_rate (1e-6): {}", fr);
        println!("tick_current:    {}", tc);
    }

    // Fetch tick arrays using production decoder (now with tick_spacing fix)
    {
        let PoolMath::Concentrated { ref mut tick_arrays, tick_current: tc, tick_spacing: ts, .. } = pool_state.math else {
            anyhow::bail!("Not a Concentrated pool");
        };

        let start_indices = wp_decoder::tick_array_start_indices(tc, ts);
        let mut loaded = 0u32;
        for start_index in start_indices {
            if let Some(pda) = wp_decoder::tick_array_pda(&whirlpool, start_index) {
                if let Ok(ta_data) = rpc.get_account_data(&pda).await {
                    if let Some(ta) = wp_decoder::decode_tick_array(&ta_data, ts) {
                        let tick_count = ta.ticks.len();
                        tick_arrays.push(ta);
                        loaded += 1;
                        if loaded <= 3 {
                            println!("  loaded tick_array[{}]: {} ticks", start_index, tick_count);
                        }
                    }
                }
            }
        }
        println!("  total: {} tick arrays loaded", loaded);
    }

    // Run production quotes
    println!("\n--- Production Pipeline Quotes: A→B ---");
    println!(
        "{:<15} {:<18} {:<18} {:<10}",
        "Amount In", "Pipeline quote", "FIXED local", "Diff %"
    );
    for &amt in &test_amounts {
        let pipeline_out = pool_state.math.get_amount_out(amt, true);
        let local_out = clmm_quote(
            amt, true, sqrt_price_x64, liquidity, tick_current, fee_rate_1e6, &all_ticks_fixed,
        );
        let diff = if local_out > 0 {
            ((pipeline_out as f64 - local_out as f64) / local_out as f64) * 100.0
        } else {
            f64::NAN
        };
        println!(
            "{:<15} {:<18} {:<18} {:<+10.2}%",
            amt, pipeline_out, local_out, diff,
        );
    }

    println!("\n--- Production Pipeline Quotes: B→A ---");
    println!(
        "{:<15} {:<18} {:<18} {:<10}",
        "Amount In", "Pipeline quote", "FIXED local", "Diff %"
    );
    for &amt in &test_amounts {
        let pipeline_out = pool_state.math.get_amount_out(amt, false);
        let local_out = clmm_quote(
            amt, false, sqrt_price_x64, liquidity, tick_current, fee_rate_1e6, &all_ticks_fixed,
        );
        let diff = if local_out > 0 {
            ((pipeline_out as f64 - local_out as f64) / local_out as f64) * 100.0
        } else {
            f64::NAN
        };
        println!(
            "{:<15} {:<18} {:<18} {:<+10.2}%",
            amt, pipeline_out, local_out, diff,
        );
    }

    // Verify tick indices in loaded tick arrays
    if let PoolMath::Concentrated { ref tick_arrays, tick_current: tc, tick_spacing: ts, .. } = pool_state.math {
        println!("\n--- Tick Index Verification (production decoder) ---");
        let mut all_ticks: Vec<(i32, i128)> = tick_arrays
            .iter()
            .flat_map(|ta| ta.ticks.iter().map(|t| (t.index, t.liquidity_net)))
            .collect();
        all_ticks.sort_by_key(|(idx, _)| *idx);
        let below = all_ticks.iter().filter(|(idx, _)| *idx <= tc).last();
        let above = all_ticks.iter().filter(|(idx, _)| *idx > tc).next();
        if let Some((idx, _)) = below {
            let p = tick_index_to_sqrt_price(*idx).powi(2);
            println!("nearest tick below: {} (price {:.6})", idx, p);
        }
        if let Some((idx, _)) = above {
            let p = tick_index_to_sqrt_price(*idx).powi(2);
            println!("nearest tick above: {} (price {:.6})", idx, p);
        }
        println!("tick_spacing:    {}", ts);
        println!("real price:      {:.6}", price);

        // Check tick indices are multiples of tick_spacing
        let bad_ticks: Vec<i32> = all_ticks
            .iter()
            .filter(|(idx, _)| *idx % ts as i32 != 0)
            .map(|(idx, _)| *idx)
            .take(5)
            .collect();
        if bad_ticks.is_empty() {
            println!("✓ All tick indices are multiples of tick_spacing={}", ts);
        } else {
            println!("✗ BAD: tick indices not multiples of tick_spacing={}: {:?}", ts, bad_ticks);
        }
    }

    println!("\n=== Full Verification Complete ===");
    Ok(())
}

/// sqrt_price = 1.0001^(tick/2)
fn tick_index_to_sqrt_price(tick: i32) -> f64 {
    1.0001_f64.powf(tick as f64 / 2.0)
}

/// Simplified CLMM quote matching the logic in state.rs clmm_get_amount_out().
/// Takes pre-sorted ticks as (index, liquidity_net) pairs.
fn clmm_quote(
    amount_in: u64,
    is_a_to_b: bool,
    sqrt_price_x64: u128,
    liquidity: u128,
    tick_current: i32,
    fee_rate_1e6: u32,
    ticks: &[(i32, i128)],
) -> u64 {
    if fee_rate_1e6 == 0 || ticks.is_empty() {
        return 0;
    }

    let q64 = (1u128 << 64) as f64;
    let mut sqrt_price = sqrt_price_x64 as f64 / q64;
    let mut liq = liquidity as f64;
    let fee_pct = fee_rate_1e6 as f64 / 1_000_000.0;
    let mut remaining = amount_in as f64 * (1.0 - fee_pct);
    let mut amount_out: f64 = 0.0;

    if remaining <= 0.0 || liq <= 0.0 {
        return 0;
    }

    let mut sorted_ticks = ticks.to_vec();
    sorted_ticks.sort_by_key(|(idx, _)| *idx);

    if is_a_to_b {
        let relevant: Vec<&(i32, i128)> = sorted_ticks
            .iter()
            .filter(|(idx, _)| *idx <= tick_current)
            .rev()
            .collect();

        for (tick_idx, liq_net) in relevant {
            if remaining <= 0.0 {
                break;
            }
            let tick_sqrt_price = tick_index_to_sqrt_price(*tick_idx);
            if tick_sqrt_price >= sqrt_price || tick_sqrt_price <= 0.0 {
                continue;
            }
            let max_a = liq * (1.0 / tick_sqrt_price - 1.0 / sqrt_price);
            if max_a <= 0.0 {
                continue;
            }
            let consumed = remaining.min(max_a);
            let new_sqrt_price = if consumed >= max_a {
                tick_sqrt_price
            } else {
                liq * sqrt_price / (liq + consumed * sqrt_price)
            };
            let out = liq * (sqrt_price - new_sqrt_price);
            amount_out += out;
            remaining -= consumed;
            sqrt_price = new_sqrt_price;
            if consumed >= max_a {
                liq -= *liq_net as f64;
                if liq <= 0.0 {
                    break;
                }
            }
        }
    } else {
        let relevant: Vec<&(i32, i128)> = sorted_ticks
            .iter()
            .filter(|(idx, _)| *idx > tick_current)
            .collect();

        for (tick_idx, liq_net) in relevant {
            if remaining <= 0.0 {
                break;
            }
            let tick_sqrt_price = tick_index_to_sqrt_price(*tick_idx);
            if tick_sqrt_price <= sqrt_price {
                continue;
            }
            let max_b = liq * (tick_sqrt_price - sqrt_price);
            if max_b <= 0.0 {
                continue;
            }
            let consumed = remaining.min(max_b);
            let new_sqrt_price = if consumed >= max_b {
                tick_sqrt_price
            } else {
                sqrt_price + consumed / liq
            };
            let out = liq * (1.0 / sqrt_price - 1.0 / new_sqrt_price);
            amount_out += out;
            remaining -= consumed;
            sqrt_price = new_sqrt_price;
            if consumed >= max_b {
                liq += *liq_net as f64;
                if liq <= 0.0 {
                    break;
                }
            }
        }
    }

    let out = amount_out * 0.997; // same haircut as production
    out.max(0.0) as u64
}
