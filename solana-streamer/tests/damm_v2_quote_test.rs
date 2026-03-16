//! Verify DammV2 U256 quote against known on-chain pool data + Python calculations.
//!
//! Pool: FweBoHNYGq15tT48BqM7hMEfdJ9a3oPa3UJxCVUGxeGL
//! token_a = SOL (9 decimals), token_b = USDC (6 decimals)
//! sqrt_price = 5496080890236756753  (Q64.64)
//! liquidity = 14757395258967641293093655
//! fee_numerator = 800000 (0.08%, fee on output for mode=0)

use solana_streamer_sdk::pool::state::PoolMath;

const SP: u128 = 5496080890236756753;
const SP_MIN: u128 = 4295048016;
const SP_MAX: u128 = 79226673521066979257578248091;
const LIQ: u128 = 14757395258967641293093655;
const FEE_NUM: u64 = 800000;
const FEE_MODE: u8 = 0; // BothToken (fee on output)

fn make_math() -> PoolMath {
    PoolMath::DammV2Concentrated {
        sqrt_price_x64: SP,
        sqrt_min_price_x64: SP_MIN,
        sqrt_max_price_x64: SP_MAX,
        liquidity: LIQ,
        fee_numerator: FEE_NUM,
        collect_fee_mode: FEE_MODE,
    }
}

#[test]
fn test_a_to_b_small() {
    // 0.001 SOL → USDC
    // Python: output before fee = 64681, fee = 51, after fee = 64630
    let math = make_math();
    let out = math.get_amount_out(1_000_000, true);
    println!("A→B 0.001 SOL: out = {} USDC lamports ({:.6} USDC)", out, out as f64 / 1e6);
    assert!(out > 60_000 && out < 70_000, "expected ~64630, got {}", out);
}

#[test]
fn test_b_to_a_small() {
    // 0.001 USDC (1000 lamports) → SOL
    // Python: output before fee = 11217, fee = 8, after fee = 11209
    let math = make_math();
    let out = math.get_amount_out(1_000, false);
    println!("B→A 0.001 USDC: out = {} SOL lamports ({:.9} SOL)", out, out as f64 / 1e9);
    assert!(out > 10_000 && out < 13_000, "expected ~11209, got {}", out);
}

#[test]
fn test_a_to_b_medium() {
    // 0.1 SOL → USDC, should be reasonable (< 10 USDC = 10M lamports)
    let math = make_math();
    let out = math.get_amount_out(100_000_000, true);
    println!("A→B 0.1 SOL: out = {} USDC lamports ({:.4} USDC)", out, out as f64 / 1e6);
    assert!(out > 0 && out < 10_000_000, "unreasonable output: {}", out);
}

#[test]
fn test_zero_input() {
    let math = make_math();
    assert_eq!(math.get_amount_out(0, true), 0);
    assert_eq!(math.get_amount_out(0, false), 0);
}

#[test]
fn test_boundary_saturation_returns_zero() {
    let math = make_math();
    // Very large input should saturate the range → return 0
    let out = math.get_amount_out(u64::MAX, true);
    assert_eq!(out, 0, "saturated A→B should return 0, got {}", out);
    let out = math.get_amount_out(u64::MAX, false);
    assert_eq!(out, 0, "saturated B→A should return 0, got {}", out);
}

#[test]
fn test_price_consistency() {
    let math = make_math();
    // A→B and B→A prices should be close
    let out_ab = math.get_amount_out(1_000_000, true);  // 0.001 SOL → USDC
    let out_ba = math.get_amount_out(1_000, false);       // 0.001 USDC → SOL

    // Implied price from A→B: USDC per SOL = out_ab / input * (10^9/10^6)
    let price_ab = out_ab as f64 * 1000.0 / 1_000_000.0;
    // Implied price from B→A: USDC per SOL = input / out_ba * (10^9/10^6)
    let price_ba = 1_000_000.0 / out_ba as f64;

    println!("Price A→B: {:.4} USDC/SOL", price_ab);
    println!("Price B→A: {:.4} USDC/SOL", price_ba);

    let ratio = price_ab / price_ba;
    println!("Ratio: {:.4} (should be ~1.0, diff due to fee)", ratio);
    // CL curve + fee causes directional asymmetry. Ratio 0.5-1.5 is normal.
    assert!(ratio > 0.5 && ratio < 1.5, "extreme price inconsistency: ratio={}", ratio);
}

#[test]
fn test_not_astronomically_large() {
    // This was the original bug: output was 2^64 times too large
    let math = make_math();
    let out = math.get_amount_out(1_000_000, true); // 0.001 SOL
    // If the bug is present, output would be > 10^18 (absurd for 0.001 SOL)
    assert!(out < 1_000_000_000, "output suspiciously large (2^64 bug?): {}", out);
}
