# Stage 2: Route Engine Design

> Consume PoolUpdate from Stage 1, build token graph, find arbitrage cycles, emit Opportunities.

## Goal

Single-thread scanner that maintains a token directed graph, pre-builds 2/3/4-hop cycle route table from 3 base tokens (SOL, USDC, USD1), and scans for profitable arbitrage opportunities on every pool state change.

## Design Decisions

| Decision | Choice |
|----------|--------|
| Crate location | `engine/` (independent crate, depends on `solana-streamer-sdk`) |
| Base tokens | SOL + USDC + USD1 |
| Hop limit | 2/3/4-hop |
| Route table build | Batch (after warmup) + incremental (on new pool) |
| Warmup end condition | 30 seconds OR 1000 pools, whichever comes first |
| Scan trigger | Incremental (per PoolUpdate) + full scan every 5 seconds |
| Scanner threading | Single thread (50-250 us per update, well within budget) |
| Opportunity output | Complete (includes pool accounts for direct tx construction) |
| Min pool reserve | 10 SOL (or equivalent), configurable |
| Optimal amount search | Ternary search, 10 iterations |
| Min profit threshold | 0.001 SOL, configurable |

## Architecture

```
                        Stage 1 (solana-streamer)
                               |
                        PoolUpdate channel
                               |
                               v
+--------------------------------------------------------------+
|                    engine/ crate                              |
|                                                              |
|  +----------------+    +-------------------------------+     |
|  | Token Graph     |    | Route Table                   |     |
|  | (mint=node,     |--->| Vec<Route> + inverted index   |     |
|  |  pool=edge)     |    | (pool_idx -> route_indices)   |     |
|  +----------------+    +--------------+----------------+     |
|                                       |                      |
|  +------------------------------------v--------------------+ |
|  | Scanner (single thread)                                 | |
|  | 1. PoolUpdate -> update graph/cache                     | |
|  | 2. Inverted index -> scan affected routes               | |
|  | 3. Full scan every 5 seconds                            | |
|  | 4. Profitable -> ternary search optimal amount          | |
|  | 5. Emit Opportunity                                     | |
|  +------------------------------------+-------------------+  |
|                                       |                      |
|                              Opportunity channel             |
|                                       |                      |
+---------------------------------------+----------------------+
                                        |
                                        v
                                   Stage 3 (Executor)
```

## Startup Flow

1. Begin receiving PoolUpdate, accumulate pools (warmup phase)
2. 30 seconds OR 1000 pools (whichever first) -> batch build token graph + route table
3. Switch to runtime mode: incremental updates + incremental scan + 5s full scan

## Core Structures

```rust
/// Token graph: mint = node, pool = edge (bidirectional)
struct TokenGraph {
    /// mint -> node_index
    mint_to_index: HashMap<Pubkey, u32>,
    /// node_index -> mint
    index_to_mint: Vec<Pubkey>,
    /// Adjacency list: node_index -> Vec<Edge>
    adjacency: Vec<Vec<Edge>>,
}

struct Edge {
    target: u32,           // neighbor node_index
    pool_index: u32,       // index into pool cache Vec
    is_a_to_b: bool,       // swap direction
}

/// A cycle route back to base token
struct Route {
    hops: ArrayVec<Hop, 4>,  // stack-allocated, max 4 hops
    base_mint: Pubkey,        // SOL / USDC / USD1
}

struct Hop {
    pool_index: u32,
    is_a_to_b: bool,
}

/// Inverted index: pool_index -> which routes are affected
struct RouteIndex {
    pool_to_routes: Vec<Vec<u32>>,  // pool_index -> Vec<route_index>
}

/// Scanner output - everything Executor needs to build a transaction
struct Opportunity {
    route: Route,
    amount_in: u64,
    expected_profit: u64,        // lamports
    pool_states: Vec<PoolSnapshot>,  // snapshot of each hop's pool
    slot: u64,                       // latest slot among involved pools
}

struct PoolSnapshot {
    address: Pubkey,
    dex_type: DexType,
    mint_a: Pubkey,
    mint_b: Pubkey,
    is_a_to_b: bool,
    accounts: Vec<Pubkey>,       // all accounts needed for CPI
}
```

## Route Table Build

### Batch Build (after warmup)

From each base token (SOL, USDC, USD1), DFS to enumerate all 2/3/4-hop cycles:

```
for base in [SOL, USDC, USD1]:
    DFS(base, depth=0, max_depth=4, path=[])
    each cycle back to base -> store as Route
```

### Pruning Rules

1. Pool reserve < 10 SOL (or equivalent) -> exclude
2. Single hop fee > 2% -> exclude
3. Same token pair + same DEX -> keep highest liquidity only
4. 4-hop constraint: intermediate tokens must be top-200 liquidity mints

### Incremental Update (new pool discovered)

On PoolUpdate with new pool (not in graph):
1. Add mint_a / mint_b to token graph (if new nodes)
2. Add edges (bidirectional)
3. Local DFS from these two mints only, find new cycles involving the new edge
4. Append new routes to route table + update inverted index

No full route table rebuild needed.

## Scanner Logic

### Incremental Scan (per PoolUpdate)

```
Receive PoolUpdate(pool_address, new_math, slot)
  -> Update local pool cache
  -> Lookup inverted index: pool_index -> affected_route_indices
  -> For each affected route:
      simulate_route_profit(route, amount=fixed_probe)
      if profit > 0:
          ternary_search -> (optimal_amount, max_profit)
          if max_profit > min_threshold:
              emit Opportunity
```

### Full Scan (every 5 seconds)

```
For all routes:
    simulate_route_profit(route, amount=fixed_probe)
    if profit > 0:
        ternary_search -> emit Opportunity
```

## Ternary Search (Optimal Input Amount)

Profit function f(amount_in) is unimodal: rises to a peak, then falls.

- Lower bound: 10,000 lamports (0.00001 SOL)
- Upper bound: configurable (default 100 SOL, or wallet balance)
- Iterations: 10 (converges to 0.002%)
- 10 iterations x 2 simulations = 20 evaluations -> ~20-1000 us

```rust
fn find_optimal_amount(route, pool_cache, max_amount) -> Option<(u64, u64)> {
    let mut lo = 10_000;
    let mut hi = max_amount;

    for _ in 0..10 {
        let m1 = lo + (hi - lo) / 3;
        let m2 = hi - (hi - lo) / 3;
        let p1 = simulate_route_profit(route, pool_cache, m1);
        let p2 = simulate_route_profit(route, pool_cache, m2);
        if p1 < p2 { lo = m1; } else { hi = m2; }
    }

    let optimal = (lo + hi) / 2;
    let profit = simulate_route_profit(route, pool_cache, optimal);
    if profit > 0 { Some((optimal, profit)) } else { None }
}

fn simulate_route_profit(route, pool_cache, amount_in) -> i64 {
    let mut current = amount_in;
    for hop in route.hops {
        current = pool_cache[hop.pool_index].math.get_amount_out(current, hop.is_a_to_b);
        if current == 0 { return i64::MIN; }
    }
    current as i64 - amount_in as i64
}
```

## Scale Estimates

```
Active token mints:        ~5,000
Average pools per token:   ~8
Graph edges:               ~40,000

Route counts (cycles):
  2-hop:   ~40,000
  3-hop:   ~500,000 (post-pruning)
  4-hop:   ~2,000,000 (post-pruning)

Memory:
  Route struct ~ 40 bytes
  2.5M routes x 40 bytes    ~ 100 MB
  Inverted index             ~ 20 MB
  Token graph                ~ 5 MB
  Total                      ~ 125 MB

Per PoolUpdate scan:
  Average affected routes    ~50
  Per-route calculation      ~ 1-5 us
  Total scan time            ~ 50-250 us
```

## Module Structure

```
engine/
├── Cargo.toml              # solana-streamer-sdk, arrayvec, tokio
└── src/
    ├── lib.rs              # pub mod exports
    ├── graph.rs            # TokenGraph
    ├── route.rs            # Route, Hop, RouteTable, RouteIndex
    ├── scanner.rs          # Scanner main loop
    ├── optimizer.rs        # ternary search
    ├── opportunity.rs      # Opportunity, PoolSnapshot
    └── config.rs           # EngineConfig
```

## Public API

```rust
pub struct Engine {
    pub fn new(
        config: EngineConfig,
        update_rx: mpsc::Receiver<PoolUpdate>,
    ) -> (Self, mpsc::Receiver<Opportunity>);

    pub async fn run(&mut self);
}
```

Stage 1 `PoolStreamer` produces `update_rx`, passed to `Engine::new()`.
Engine produces `Opportunity` channel for Stage 3.

## What Stage 3 Expects

Stage 3 (Executor) consumes from `Opportunity` channel:
- Build transaction instruction data from PoolSnapshot accounts
- Construct 2 tx variants (Jito bundle / SWQoS)
- Parallel submit via MultiSender (Jito + Flashblock + Astralane)
- First-landed wins, others auto-fail (atomic)
