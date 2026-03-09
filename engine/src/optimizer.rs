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

/// Ternary search for the optimal input amount that maximizes profit.
/// Returns Some((optimal_amount_in, expected_profit)) if profitable.
pub fn find_optimal_amount(
    route: &Route,
    graph: &TokenGraph,
    max_amount: u64,
    iterations: u32,
) -> Option<(u64, u64)> {
    let mut lo: u64 = 10_000; // 0.00001 SOL
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
