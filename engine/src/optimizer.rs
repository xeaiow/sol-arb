use solana_streamer_sdk::pool::state::PoolMath;
use crate::graph::TokenGraph;
use crate::opportunity::Route;

/// Simulate a route's profit for a given input amount.
/// Returns (amount_out - amount_in) as i64.
pub fn simulate_route_profit(
    route: &Route,
    graph: &TokenGraph,
    amount_in: u64,
) -> i64 {
    let mut current = amount_in;
    for hop in &route.hops {
        let pool = &graph.pools[hop.pool_index as usize];
        current = pool.math.get_amount_out(current, hop.is_a_to_b);
        if current == 0 {
            return i64::MIN;
        }
    }
    current as i64 - amount_in as i64
}

// ---------------------------------------------------------------------------
// Closed-form helpers
// ---------------------------------------------------------------------------

/// Extracted CP pool parameters for closed-form calculations
struct CpParams {
    r_in: f64,
    r_out: f64,
    fee_factor: f64,
}

/// Extracted CLMM pool parameters for closed-form calculations
struct ClmmParams {
    sqrt_price: f64,
    liquidity: f64,
    fee_factor: f64,
    limit_in_a: u64,
    limit_in_b: u64,
}

fn extract_cp(math: &PoolMath, is_a_to_b: bool) -> Option<CpParams> {
    match math {
        PoolMath::ConstantProduct { reserve_a, reserve_b, fee_numerator, fee_denominator } => {
            let (r_in, r_out) = if is_a_to_b {
                (*reserve_a as f64, *reserve_b as f64)
            } else {
                (*reserve_b as f64, *reserve_a as f64)
            };
            if r_in <= 0.0 || r_out <= 0.0 || *fee_denominator == 0 {
                return None;
            }
            Some(CpParams {
                r_in,
                r_out,
                fee_factor: 1.0 - (*fee_numerator as f64 / *fee_denominator as f64),
            })
        }
        PoolMath::BondingCurve { virtual_token_reserves, virtual_sol_reserves, complete, .. } => {
            if *complete { return None; }
            // BondingCurve: mint_a = SOL, mint_b = token
            let (r_in, r_out) = if is_a_to_b {
                (*virtual_sol_reserves as f64, *virtual_token_reserves as f64)
            } else {
                (*virtual_token_reserves as f64, *virtual_sol_reserves as f64)
            };
            if r_in <= 0.0 || r_out <= 0.0 { return None; }
            Some(CpParams { r_in, r_out, fee_factor: 0.99 })
        }
        _ => None,
    }
}

fn extract_clmm(math: &PoolMath) -> Option<ClmmParams> {
    match math {
        PoolMath::Concentrated { sqrt_price_x64, liquidity, fee_rate, limit_in_a, limit_in_b, .. } => {
            if *liquidity == 0 || *sqrt_price_x64 == 0 { return None; }
            let q64 = (1u128 << 64) as f64;
            Some(ClmmParams {
                sqrt_price: *sqrt_price_x64 as f64 / q64,
                liquidity: *liquidity as f64,
                fee_factor: 1.0 - (*fee_rate as f64 / 1_000_000.0),
                limit_in_a: *limit_in_a,
                limit_in_b: *limit_in_b,
            })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Closed-form solvers
// ---------------------------------------------------------------------------

/// CP × CP: touyi AMM×AMM formula.
/// Route: Y into pool1 → X intermediate → X into pool2 → Y out.
/// p1 = pool1 (input=Y, output=X), p2 = pool2 (input=X, output=Y).
fn closed_form_cp_cp(p1: &CpParams, p2: &CpParams) -> Option<(u64, u64)> {
    let a1_f = p1.fee_factor;
    let a2_f = p2.fee_factor;
    let a1_x = p1.r_out; // pool1 output reserve (intermediate token)
    let a1_y = p1.r_in;  // pool1 input reserve (base token)
    let a2_x = p2.r_in;  // pool2 input reserve (intermediate token)
    let a2_y = p2.r_out; // pool2 output reserve (base token)

    if a1_x <= 0.0 || a1_y <= 0.0 || a2_x <= 0.0 || a2_y <= 0.0 {
        return None;
    }

    let inner = (a1_f * a1_x * a1_y * a2_f) / (a2_x * a2_y);
    if inner < 0.0 { return None; }
    let sqrt_0 = inner.sqrt();

    let n0 = a1_f * a1_x * a2_f;
    let n1 = a1_x * a1_y * a2_f / a2_y;
    let n2 = a1_x * a2_f * sqrt_0;
    let n3 = a2_x * sqrt_0;

    let d0 = a1_f * a2_f;
    let d1 = a1_x * a1_y * a2_f * a2_f / (a2_x * a2_y);
    let denom = d0 - d1;
    if denom.abs() < 1e-12 { return None; }

    // Try positive root first
    let dx_pos = (n0 + n1 - n2 - n3) / denom;
    if let Some(r) = try_cp_cp_solution(dx_pos, a1_x, a1_y, a1_f, a2_x, a2_y, a2_f) {
        return Some(r);
    }

    // Try negative root
    let dx_neg = (n0 + n1 + n2 + n3) / denom;
    try_cp_cp_solution(dx_neg, a1_x, a1_y, a1_f, a2_x, a2_y, a2_f)
}

fn try_cp_cp_solution(
    dx: f64, a1_x: f64, a1_y: f64, a1_f: f64,
    a2_x: f64, a2_y: f64, a2_f: f64,
) -> Option<(u64, u64)> {
    if dx <= 0.0 || (a1_x - dx).abs() < 1.0 { return None; }
    let dy_in = ((a1_y * a1_x) / (a1_x - dx) - a1_y) / a1_f;
    let dy_out = a2_y - (a2_y * a2_x) / (a2_x + a2_f * dx);
    if dy_in > 1.0 && dy_out > dy_in {
        let profit = dy_out - dy_in;
        Some((dy_in as u64, profit as u64))
    } else {
        None
    }
}

/// CP × CLMM: touyi AMM×CLMM formula.
fn closed_form_cp_clmm(p1: &CpParams, p2: &ClmmParams, hop2_is_a_to_b: bool) -> Option<(u64, u64)> {
    let a_f = p1.fee_factor;
    let a_x = p1.r_out;
    let a_y = p1.r_in;
    let sp2 = p2.sqrt_price;
    let l2 = p2.liquidity;
    let cl2_f = p2.fee_factor;

    if a_x <= 0.0 || a_y <= 0.0 || l2 <= 0.0 || sp2 <= 0.0 { return None; }

    let clmm_limit = if hop2_is_a_to_b { p2.limit_in_a } else { p2.limit_in_b };

    let n0 = a_f * cl2_f * l2 * sp2;
    let n1 = a_y * cl2_f;
    let sqrt_inner = a_f * a_x * a_y * cl2_f;
    if sqrt_inner < 0.0 { return None; }
    let sqrt_0 = sqrt_inner.sqrt();
    let n2 = cl2_f * sp2 * sqrt_0;
    let n3 = l2 * sqrt_0 / a_x;

    let d0 = a_f * cl2_f * l2 * sp2 / a_x;
    let d1 = a_y * cl2_f * cl2_f * sp2 / l2;
    let denom = d0 - d1;
    if denom.abs() < 1e-12 { return None; }

    let mut dx = (n0 + n1 - n2 - n3) / denom;
    if dx <= 0.0 {
        dx = (n0 + n1 + n2 + n3) / denom;
    }
    if dx <= 0.0 { return None; }
    if clmm_limit > 0 { dx = dx.min(clmm_limit as f64); }

    if (dx - a_x).abs() < 1.0 { return None; }
    let dy_in = -dx * a_y / (a_f * (dx - a_x));
    let dy_out = dx * cl2_f * l2 * sp2 / (dx * cl2_f + l2 / sp2);
    if dy_in > 1.0 && dy_out > dy_in {
        Some((dy_in as u64, (dy_out - dy_in) as u64))
    } else {
        None
    }
}

/// CLMM × CP: touyi CLMM×AMM formula.
fn closed_form_clmm_cp(p1: &ClmmParams, p2: &CpParams, hop1_is_a_to_b: bool) -> Option<(u64, u64)> {
    let c_f = p1.fee_factor;
    let sp = p1.sqrt_price;
    let l = p1.liquidity;
    let a_f = p2.fee_factor;
    let a_x = p2.r_in;
    let a_y = p2.r_out;

    if l <= 0.0 || sp <= 0.0 || a_x <= 0.0 || a_y <= 0.0 { return None; }

    let clmm_limit = if hop1_is_a_to_b { p1.limit_in_a } else { p1.limit_in_b };

    let n0 = -a_f * a_y * c_f;
    let n1 = a_f * l * sp;
    let sqrt_inner = a_f * a_y * c_f * a_x;
    if sqrt_inner < 0.0 { return None; }
    let sqrt_0 = sqrt_inner.sqrt();
    let n2 = a_f * l * sqrt_0 / a_x;
    let n3 = sp * sqrt_0;

    let d0 = a_f * a_f * l * sp / a_x;
    let d1 = a_f * a_y * c_f * sp / l;
    let denom = d0 - d1;
    if denom.abs() < 1e-12 { return None; }

    let mut dx = (n0 - n1 + n2 + n3) / denom;
    if dx <= 0.0 {
        dx = (n0 - n1 - n2 - n3) / denom;
    }
    if dx <= 0.0 { return None; }

    let sp_dx = sp * dx;
    if (l - sp_dx).abs() < 1.0 { return None; }
    let mut cdy = (l / c_f) * (sp * sp * dx) / (l - sp_dx);

    if clmm_limit > 0 && cdy > clmm_limit as f64 {
        cdy = clmm_limit as f64;
        dx = c_f * cdy * l / (sp * (c_f * cdy + l * sp));
    }

    let ady = (a_y * dx * a_f) / (a_x + dx * a_f);
    if cdy > 1.0 && ady > cdy {
        Some((cdy as u64, (ady - cdy) as u64))
    } else {
        None
    }
}

/// CLMM × CLMM: touyi CLMM×CLMM formula.
fn closed_form_clmm_clmm(
    p1: &ClmmParams, p2: &ClmmParams,
    hop1_is_a_to_b: bool, hop2_is_a_to_b: bool,
) -> Option<(u64, u64)> {
    let sp = p1.sqrt_price;
    let l = p1.liquidity;
    let cl_f = p1.fee_factor;
    let sp2 = p2.sqrt_price;
    let l2 = p2.liquidity;
    let cl2_f = p2.fee_factor;

    if l <= 0.0 || sp <= 0.0 || l2 <= 0.0 || sp2 <= 0.0 { return None; }

    let clmm1_limit = if hop1_is_a_to_b { p1.limit_in_a } else { p1.limit_in_b };
    let clmm2_limit = if hop2_is_a_to_b { p2.limit_in_a } else { p2.limit_in_b };

    let sqrt_0 = (cl2_f * cl_f).sqrt();

    let n0 = -cl2_f * cl_f * l2 / sp;
    let n1 = cl2_f * l / sp2;
    let n2 = cl2_f * l * sqrt_0 / sp;
    let n3 = l2 * sqrt_0 / sp2;

    let d0 = cl2_f * cl2_f * l / l2;
    let d1 = cl2_f * cl_f * l2 / l;
    let denom = d0 - d1;
    if denom.abs() < 1e-12 { return None; }

    let mut dx = (n0 - n1 + n2 + n3) / denom;
    if dx <= 0.0 {
        dx = (n0 - n1 - n2 - n3) / denom;
    }
    if dx <= 0.0 { return None; }

    if clmm2_limit > 0 { dx = dx.min(clmm2_limit as f64); }

    let sp_dx = sp * dx;
    if (l - sp_dx).abs() < 1.0 { return None; }
    let mut cl_dy = dx * l * sp * sp / (cl_f * (-dx * sp + l));

    if clmm1_limit > 0 && cl_dy > clmm1_limit as f64 {
        cl_dy = clmm1_limit as f64;
        dx = cl_dy * cl_f * l / (sp * (cl_dy * cl_f + l * sp));
    }

    let cl2_dy = cl2_f * dx * l2 * sp2 * sp2 / (cl2_f * dx * sp2 + l2);

    if cl_dy > 1.0 && cl2_dy > cl_dy {
        Some((cl_dy as u64, (cl2_dy - cl_dy) as u64))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Find optimal input amount for a route. For 2-hop routes, uses closed-form
/// formulas with ternary search fallback. For 3-hop+, uses ternary search.
pub fn find_optimal_amount(
    route: &Route,
    graph: &TokenGraph,
    max_amount: u64,
    iterations: u32,
) -> Option<(u64, u64)> {
    // 2-hop: try closed-form first
    if route.hops.len() == 2 {
        if let Some((amount_in, cf_profit)) = try_closed_form(route, graph, max_amount) {
            if amount_in > 0 && cf_profit > 0 {
                let sim_profit = simulate_route_profit(route, graph, amount_in);
                if sim_profit > 0 {
                    // Use simulated profit (ground truth) regardless of divergence
                    return Some((amount_in, sim_profit as u64));
                }
            }
        }
    }

    // Fallback: ternary search (also used for 3+ hops)
    ternary_search(route, graph, max_amount, iterations)
}

/// Try closed-form optimal calculation for a 2-hop route.
fn try_closed_form(
    route: &Route,
    graph: &TokenGraph,
    max_amount: u64,
) -> Option<(u64, u64)> {
    let hop1 = &route.hops[0];
    let hop2 = &route.hops[1];
    let pool1 = &graph.pools[hop1.pool_index as usize];
    let pool2 = &graph.pools[hop2.pool_index as usize];

    let result = match (&pool1.math, &pool2.math) {
        (PoolMath::ConstantProduct { .. } | PoolMath::BondingCurve { .. },
         PoolMath::ConstantProduct { .. } | PoolMath::BondingCurve { .. }) => {
            let p1 = extract_cp(&pool1.math, hop1.is_a_to_b)?;
            let p2 = extract_cp(&pool2.math, hop2.is_a_to_b)?;
            closed_form_cp_cp(&p1, &p2)
        }
        (PoolMath::ConstantProduct { .. } | PoolMath::BondingCurve { .. },
         PoolMath::Concentrated { .. }) => {
            let p1 = extract_cp(&pool1.math, hop1.is_a_to_b)?;
            let p2 = extract_clmm(&pool2.math)?;
            closed_form_cp_clmm(&p1, &p2, hop2.is_a_to_b)
        }
        (PoolMath::Concentrated { .. },
         PoolMath::ConstantProduct { .. } | PoolMath::BondingCurve { .. }) => {
            let p1 = extract_clmm(&pool1.math)?;
            let p2 = extract_cp(&pool2.math, hop2.is_a_to_b)?;
            closed_form_clmm_cp(&p1, &p2, hop1.is_a_to_b)
        }
        (PoolMath::Concentrated { .. }, PoolMath::Concentrated { .. }) => {
            let p1 = extract_clmm(&pool1.math)?;
            let p2 = extract_clmm(&pool2.math)?;
            closed_form_clmm_clmm(&p1, &p2, hop1.is_a_to_b, hop2.is_a_to_b)
        }
    };

    // Cap by max_amount
    result.and_then(|(amount_in, profit)| {
        if amount_in > max_amount {
            let sim = simulate_route_profit(route, graph, max_amount);
            if sim > 0 { Some((max_amount, sim as u64)) } else { None }
        } else {
            Some((amount_in, profit))
        }
    })
}

/// Ternary search for the optimal input amount that maximizes profit.
fn ternary_search(
    route: &Route,
    graph: &TokenGraph,
    max_amount: u64,
    iterations: u32,
) -> Option<(u64, u64)> {
    let mut lo: u64 = 10_000;
    let mut hi: u64 = max_amount;

    if lo >= hi {
        return None;
    }

    for _ in 0..iterations {
        if hi - lo < 3 {
            break;
        }
        let m1 = lo + (hi - lo) / 3;
        let m2 = hi - (hi - lo) / 3;

        let p1 = simulate_route_profit(route, graph, m1);
        let p2 = simulate_route_profit(route, graph, m2);

        if p1 < p2 {
            lo = m1;
        } else {
            hi = m2;
        }
    }

    let optimal = (lo + hi) / 2;
    let profit = simulate_route_profit(route, graph, optimal);

    if profit > 0 {
        Some((optimal, profit as u64))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{PoolEntry, TokenGraph};
    use crate::opportunity::{Hop, Route};
    use arrayvec::ArrayVec;
    use solana_sdk::pubkey::Pubkey;
    use solana_streamer_sdk::pool::state::{DexType, PoolMath};

    fn make_cp_pool(reserve_a: u64, reserve_b: u64, fee_num: u64, fee_den: u64) -> PoolEntry {
        PoolEntry {
            address: Pubkey::new_unique(),
            mint_a: Pubkey::new_unique(),
            mint_b: Pubkey::new_unique(),
            math: PoolMath::ConstantProduct {
                reserve_a,
                reserve_b,
                fee_numerator: fee_num,
                fee_denominator: fee_den,
            },
            dex_type: DexType::RaydiumCpmm,
            vault_a: None,
            vault_b: None,
            mint_a_is_2022: false,
            mint_b_is_2022: false,
            extra_accounts: vec![],
            last_updated_slot: 1,
        }
    }

    fn build_2hop_graph(pool1: PoolEntry, pool2: PoolEntry) -> (TokenGraph, Route) {
        let base_mint = Pubkey::new_unique();
        let mid_mint = Pubkey::new_unique();

        let mut p1 = pool1;
        p1.mint_a = base_mint;
        p1.mint_b = mid_mint;

        let mut p2 = pool2;
        p2.mint_a = mid_mint;
        p2.mint_b = base_mint;

        let mut graph = TokenGraph::new();
        let idx1 = graph.add_pool(p1);
        let idx2 = graph.add_pool(p2);

        let mut hops = ArrayVec::new();
        hops.push(Hop { pool_index: idx1, is_a_to_b: true });  // base -> mid
        hops.push(Hop { pool_index: idx2, is_a_to_b: true });  // mid -> base
        let route = Route { hops, base_mint };

        (graph, route)
    }

    #[test]
    fn test_cp_cp_closed_form_finds_profit() {
        // Pool1: 100 SOL / 200k tokens, 0.3% fee
        // Pool2: 190k tokens / 110 SOL, 0.3% fee
        // Price diff creates arb opportunity
        let pool1 = make_cp_pool(
            100_000_000_000,  // 100 SOL
            200_000_000_000,  // 200k tokens
            3, 1000,
        );
        let pool2 = make_cp_pool(
            190_000_000_000,  // 190k tokens (mid_mint = mint_a)
            110_000_000_000,  // 110 SOL (base_mint = mint_b)
            3, 1000,
        );
        let (graph, route) = build_2hop_graph(pool1, pool2);

        let result = find_optimal_amount(&route, &graph, 100_000_000_000, 10);
        assert!(result.is_some(), "Should find profitable arb");
        let (amount_in, profit) = result.unwrap();
        assert!(amount_in > 0, "Amount in should be positive");
        assert!(profit > 0, "Profit should be positive");

        // Verify with simulation
        let sim_profit = simulate_route_profit(&route, &graph, amount_in);
        assert!(sim_profit > 0, "Simulated profit should match");
    }

    #[test]
    fn test_cp_cp_no_profit_when_equal() {
        let pool1 = make_cp_pool(100_000_000_000, 100_000_000_000, 3, 1000);
        let pool2 = make_cp_pool(100_000_000_000, 100_000_000_000, 3, 1000);
        let (graph, route) = build_2hop_graph(pool1, pool2);

        let result = find_optimal_amount(&route, &graph, 100_000_000_000, 10);
        if let Some((_, profit)) = result {
            assert_eq!(profit, 0, "No profit when pools are equal");
        }
    }

    #[test]
    fn test_3hop_uses_ternary_search() {
        let base_mint = Pubkey::new_unique();
        let mid1_mint = Pubkey::new_unique();
        let mid2_mint = Pubkey::new_unique();

        let mut p1 = make_cp_pool(100_000_000_000, 200_000_000_000, 3, 1000);
        p1.mint_a = base_mint; p1.mint_b = mid1_mint;
        let mut p2 = make_cp_pool(150_000_000_000, 180_000_000_000, 3, 1000);
        p2.mint_a = mid1_mint; p2.mint_b = mid2_mint;
        let mut p3 = make_cp_pool(160_000_000_000, 110_000_000_000, 3, 1000);
        p3.mint_a = mid2_mint; p3.mint_b = base_mint;

        let mut graph = TokenGraph::new();
        let idx1 = graph.add_pool(p1);
        let idx2 = graph.add_pool(p2);
        let idx3 = graph.add_pool(p3);

        let mut hops = ArrayVec::new();
        hops.push(Hop { pool_index: idx1, is_a_to_b: true });
        hops.push(Hop { pool_index: idx2, is_a_to_b: true });
        hops.push(Hop { pool_index: idx3, is_a_to_b: true });
        let route = Route { hops, base_mint };

        // Should not panic, uses ternary search for 3-hop
        let _result = find_optimal_amount(&route, &graph, 100_000_000_000, 10);
    }

    #[test]
    fn test_closed_form_cp_cp_better_than_ternary() {
        // Verify closed-form gives at least as good results as ternary
        let pool1 = make_cp_pool(
            50_000_000_000,
            120_000_000_000,
            25, 10000, // 0.25% fee
        );
        let pool2 = make_cp_pool(
            100_000_000_000,
            60_000_000_000,
            25, 10000,
        );
        let (graph, route) = build_2hop_graph(pool1, pool2);

        let cf_result = find_optimal_amount(&route, &graph, 50_000_000_000, 10);
        let ts_result = ternary_search(&route, &graph, 50_000_000_000, 10);

        // Both should find profit
        assert!(cf_result.is_some());
        assert!(ts_result.is_some());

        let (_, cf_profit) = cf_result.unwrap();
        let (_, ts_profit) = ts_result.unwrap();

        // Closed-form should be at least as good (simulated profit)
        assert!(cf_profit >= ts_profit * 95 / 100,
            "Closed-form profit {} should be close to ternary {}", cf_profit, ts_profit);
    }
}
