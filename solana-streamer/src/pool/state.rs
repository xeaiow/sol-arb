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
    /// x * y = k (Raydium AMM V4/CPMM, PumpSwap, Bonk, Meteora DAMM v2)
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
                    (*reserve_a as f64, *reserve_b as f64)
                } else {
                    (*reserve_b as f64, *reserve_a as f64)
                };
                if r_in == 0.0 || r_out == 0.0 {
                    return 0;
                }
                let fee = *fee_numerator as f64 / *fee_denominator as f64;
                let amt = amount_in as f64 * (1.0 - fee);
                let out = (r_out * amt) / (r_in + amt);
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

    let q64 = (1u128 << 64) as f64;
    let mut sqrt_price = sqrt_price_x64 as f64 / q64;
    let mut liq = liquidity as f64;
    let fee_pct = fee_rate as f64 / 1_000_000.0;
    let mut remaining = amount_in as f64 * (1.0 - fee_pct);
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

        // Remaining in current range (no more ticks)
        if remaining > 0.0 && liq > 0.0 {
            let new_sqrt_price = liq * sqrt_price / (liq + remaining * sqrt_price);
            if new_sqrt_price > 0.0 {
                amount_out += liq * (sqrt_price - new_sqrt_price);
            }
        }
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

        // Remaining in current range
        if remaining > 0.0 && liq > 0.0 {
            let new_sqrt_price = sqrt_price + remaining / liq;
            if new_sqrt_price > 0.0 && sqrt_price > 0.0 {
                amount_out += liq * (1.0 / sqrt_price - 1.0 / new_sqrt_price);
            }
        }
    }

    // Apply conservative haircut to account for f64 precision loss on u128 values.
    // On-chain uses fixed-point u128 math; our f64 has only 53-bit mantissa,
    // causing ~0.1-0.3% divergence on large amounts. 0.3% haircut prevents
    // over-quoting that leads to simulate failures.
    let out = amount_out * 0.997;
    out.max(0.0) as u64
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
    volatility_accumulator: u32,
    volatility_reference: u32,
    index_reference: i32,
    bin_arrays: &[DlmmBinArray],
) -> u64 {
    if amount_in == 0 || bin_step == 0 {
        return 0;
    }

    // Compute base fee rate: base_factor * bin_step * 10 / 1e9
    let base_fee = base_factor as f64 * bin_step as f64 * 10.0 / 1_000_000_000.0;

    // Compute variable fee rate
    let var_fee = if variable_fee_control > 0 {
        let va_bin = volatility_accumulator as f64 * bin_step as f64;
        let v = variable_fee_control as f64 * va_bin * va_bin / 100_000_000_000.0;
        v.min(0.1) // cap at 10%
    } else {
        0.0
    };

    let _total_fee_rate = (base_fee + var_fee).min(0.1); // cap total at 10%

    // Collect all bins from bin_arrays, sorted by bin_id
    let mut all_bins: Vec<&DlmmBin> = bin_arrays
        .iter()
        .flat_map(|ba| ba.bins.iter())
        .collect();
    all_bins.sort_by_key(|b| b.bin_id);

    log::trace!(
        "[DLMM_QUOTE] active_id={} bin_step={} a_to_b={} amount_in={} arrays={} total_bins={} base_fee={:.6} var_fee={:.6}",
        active_id, bin_step, is_a_to_b, amount_in, bin_arrays.len(), all_bins.len(), base_fee, var_fee,
    );

    // is_a_to_b means X→Y (swap_for_y = true): traverse bins downward from active_id
    // !is_a_to_b means Y→X (swap_for_y = false): traverse bins upward from active_id
    let swap_for_y = is_a_to_b;

    let mut remaining = amount_in as f64;
    let mut total_out: f64 = 0.0;
    let mut bins_consumed = 0u32;

    let step_bps = bin_step as f64 / 10000.0;

    if swap_for_y {
        // X→Y: price decreases, traverse from active_id downward
        // Bins with Y liquidity can accept X input
        let relevant: Vec<&&DlmmBin> = all_bins
            .iter()
            .filter(|b| b.bin_id <= active_id && b.amount_y > 0)
            .rev() // highest bin_id first (start from active)
            .collect();

        for bin in relevant {
            if remaining <= 0.0 {
                break;
            }

            let price = (1.0 + step_bps).powi(bin.bin_id);
            if price <= 0.0 {
                continue;
            }

            // Dynamic fee for this bin
            let fee_rate = dlmm_dynamic_fee_rate(
                bin.bin_id, base_fee, variable_fee_control,
                max_volatility_accumulator, volatility_reference,
                index_reference, bin_step,
            );

            // Max X that can consume all Y in this bin: max_x = amount_y / price
            let max_x_net = bin.amount_y as f64 / price;
            let max_x_gross = max_x_net / (1.0 - fee_rate);

            let consumed = remaining.min(max_x_gross);
            let consumed_after_fee = consumed * (1.0 - fee_rate);

            let out = if consumed >= max_x_gross {
                bin.amount_y as f64
            } else {
                consumed_after_fee * price
            };

            total_out += out;
            remaining -= consumed;
            bins_consumed += 1;
        }
    } else {
        // Y→X: price increases, traverse from active_id upward
        // Bins with X liquidity can accept Y input
        let relevant: Vec<&&DlmmBin> = all_bins
            .iter()
            .filter(|b| b.bin_id >= active_id && b.amount_x > 0)
            .collect(); // lowest bin_id first (start from active)

        for bin in relevant {
            if remaining <= 0.0 {
                break;
            }

            let price = (1.0 + step_bps).powi(bin.bin_id);
            if price <= 0.0 {
                continue;
            }

            let fee_rate = dlmm_dynamic_fee_rate(
                bin.bin_id, base_fee, variable_fee_control,
                max_volatility_accumulator, volatility_reference,
                index_reference, bin_step,
            );

            // Max Y that can consume all X in this bin: max_y = amount_x * price
            let max_y_net = bin.amount_x as f64 * price;
            let max_y_gross = max_y_net / (1.0 - fee_rate);

            let consumed = remaining.min(max_y_gross);
            let consumed_after_fee = consumed * (1.0 - fee_rate);

            let out = if consumed >= max_y_gross {
                bin.amount_x as f64
            } else {
                consumed_after_fee / price
            };

            total_out += out;
            remaining -= consumed;
            bins_consumed += 1;
        }
    }

    // Apply 0.5% haircut for f64 precision loss (similar to CLMM's 0.3%)
    let out = total_out * 0.995;
    let result = out.max(0.0) as u64;

    log::trace!(
        "[DLMM_QUOTE] result: in={} out={} bins_consumed={} remaining={:.0} haircut_out={}",
        amount_in, result, bins_consumed, remaining, result,
    );

    result
}

/// Compute dynamic fee rate for a specific bin_id.
/// Returns fee as a fraction (e.g. 0.001 = 0.1%).
#[inline]
fn dlmm_dynamic_fee_rate(
    bin_id: i32,
    base_fee: f64,
    variable_fee_control: u32,
    max_volatility_accumulator: u32,
    volatility_reference: u32,
    index_reference: i32,
    bin_step: u16,
) -> f64 {
    if variable_fee_control == 0 {
        return base_fee;
    }

    let delta_id = (index_reference as i64 - bin_id as i64).unsigned_abs();
    let va = (volatility_reference as u64 + delta_id * 10_000)
        .min(max_volatility_accumulator as u64);

    let va_bin = va as f64 * bin_step as f64;
    let v_fee = variable_fee_control as f64 * va_bin * va_bin / 100_000_000_000.0;

    (base_fee + v_fee).min(0.1)
}

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
