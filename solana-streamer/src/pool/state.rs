use solana_sdk::pubkey::Pubkey;

/// Simplified tick for off-chain quoting
#[derive(Debug, Clone)]
pub struct Tick {
    pub index: i32,
    pub liquidity_net: i128,
}

/// Pre-loaded tick array for CLMM cross-tick traversal
#[derive(Debug, Clone)]
pub struct TickArray {
    pub start_tick_index: i32,
    pub ticks: Vec<Tick>,
}

/// A single DLMM bin with liquidity info
#[derive(Debug, Clone)]
pub struct DlmmBin {
    pub bin_id: i32,
    pub amount_x: u64,
    pub amount_y: u64,
}

/// Pre-loaded DLMM bin array (70 bins per array)
#[derive(Debug, Clone)]
pub struct DlmmBinArray {
    pub index: i64,
    pub bins: Vec<DlmmBin>,
}

/// DEX type identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DexType {
    RaydiumAmmV4 = 0,
    RaydiumCpmm = 1,
    RaydiumClmm = 2,
    PumpFun = 3,
    PumpSwap = 4,
    Bonk = 5,
    MeteoraDammV2 = 6,
    MeteoraDlmm = 7,
    OrcaWhirlpool = 8,
}

/// Pool pricing math — off-chain only, f64 fast-path
#[derive(Debug, Clone)]
pub enum PoolMath {
    /// x * y = k (Raydium AMM V4/CPMM, PumpSwap, Bonk)
    ConstantProduct {
        reserve_a: u64,
        reserve_b: u64,
        fee_numerator: u64,
        fee_denominator: u64,
    },

    /// Tick-based CLMM (Raydium CLMM)
    Concentrated {
        sqrt_price_x64: u128,
        liquidity: u128,
        tick_current: i32,
        tick_spacing: u16,
        fee_rate: u32,
        /// Pre-loaded tick arrays for accurate cross-tick quoting.
        /// Typically 3: left of current, current, right of current.
        tick_arrays: Vec<TickArray>,
        /// Max token_a input without crossing a tick boundary
        limit_in_a: u64,
        /// Max token_b input without crossing a tick boundary
        limit_in_b: u64,
    },

    /// Bonding curve (PumpFun)
    BondingCurve {
        virtual_token_reserves: u64,
        virtual_sol_reserves: u64,
        real_token_reserves: u64,
        real_sol_reserves: u64,
        complete: bool,
    },

    /// Concentrated liquidity (Meteora DAMM V2)
    /// Uniswap V3-style single-range CL with sqrt-price math.
    DammV2Concentrated {
        sqrt_price_x64: u128,
        sqrt_min_price_x64: u128,
        sqrt_max_price_x64: u128,
        liquidity: u128,
        fee_numerator: u64,
        /// 0=BothToken (fee on output), 2=Compounding (CP-like, uses token_a/b_amount)
        collect_fee_mode: u8,
    },

    /// Bin-based constant-sum (Meteora DLMM)
    MeteoraDlmm {
        active_id: i32,
        bin_step: u16,
        // Fee parameters
        base_factor: u16,
        variable_fee_control: u32,
        max_volatility_accumulator: u32,
        volatility_accumulator: u32,
        volatility_reference: u32,
        index_reference: i32,
        /// Pre-loaded bin arrays (typically 3: current-1, current, current+1)
        bin_arrays: Vec<DlmmBinArray>,
    },
}

impl PoolMath {
    /// Fast f64 quote for off-chain routing decisions
    pub fn get_amount_out(&self, amount_in: u64, is_a_to_b: bool) -> u64 {
        match self {
            PoolMath::ConstantProduct {
                reserve_a,
                reserve_b,
                fee_numerator,
                fee_denominator,
            } => {
                let (r_in, r_out) = if is_a_to_b {
                    (*reserve_a, *reserve_b)
                } else {
                    (*reserve_b, *reserve_a)
                };
                if r_in == 0 || r_out == 0 {
                    return 0;
                }
                // Match on-chain math exactly using u128:
                // 1. Fee: ceil_div(amount * fee_rate, fee_denom) — rounds fee UP (less input after fee)
                // 2. Swap: (amt_after_fee * r_out) / (r_in + amt_after_fee) — floor div (less output)
                let amount = amount_in as u128;
                let fee_num = *fee_numerator as u128;
                let fee_den = *fee_denominator as u128;
                // Ceiling division for fee: ⌈amount * fee_rate / fee_denom⌉
                let fee_amount = amount
                    .checked_mul(fee_num)
                    .and_then(|v| v.checked_add(fee_den))
                    .map(|v| v.saturating_sub(1) / fee_den)
                    .unwrap_or(0);
                let amt_after_fee = amount.saturating_sub(fee_amount);
                if amt_after_fee == 0 {
                    return 0;
                }
                let ri = r_in as u128;
                let ro = r_out as u128;
                let numerator = amt_after_fee * ro;
                let denominator = ri + amt_after_fee;
                let out = numerator / denominator;
                out as u64
            }
            PoolMath::BondingCurve {
                virtual_token_reserves,
                virtual_sol_reserves,
                real_token_reserves,
                real_sol_reserves,
                complete,
            } => {
                // Completed bonding curves are migrated to Raydium, no longer tradeable
                if *complete {
                    return 0;
                }
                let (r_in, r_out, real_cap) = if is_a_to_b {
                    // SOL → Token: capped by real_token_reserves
                    (*virtual_sol_reserves as f64, *virtual_token_reserves as f64, *real_token_reserves)
                } else {
                    // Token → SOL: capped by real_sol_reserves
                    (*virtual_token_reserves as f64, *virtual_sol_reserves as f64, *real_sol_reserves)
                };
                if r_in == 0.0 || r_out == 0.0 {
                    return 0;
                }
                let amt = amount_in as f64 * 0.99; // 1% fee
                let out = (r_out * amt) / (r_in + amt);
                // Cap output by real reserves (what's actually available)
                (out as u64).min(real_cap)
            }
            PoolMath::Concentrated {
                sqrt_price_x64,
                liquidity,
                tick_current,
                tick_spacing,
                fee_rate,
                tick_arrays,
                limit_in_a: _,
                limit_in_b: _,
            } => {
                if *liquidity == 0 || *sqrt_price_x64 == 0 {
                    return 0;
                }
                clmm_get_amount_out(
                    amount_in,
                    is_a_to_b,
                    *sqrt_price_x64,
                    *liquidity,
                    *tick_current,
                    *tick_spacing,
                    *fee_rate,
                    tick_arrays,
                )
            }
            PoolMath::DammV2Concentrated {
                sqrt_price_x64,
                sqrt_min_price_x64,
                sqrt_max_price_x64,
                liquidity,
                fee_numerator,
                collect_fee_mode,
            } => {
                if *liquidity == 0 || *sqrt_price_x64 == 0 {
                    return 0;
                }
                damm_v2_get_amount_out(
                    amount_in,
                    is_a_to_b,
                    *sqrt_price_x64,
                    *sqrt_min_price_x64,
                    *sqrt_max_price_x64,
                    *liquidity,
                    *fee_numerator,
                    *collect_fee_mode,
                )
            }
            PoolMath::MeteoraDlmm {
                active_id,
                bin_step,
                base_factor,
                variable_fee_control,
                max_volatility_accumulator,
                volatility_accumulator,
                volatility_reference,
                index_reference,
                bin_arrays,
            } => {
                if bin_arrays.is_empty() {
                    return 0;
                }
                dlmm_get_amount_out(
                    amount_in,
                    is_a_to_b,
                    *active_id,
                    *bin_step,
                    *base_factor,
                    *variable_fee_control,
                    *max_volatility_accumulator,
                    *volatility_accumulator,
                    *volatility_reference,
                    *index_reference,
                    bin_arrays,
                )
            }
        }
    }
}

/// CLMM tick traversal quote (f64 fast-path).
///
/// Walks through initialized ticks to compute amount_out for a given amount_in,
/// accounting for liquidity changes at each tick boundary.
fn clmm_get_amount_out(
    amount_in: u64,
    is_a_to_b: bool,
    sqrt_price_x64: u128,
    liquidity: u128,
    tick_current: i32,
    _tick_spacing: u16,
    fee_rate: u32,
    tick_arrays: &[TickArray],
) -> u64 {
    // fee_rate=0 means AmmConfig hasn't been fetched yet — quote is unreliable
    if fee_rate == 0 {
        return 0;
    }

    // No tick arrays loaded — can't bound the quote, would produce infinite-range garbage
    if tick_arrays.is_empty() {
        return 0;
    }

    let q64 = (1u128 << 64) as f64;
    let mut sqrt_price = sqrt_price_x64 as f64 / q64;
    let mut liq = liquidity as f64;
    // Match on-chain fee: floor division to get amount after fee
    // On-chain: fee = amount * fee_rate / 1_000_000 (floor), remaining = amount - fee
    let fee_amount = (amount_in as u128) * (fee_rate as u128) / 1_000_000u128;
    let mut remaining = (amount_in as u128).saturating_sub(fee_amount) as f64;
    let mut amount_out: f64 = 0.0;

    if remaining <= 0.0 || liq <= 0.0 {
        return 0;
    }

    // Collect all initialized ticks from tick arrays, sorted by index
    let mut initialized_ticks: Vec<&Tick> = tick_arrays
        .iter()
        .flat_map(|ta| ta.ticks.iter())
        .filter(|t| t.liquidity_net != 0)
        .collect();
    initialized_ticks.sort_by_key(|t| t.index);

    if is_a_to_b {
        // Price goes down: traverse ticks below current tick
        let relevant: Vec<&&Tick> = initialized_ticks
            .iter()
            .filter(|t| t.index <= tick_current)
            .rev()
            .collect();

        for tick in relevant {
            if remaining <= 0.0 {
                break;
            }
            let tick_sqrt_price = tick_index_to_sqrt_price(tick.index);
            if tick_sqrt_price >= sqrt_price || tick_sqrt_price <= 0.0 {
                continue;
            }

            // Amount of token_a needed to move price from sqrt_price to tick_sqrt_price
            // delta_a = L * (1/sqrt_price_lower - 1/sqrt_price_upper)
            let max_a = liq * (1.0 / tick_sqrt_price - 1.0 / sqrt_price);
            if max_a <= 0.0 {
                continue;
            }

            let consumed = remaining.min(max_a);
            // delta_b = L * (sqrt_price_upper - sqrt_price_lower)
            // but we need to compute based on actual consumed amount
            let new_sqrt_price = if consumed >= max_a {
                tick_sqrt_price
            } else {
                // new_sqrt = L * sqrt_price / (L + consumed * sqrt_price)
                liq * sqrt_price / (liq + consumed * sqrt_price)
            };
            let out = liq * (sqrt_price - new_sqrt_price);
            amount_out += out;
            remaining -= consumed;
            sqrt_price = new_sqrt_price;

            // Cross tick: update liquidity
            if consumed >= max_a {
                liq -= tick.liquidity_net as f64;
                if liq <= 0.0 {
                    break;
                }
            }
        }

        // No more loaded ticks — stop quoting. Continuing would assume
        // unbounded liquidity beyond loaded tick arrays, producing phantom
        // output that doesn't exist on-chain (causes Custom(1) loss errors).
    } else {
        // Price goes up: traverse ticks above current tick
        let relevant: Vec<&&Tick> = initialized_ticks
            .iter()
            .filter(|t| t.index > tick_current)
            .collect();

        for tick in relevant {
            if remaining <= 0.0 {
                break;
            }
            let tick_sqrt_price = tick_index_to_sqrt_price(tick.index);
            if tick_sqrt_price <= sqrt_price {
                continue;
            }

            // Amount of token_b needed to move price from sqrt_price to tick_sqrt_price
            // delta_b = L * (sqrt_price_upper - sqrt_price_lower)
            let max_b = liq * (tick_sqrt_price - sqrt_price);
            if max_b <= 0.0 {
                continue;
            }

            let consumed = remaining.min(max_b);
            // delta_a = L * (1/sqrt_price_lower - 1/sqrt_price_upper)
            let new_sqrt_price = if consumed >= max_b {
                tick_sqrt_price
            } else {
                sqrt_price + consumed / liq
            };
            let out = liq * (1.0 / sqrt_price - 1.0 / new_sqrt_price);
            amount_out += out;
            remaining -= consumed;
            sqrt_price = new_sqrt_price;

            // Cross tick: update liquidity
            if consumed >= max_b {
                liq += tick.liquidity_net as f64;
                if liq <= 0.0 {
                    break;
                }
            }
        }

        // No more loaded ticks — stop quoting (same as a_to_b branch).
    }

    // Apply conservative haircut to account for f64 precision loss on u128 values.
    // On-chain uses fixed-point u128 math; our f64 has only 53-bit mantissa,
    // causing ~0.1-0.3% divergence on large amounts. 0.3% haircut prevents
    // over-quoting that leads to simulate failures.
    let out = amount_out * 0.997;
    out.max(0.0) as u64
}

/// Meteora DAMM V2 concentrated liquidity quote (f64 fast-path).
///
/// Single-range CL with sqrt-price math (Uniswap V3 style, one position).
/// Formulas (real f64 values, sp = sqrt_price / 2^64, L = liquidity raw):
///   A→B: next_sp = L*sp / (L + amt*sp), output = L*(sp - next_sp)
///   B→A: next_sp = sp + amt/L, output = L*(1/sp - 1/next_sp)
/// Fee: applied on output for collect_fee_mode=0 (BothToken).
/// DammV2 concentrated liquidity quote using U256 integer math.
///
/// Matches on-chain formulas from MeteoraAg/damm-v2 exactly:
///   A→B: next_sp = L*sp / (L + amt*sp)  (round UP → less output)
///        output (delta_b) = (L * (sp - next_sp)) >> 128  (round DOWN)
///   B→A: next_sp = sp + (amt << 128) / L  (round DOWN)
///        output (delta_a) = L * (next_sp - sp) / (sp * next_sp)  (round DOWN)
///
/// sqrt_price is Q64.64, liquidity is raw u128. The >> 128 shift in delta_b
/// comes from L_raw × delta_sp(Q64.64) needing division by 2^64 to get
/// integer output, combined with the Q64.64 multiplication structure.
#[allow(clippy::too_many_arguments)]
fn damm_v2_get_amount_out(
    amount_in: u64,
    is_a_to_b: bool,
    sqrt_price_x64: u128,
    sqrt_min_price_x64: u128,
    sqrt_max_price_x64: u128,
    liquidity: u128,
    fee_numerator: u64,
    collect_fee_mode: u8,
) -> u64 {
    use ruint::aliases::U256;

    if sqrt_price_x64 == 0 || liquidity == 0 {
        return 0;
    }

    // Fee: deduct from input for compounding mode (mode=2), from output otherwise
    let fee_den = 1_000_000_000u128;
    let fee_num = fee_numerator as u128;
    let amt: u64 = if collect_fee_mode == 2 {
        let fee = (amount_in as u128) * fee_num / fee_den;
        (amount_in as u128).saturating_sub(fee) as u64
    } else {
        amount_in
    };

    if amt == 0 {
        return 0;
    }

    let sp = U256::from(sqrt_price_x64);
    let l = U256::from(liquidity);

    let output = if is_a_to_b {
        // A→B: price decreases
        // next_sp = L * sp / (L + amt * sp)  — round UP (conservative)
        let product = U256::from(amt as u128) * sp;
        let denominator = l + product;
        if denominator == U256::ZERO {
            return 0;
        }
        let numerator = l * sp;
        // Ceiling division: (num + den - 1) / den
        let next_sp_u256 = (numerator + denominator - U256::from(1u64)) / denominator;
        let next_sp = u128::try_from(next_sp_u256).unwrap_or(0);

        // Boundary check
        if next_sp <= sqrt_min_price_x64 {
            return 0; // saturated
        }

        // delta_b = (L * (sp - next_sp)) >> 128  — round DOWN
        let delta_sp = sqrt_price_x64.saturating_sub(next_sp);
        let prod = U256::from(liquidity) * U256::from(delta_sp);
        let out = prod >> 128;
        u64::try_from(out).unwrap_or(0)
    } else {
        // B→A: price increases
        // next_sp = sp + (amt << 128) / L  — round DOWN
        let shifted = U256::from(amt as u128) << 128;
        let quotient = shifted / l;
        let next_sp_u256 = sp + quotient;
        let next_sp = u128::try_from(next_sp_u256).unwrap_or(u128::MAX);

        // Boundary check
        if next_sp >= sqrt_max_price_x64 {
            return 0; // saturated
        }

        // delta_a = L * (next_sp - sp) / (sp * next_sp)  — round DOWN
        let delta = next_sp.saturating_sub(sqrt_price_x64);
        let num = U256::from(liquidity) * U256::from(delta);
        let den = U256::from(sqrt_price_x64) * U256::from(next_sp);
        if den == U256::ZERO {
            return 0;
        }
        let out = num / den;
        u64::try_from(out).unwrap_or(0)
    };

    // Fee on output for BothToken mode (mode=0)
    let output = if collect_fee_mode != 2 && output > 0 {
        let fee = (output as u128) * fee_num / fee_den;
        output.saturating_sub(fee as u64)
    } else {
        output
    };

    // No haircut needed — U256 math matches on-chain exactly.
    // Output is always <= on-chain due to conservative rounding (UP for next_sp, DOWN for output).
    output
}

/// DLMM bin-level constant-sum quote (f64 fast-path).
///
/// Walks through bins from active_id in the swap direction, consuming
/// liquidity from each bin using constant-sum math within each bin.
/// Price per bin = (1 + bin_step/10000)^bin_id.
#[allow(clippy::too_many_arguments)]
fn dlmm_get_amount_out(
    amount_in: u64,
    is_a_to_b: bool,
    active_id: i32,
    bin_step: u16,
    base_factor: u16,
    variable_fee_control: u32,
    max_volatility_accumulator: u32,
    _volatility_accumulator: u32,
    volatility_reference: u32,
    index_reference: i32,
    bin_arrays: &[DlmmBinArray],
) -> u64 {
    use ruint::aliases::U256;

    if amount_in == 0 || bin_step == 0 {
        return 0;
    }

    const FEE_PRECISION: u128 = 1_000_000_000;
    const BASIS_POINT_MAX: u64 = 10_000;

    // Collect bins sorted by bin_id
    let mut all_bins: Vec<&DlmmBin> = bin_arrays
        .iter()
        .flat_map(|ba| ba.bins.iter())
        .collect();
    all_bins.sort_by_key(|b| b.bin_id);

    let swap_for_y = is_a_to_b;
    let mut remaining: u64 = amount_in;
    let mut total_out: u64 = 0;

    // Track current active_id (moves as we consume bins)
    let mut current_active_id = active_id;
    // Simulate update_references: for most pools, enough time has passed
    // since last trade that volatility_reference decays to 0 and
    // index_reference resets to active_id. This means delta_id starts at 0
    // and variable fee only grows as we cross bins away from start.
    let effective_vol_ref: u64 = 0;
    let effective_idx_ref: i32 = active_id;

    let relevant: Vec<&&DlmmBin> = if swap_for_y {
        all_bins.iter()
            .filter(|b| b.bin_id <= active_id && b.amount_y > 0)
            .rev().collect()
    } else {
        all_bins.iter()
            .filter(|b| b.bin_id >= active_id && b.amount_x > 0)
            .collect()
    };

    for bin in relevant {
        if remaining == 0 { break; }

        // Update volatility_accumulator per bin crossing (on-chain does this)
        let delta_id = (effective_idx_ref as i64 - current_active_id as i64).unsigned_abs();
        let vol_acc = (effective_vol_ref + delta_id * BASIS_POINT_MAX)
            .min(max_volatility_accumulator as u64);

        // Compute fee rate with updated volatility
        let base_fee = base_factor as u128 * bin_step as u128 * 10;
        let v_fee = if variable_fee_control > 0 {
            let va_bin = vol_acc as u128 * bin_step as u128;
            let sq = va_bin * va_bin;
            (variable_fee_control as u128 * sq + 99_999_999_999) / 100_000_000_000
        } else { 0 };
        let fee_rate = (base_fee + v_fee).min(100_000_000); // MAX_FEE_RATE

        let price = dlmm_price_q64(bin.bin_id, bin_step);
        if price == 0 { continue; }

        if swap_for_y {
            let amount_y = bin.amount_y as u64;

            // [Fix 2] Use U256 for ceiling division: ceil(amount_y << 64 / price)
            let max_in_net = {
                let num = U256::from(amount_y as u128) << 64;
                let den = U256::from(price);
                let q: U256 = (num + den - U256::from(1u64)) / den;
                q.try_into().unwrap_or(u64::MAX)
            };

            // compute_fee: fee = ceil(max_in_net * fee_rate / (FEE_PRECISION - fee_rate))
            let denom = FEE_PRECISION.saturating_sub(fee_rate);
            let max_fee: u64 = if denom > 0 {
                let f = (max_in_net as u128 * fee_rate + denom - 1) / denom;
                f.min(u64::MAX as u128) as u64
            } else { 0 };
            let max_in_gross = max_in_net.saturating_add(max_fee);

            if remaining >= max_in_gross {
                total_out = total_out.saturating_add(amount_y);
                remaining -= max_in_gross;
            } else {
                // compute_fee_from_amount: fee = ceil(remaining * fee_rate / FEE_PRECISION)
                let fee = ((remaining as u128 * fee_rate + FEE_PRECISION - 1) / FEE_PRECISION) as u64;
                let after_fee = remaining.saturating_sub(fee);
                // [Fix 2] Use U256: out = floor(price * after_fee >> 64)
                let out = {
                    let prod = U256::from(price) * U256::from(after_fee as u128);
                    let q: U256 = prod >> 64;
                    let v: u64 = q.try_into().unwrap_or(u64::MAX);
                    // [Fix 3] Clamp to max_amount_out
                    v.min(amount_y)
                };
                total_out = total_out.saturating_add(out);
                remaining = 0;
            }
        } else {
            let amount_x = bin.amount_x as u64;

            // [Fix 2] Use U256: max_in_net = ceil(amount_x * price >> 64)
            let max_in_net = {
                let prod = U256::from(amount_x as u128) * U256::from(price);
                let shift = U256::from(1u128) << 64;
                let q: U256 = (prod + shift - U256::from(1u64)) / shift;
                q.try_into().unwrap_or(u64::MAX)
            };

            let denom = FEE_PRECISION.saturating_sub(fee_rate);
            let max_fee: u64 = if denom > 0 {
                let f = (max_in_net as u128 * fee_rate + denom - 1) / denom;
                f.min(u64::MAX as u128) as u64
            } else { 0 };
            let max_in_gross = max_in_net.saturating_add(max_fee);

            if remaining >= max_in_gross {
                total_out = total_out.saturating_add(amount_x);
                remaining -= max_in_gross;
            } else {
                let fee = ((remaining as u128 * fee_rate + FEE_PRECISION - 1) / FEE_PRECISION) as u64;
                let after_fee = remaining.saturating_sub(fee);
                // [Fix 2] Use U256: out = floor(after_fee << 64 / price)
                let out = {
                    let num = U256::from(after_fee as u128) << 64;
                    let q: U256 = num / U256::from(price);
                    let v: u64 = q.try_into().unwrap_or(u64::MAX);
                    v.min(amount_x)
                };
                total_out = total_out.saturating_add(out);
                remaining = 0;
            }
        }

        // Advance active_id for next bin
        current_active_id = if swap_for_y { bin.bin_id - 1 } else { bin.bin_id + 1 };
    }

    total_out
}

/// Compute Q64.64 price: (1 + bin_step/10000) ^ bin_id
/// Uses binary exponentiation with truncation matching on-chain pow()
fn dlmm_price_q64(bin_id: i32, bin_step: u16) -> u128 {
    let one: u128 = 1u128 << 64;
    let bps = ((bin_step as u128) << 64) / 10_000;
    let base = one + bps;

    if bin_id == 0 { return one; }

    let (exp, invert) = if bin_id > 0 {
        (bin_id as u32, false)
    } else {
        ((-bin_id) as u32, true)
    };

    // Binary exponentiation in Q64.64
    let mut result: u128 = one;
    let mut b = base;
    let mut e = exp;
    while e > 0 {
        if e & 1 == 1 {
            result = mul_shr64(result, b);
        }
        b = mul_shr64(b, b);
        e >>= 1;
    }

    if invert {
        if result == 0 { return 0; }
        // 1/result in Q64.64 = (1 << 128) / result
        // Use u128 division: split (1 << 128) / result into parts
        // (1 << 128) = (u128::MAX + 1), which doesn't fit u128
        // Instead: result_inv = u128::MAX / result (approximate, off by at most 1)
        u128::MAX / result
    } else {
        result
    }
}

/// Multiply two Q64.64 numbers: (a * b) >> 64, with truncation
#[inline]
fn mul_shr64(a: u128, b: u128) -> u128 {
    // Use u128 multiplication, take upper 128 bits by shifting
    // a * b could overflow u128, so split into parts
    let a_hi = a >> 64;
    let a_lo = a & ((1u128 << 64) - 1);
    let b_hi = b >> 64;
    let b_lo = b & ((1u128 << 64) - 1);

    // a * b = (a_hi * b_hi) << 128 + (a_hi * b_lo + a_lo * b_hi) << 64 + a_lo * b_lo
    // We want (a * b) >> 64:
    let mid = a_hi * b_lo + a_lo * b_hi;
    let lo = a_lo * b_lo;

    a_hi * b_hi * (1u128 << 64) + mid + (lo >> 64)
}

/// Dynamic fee rate in 1e-9 integer units (matching on-chain)
/// Convert a tick index to sqrt_price (f64).
/// sqrt_price = 1.0001^(tick/2)
#[inline]
fn tick_index_to_sqrt_price(tick: i32) -> f64 {
    1.0001_f64.powf(tick as f64 / 2.0)
}

/// Compute the max input amounts that stay within the current tick range.
/// Uses the nearest initialized ticks above and below tick_current.
/// The limits account for fees — they represent the gross input (before fee
/// deduction) that would consume all liquidity to the next tick boundary.
pub fn compute_clmm_limits(
    sqrt_price_x64: u128,
    liquidity: u128,
    tick_current: i32,
    tick_arrays: &[TickArray],
) -> (u64, u64) {
    // Pass fee_rate=0 for backwards compatibility — use compute_clmm_limits_with_fee instead
    compute_clmm_limits_with_fee(sqrt_price_x64, liquidity, tick_current, tick_arrays, 0)
}

/// Compute the max input amounts with fee adjustment.
/// fee_rate is in 1e-6 units (e.g. 2500 = 0.25%).
/// Returns gross input amounts (before fee) that would move price to the next tick.
pub fn compute_clmm_limits_with_fee(
    sqrt_price_x64: u128,
    liquidity: u128,
    tick_current: i32,
    tick_arrays: &[TickArray],
    fee_rate: u32,
) -> (u64, u64) {
    if liquidity == 0 || sqrt_price_x64 == 0 || tick_arrays.is_empty() {
        return (0, 0);
    }

    let q64 = (1u128 << 64) as f64;
    let sqrt_price = sqrt_price_x64 as f64 / q64;
    let liq = liquidity as f64;

    // Collect all initialized ticks
    let mut initialized: Vec<i32> = tick_arrays
        .iter()
        .flat_map(|ta| ta.ticks.iter())
        .filter(|t| t.liquidity_net != 0)
        .map(|t| t.index)
        .collect();
    initialized.sort();

    // Find nearest initialized tick below (or equal to) tick_current
    let lower_tick = initialized.iter().rev()
        .find(|&&t| t <= tick_current)
        .copied();
    // Find nearest initialized tick above tick_current
    let upper_tick = initialized.iter()
        .find(|&&t| t > tick_current)
        .copied();

    let lower_sqrt = lower_tick
        .map(|t| tick_index_to_sqrt_price(t))
        .unwrap_or(tick_index_to_sqrt_price(-443636));
    let upper_sqrt = upper_tick
        .map(|t| tick_index_to_sqrt_price(t))
        .unwrap_or(tick_index_to_sqrt_price(443636));

    // Net amount (after fee) that fills the tick range
    // limit_in_a (net): max token_a to push price down to lower_sqrt
    // delta_a = L * (1/lower_sqrt - 1/sqrt_price)
    let net_a = if lower_sqrt > 0.0 && lower_sqrt < sqrt_price {
        liq * (1.0 / lower_sqrt - 1.0 / sqrt_price)
    } else {
        0.0
    };

    // limit_in_b (net): max token_b to push price up to upper_sqrt
    // delta_b = L * (upper_sqrt - sqrt_price)
    let net_b = if upper_sqrt > sqrt_price {
        liq * (upper_sqrt - sqrt_price)
    } else {
        0.0
    };

    // Convert net → gross: gross = net / (1 - fee_rate/1e6)
    // This is the actual input the user needs to provide (before fee deduction).
    let fee_factor = if fee_rate > 0 {
        1.0 - (fee_rate as f64 / 1_000_000.0)
    } else {
        1.0
    };

    let limit_a = if fee_factor > 0.0 { net_a / fee_factor } else { net_a };
    let limit_b = if fee_factor > 0.0 { net_b / fee_factor } else { net_b };

    (limit_a.max(0.0) as u64, limit_b.max(0.0) as u64)
}

/// Unified pool state
#[derive(Debug, Clone)]
pub struct PoolState {
    pub address: Pubkey,
    pub dex_type: DexType,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Option<Pubkey>,
    pub vault_b: Option<Pubkey>,
    pub mint_a_is_2022: bool,
    pub mint_b_is_2022: bool,
    /// DEX-specific accounts needed for CPI but not vaults/mints.
    /// Ordered per DEX to match swap.rs account expectations.
    /// See docs/cpi-accounts.md for per-DEX ordering.
    pub extra_accounts: Vec<Pubkey>,
    pub math: PoolMath,
    pub last_updated_slot: u64,
}

/// Sent to downstream channel on every pool state change
#[derive(Debug, Clone)]
pub struct PoolUpdate {
    pub pool_address: Pubkey,
    pub dex_type: DexType,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Option<Pubkey>,
    pub vault_b: Option<Pubkey>,
    pub mint_a_is_2022: bool,
    pub mint_b_is_2022: bool,
    pub extra_accounts: Vec<Pubkey>,
    pub math: PoolMath,
    pub slot: u64,
}
