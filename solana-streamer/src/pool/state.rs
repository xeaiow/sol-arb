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
    },

    /// Bonding curve (PumpFun)
    BondingCurve {
        virtual_token_reserves: u64,
        virtual_sol_reserves: u64,
        real_token_reserves: u64,
        real_sol_reserves: u64,
        complete: bool,
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

    amount_out.max(0.0) as u64
}

/// Convert a tick index to sqrt_price (f64).
/// sqrt_price = 1.0001^(tick/2)
#[inline]
fn tick_index_to_sqrt_price(tick: i32) -> f64 {
    1.0001_f64.powf(tick as f64 / 2.0)
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
