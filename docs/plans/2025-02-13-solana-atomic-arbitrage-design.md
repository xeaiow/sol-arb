# Solana Atomic Arbitrage System Design

> Off-Chain Calculation, On-Chain Execution

## Design Decisions

| Decision | Choice |
|----------|--------|
| Hop Strategy | Dynamic N-hop (fixed instruction variants: 2/3/4-hop) |
| Route Search | Pre-built route table + incremental scan (microsecond latency) |
| DEX Coverage | All 35 DEXes (via dex-pinocchio-cpi) |
| Transaction Delivery | Jito Bundle (fail = no cost) |
| Pool Data Source | gRPC Geyser real-time push |
| Profit Calculation | Fully off-chain; on-chain is pure execution + verification |
| Simulation | Skipped in production (Jito bundle failure = no cost) |

## Reference Projects

| Repo | Role | Key Takeaways |
|------|------|---------------|
| [solana-arbitrage-bot-cpi](https://github.com/RAYNBINGHAN3/solana-arbitrage-bot-cpi) | On-chain engine | Pinocchio framework, ultra-low CU patterns, simulate mode via `set_return_data`, unsafe ptr balance reads, const template instruction data |
| [dex-pinocchio-cpi](https://github.com/vnxfsc/dex-pinocchio-cpi) | CPI library | 35 DEX coverage, no_std, `repr(C, packed)` zero-copy serialization, `five8_const` compile-time base58, type-safe account structs |
| [solana-mev-bot](https://github.com/Cetipoo/solana-mev-bot) | Off-chain client | Auto DEX detection by program owner, pool grouping by mint, mixed-mode base mint, flashloan integration, multi-RPC sending |

---

## 1. High-Level Architecture

```
┌─────────────────────────────────────────────────────────┐
│                    Off-Chain Engine (Rust)               │
│                                                         │
│  ┌──────────┐  ┌──────────────┐  ┌───────────────────┐  │
│  │ gRPC     │→ │ Pool State   │→ │ Route Table       │  │
│  │ Listener │  │ Cache        │  │ (pre-built paths) │  │
│  └──────────┘  └──────────────┘  └────────┬──────────┘  │
│                                           │              │
│  ┌──────────────────────────────────────┐ │              │
│  │ Arbitrage Scanner                    │←┘              │
│  │ - Incremental scan affected routes   │                │
│  │ - AMM math for optimal amount        │                │
│  │ - Profit estimation (gas + jito tip) │                │
│  └──────────────┬───────────────────────┘                │
│                 │                                        │
│  ┌──────────────▼───────────────────────┐                │
│  │ Executor                              │                │
│  │ - Build instruction data + accounts   │                │
│  │ - Jito Bundle submission              │                │
│  └──────────────────────────────────────┘                │
└─────────────────────────────────────────────────────────┘
                          │
                          ▼
┌─────────────────────────────────────────────────────────┐
│              On-Chain Program (Pinocchio, no_std)        │
│                                                         │
│  Instruction 4: execute_2hop(params, accounts)          │
│  Instruction 5: execute_3hop(params, accounts)          │
│  Instruction 6: execute_4hop(params, accounts)          │
│                                                         │
│  Each instruction:                                      │
│  1. Parse SwapParams (raw bytes, zero-copy)             │
│  2. Record initial balance                              │
│  3. Sequential CPI swaps (via dex-pinocchio-cpi)        │
│  4. Verify final_balance > initial + min_profit         │
│  5. Atomic revert if unprofitable                       │
└─────────────────────────────────────────────────────────┘
```

Core principle: off-chain does all "thinking" (route search, math, profit estimation), on-chain does all "acting" (CPI swap, balance verification, atomic revert). The interface between them is a compact instruction data packet (<=24 bytes per transaction).

---

## 2. Project Structure

```
onchain-arb/
├── Cargo.toml                      # Workspace root
│
├── program/                        # On-chain program (Pinocchio)
│   ├── Cargo.toml                  # pinocchio + dex-pinocchio-cpi, no_std
│   └── src/
│       ├── lib.rs                  # entrypoint + 1-byte discriminator dispatch
│       ├── execute_2hop.rs         # Fixed 2-hop execution path
│       ├── execute_3hop.rs         # Fixed 3-hop execution path
│       ├── execute_4hop.rs         # Fixed 4-hop execution path
│       ├── swap.rs                 # Unified CPI swap dispatcher
│       ├── balance.rs              # Unsafe ptr token balance read
│       ├── params.rs               # Instruction data parsing (zero-copy)
│       ├── accounts.rs             # Header/pool account splitting
│       ├── constants.rs            # pool_type -> account_count lookup
│       └── error.rs                # Minimal error enum (4 variants)
│
├── client/                         # Off-chain engine
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs                 # Entry: CLI + tokio runtime + pipeline
│       ├── config.rs               # TOML config loading
│       │
│       ├── feed/                   # Data layer
│       │   ├── mod.rs
│       │   ├── geyser.rs           # gRPC Geyser account subscriptions
│       │   ├── pool_parser.rs      # Auto-detect DEX type, parse pool state
│       │   └── pool_state.rs       # Unified pool state structures
│       │
│       ├── routing/                # Route layer
│       │   ├── mod.rs
│       │   ├── graph.rs            # Token directed graph (mint=node, pool=edge)
│       │   ├── route_table.rs      # Pre-built route table: enumerate all 2/3/4-hop cycles
│       │   ├── route_index.rs      # Inverted index: pool_address -> affected routes[]
│       │   └── pruner.rs           # Route pruning (liquidity, fee, staleness)
│       │
│       ├── engine/                 # Calculation layer
│       │   ├── mod.rs
│       │   ├── scanner.rs          # Pool update -> lookup index -> scan affected routes
│       │   ├── math/
│       │   │   ├── mod.rs
│       │   │   ├── constant_product.rs  # x * y = k
│       │   │   ├── concentrated.rs      # CLMM tick-based
│       │   │   ├── dlmm.rs             # Meteora DLMM bin-based
│       │   │   ├── stable_swap.rs       # Saber, Stabble StableSwap
│       │   │   ├── weighted.rs          # Stabble WeightedSwap
│       │   │   └── bonding_curve.rs     # Pump.fun, Vertigo, DBC
│       │   ├── optimizer.rs        # Binary search for optimal amount_in
│       │   └── profit.rs           # net_profit = output - input - gas - jito_tip
│       │
│       ├── executor/               # Execution layer
│       │   ├── mod.rs
│       │   ├── tx_builder.rs       # Assemble instruction data + accounts
│       │   ├── simulator.rs        # simulateTransaction (debug/test only)
│       │   └── jito.rs             # Jito bundle gRPC submission
│       │
│       └── utils/
│           ├── mod.rs
│           ├── ata.rs              # ATA address derivation
│           └── metrics.rs          # Latency, profit, success rate stats
│
└── docs/
    └── plans/
        └── 2025-02-13-solana-atomic-arbitrage-design.md
```

---

## 3. On-Chain Program (Ultra-Low CU)

### 3.1 CU Optimization Strategies

Every design decision targets minimum CU consumption:

#### Zero-Allocation Principle

The entire program has zero heap allocation. No `Vec`, `Box`, `String`, `HashMap`. All arrays are stack-allocated with compile-time known sizes.

```
instruction data   → &[u8] slice, read raw bytes directly
account splitting  → split_at(), zero-copy
token balance      → unsafe ptr read at offset 64
instruction build  → const template + copy_from_slice patch
```

#### Compile-Time Pre-computation

| Item | Method | Savings |
|------|--------|---------|
| DEX program ID | `five8_const::decode_32_const` | Eliminate runtime base58 decode |
| Instruction discriminator | `const [u8; 8]` template | Eliminate SHA256 |
| Account count per DEX | `const POOL_COUNTS: [usize; 35]` | Eliminate match branches |
| Instruction data template | `const` pre-filled discriminator + fixed fields | Only patch variable bytes |
| `invoke::<N>` const generic | Compile-time specialization per DEX | Eliminate dynamic sizing |

#### Build Profile

```toml
[profile.release]
codegen-units = 1       # Maximum LTO effect
lto = true              # Cross-crate inlining
panic = "abort"         # No unwind overhead
opt-level = 3           # Maximum optimization
overflow-checks = false # Skip overflow checks
```

### 3.2 Instruction Data Layout

**2-hop (16 bytes):**

```
Byte 0:       discriminator (4 = 2hop, 5 = 3hop, 6 = 4hop)
Byte 1:       buy_dex_type (0-34)
Byte 2:       sell_dex_type (0-34)
Byte 3:       flags (packed bits)
              ├─ bit 0: is_buy_token_a
              ├─ bit 1: is_sell_token_a
              └─ bit 7: is_simulate
Bytes 4-11:   amount_in (u64 LE)
Bytes 12-15:  min_profit (u32 LE)
```

**3-hop (18 bytes):** adds `mid_dex_type` (1 byte) + `mid_flags` (1 byte).

**4-hop (20 bytes):** adds `mid1_dex_type`, `mid1_flags`, `mid2_dex_type`, `mid2_flags` (4 bytes).

### 3.3 Account Layout

```
┌─────────── Header (shared, passed once) ────────────┐
│ [0] Payer (signer)                                   │
│ [1] Base mint (WSOL)                                 │
│ [2] User base token account                          │
│ [3] SPL Token Program                                │
│ [4] Token-2022 Program                               │
│ [5] Memo Program                                     │
│ [6..6+N] Per intermediate token: (mint, program, ata)│
└──────────────────────────────────────────────────────┘
┌─────── Pool Accounts (sequential) ───────────────────┐
│ [hop1] pool_accounts[0..buy_count]                   │
│ [hop2] pool_accounts[0..sell_count]                  │
│ ...                                                  │
└──────────────────────────────────────────────────────┘
```

Account splitting uses chained `split_at()` calls with `POOL_COUNTS[dex_type]` constant lookup. Zero traversal, zero search.

**Header sizes:**
- 2-hop: 9 accounts (6 base + 3 for intermediate token)
- 3-hop: 12 accounts (6 base + 3 × 2 intermediate tokens)
- 4-hop: 15 accounts (6 base + 3 × 3 intermediate tokens)

### 3.4 Execution Flow (2-hop example)

```rust
#[inline(always)]
fn execute_2hop(accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    // 1. Zero-copy parse params (direct byte reads, no deserialization)
    let params = parse_2hop_params(data);

    // 2. Split accounts
    let (header, pools) = accounts.split_at(HEADER_2HOP);
    let buy_count = unsafe { *POOL_COUNTS.get_unchecked(params.buy_dex as usize) };
    let (buy_accs, sell_accs) = pools.split_at(buy_count);

    // 3. Record initial balance (unsafe ptr read, ~1 CU)
    let initial = unsafe { read_balance(header[2]) };

    // 4. CPI buy swap
    dispatch_swap(params.buy_dex, header, buy_accs, params.amount_in, params.buy_flags)?;

    // 5. Read intermediate token balance as next hop input
    let mid_amount = unsafe { read_balance(header[8]) };

    // 6. CPI sell swap
    dispatch_swap(params.sell_dex, header, sell_accs, mid_amount, params.sell_flags)?;

    // 7. Verify profit (atomic revert if failed)
    let final_bal = unsafe { read_balance(header[2]) };
    if final_bal <= initial + params.min_profit as u64 {
        return Err(ArbitrageFailed.into());
    }

    // 8. Simulate mode: return profit via set_return_data
    if params.is_simulate {
        set_return_data(&(final_bal - initial).to_le_bytes());
    }
    Ok(())
}
```

### 3.5 Swap Dispatcher

```rust
#[inline(always)]
fn dispatch_swap(dex_type: u8, header: &[AccountInfo],
                 pool: &[AccountInfo], amount: u64, flags: u8) -> ProgramResult {
    match dex_type {
        0  => cpi_meteora_cpmm(header, pool, amount, flags),
        1  => cpi_meteora_dlmm(header, pool, amount, flags),
        2  => cpi_meteora_damm_v2(header, pool, amount, flags),
        3  => cpi_pump_fun(header, pool, amount, flags),
        4  => cpi_raydium_amm(header, pool, amount, flags),
        5  => cpi_raydium_clmm(header, pool, amount, flags),
        6  => cpi_raydium_cp(header, pool, amount, flags),
        7  => cpi_whirlpool(header, pool, amount, flags),
        8  => cpi_pump_fun_amm(header, pool, amount, flags),
        9  => cpi_bonkswap(header, pool, amount, flags),
        10 => cpi_pancakeswap(header, pool, amount, flags),
        // ... 11-34: remaining DEXes
        _  => Err(UnsupportedDex.into()),
    }
}
```

Each `cpi_xxx` function is `#[inline(always)]`. With `opt-level = 3` + `lto = true` + `codegen-units = 1`, the compiler flattens the entire match + CPI into a jump table. Zero function call overhead.

### 3.6 Estimated CU Consumption

| Operation | Est. CU |
|-----------|---------|
| Instruction parsing | ~50 |
| Account splitting | ~20 |
| Balance reads (×3) | ~15 |
| Profit verification | ~10 |
| **Program overhead** | **~100** |
| CPI swap (DEX side) | ~30,000-80,000 per hop |
| **2-hop total** | **~60,000-160,000** |
| **3-hop total** | **~90,000-240,000** |
| **4-hop total** | **~120,000-320,000** |

Program self-cost is ~100 CU. The rest is DEX-side CPI overhead (uncontrollable). This is the theoretical minimum.

### 3.7 Error Handling (Minimal)

```rust
enum ArbError {
    InvalidData,      // Instruction data length mismatch
    UnsupportedDex,   // dex_type out of range
    ArbitrageFailed,  // final_balance <= initial + min_profit
    CpiFailed,        // DEX CPI returned error
}
```

4 error variants only. No account validation, no owner checks, no PDA derivation. All validation responsibility is off-chain.

---

## 4. Pool State Cache & Route Table

### 4.1 Unified Pool Math Abstraction

35 DEXes have different pool structures, but route calculation needs unified math inputs:

```rust
enum PoolMath {
    // x * y = k (Raydium AMM/CP, Meteora CPMM, Pump AMM, Bonkswap...)
    ConstantProduct {
        reserve_a: u64,
        reserve_b: u64,
        fee_numerator: u64,
        fee_denominator: u64,
    },

    // Tick-based CLMM (Raydium CLMM, Orca Whirlpool, PancakeSwap, Byreal)
    Concentrated {
        sqrt_price: u128,
        liquidity: u128,
        tick_current: i32,
        tick_arrays: Vec<TickArray>,   // Pre-loaded 3 tick arrays
        fee_rate: u32,
    },

    // Bin-based (Meteora DLMM)
    DiscretizedLMM {
        active_id: i32,
        bin_step: u16,
        bins: Vec<Bin>,                // Pre-loaded active bin range
        base_fee: u64,
        variable_fee: u64,
    },

    // A * sum(x_i) + D^n / (n^n * prod(x_i)) (Saber, Perena)
    StableSwap {
        reserves: Vec<u64>,
        amplification: u64,
        fee_rate: u64,
    },

    // Weighted (Stabble Weighted)
    Weighted {
        reserves: Vec<u64>,
        weights: Vec<u64>,
        fee_rate: u64,
    },

    // Bonding curves (Pump.fun, Vertigo, Dynamic Bonding Curve)
    BondingCurve {
        variant: BondingCurveType,
        params: [u64; 8],              // Curve-specific parameters
    },
}

struct PoolState {
    address: Pubkey,
    dex_type: u8,                       // 0-34
    mint_a: Pubkey,
    mint_b: Pubkey,
    math: PoolMath,
    account_count: usize,               // Number of accounts needed for on-chain CPI
    accounts: Vec<Pubkey>,              // Pre-stored pool-related account addresses
    last_updated_slot: u64,
}
```

### 4.2 Route Table Structure

```rust
struct Route {
    hops: ArrayVec<Hop, 4>,             // Fixed max 4 hops, stack allocated
    hop_count: u8,                       // 2, 3, or 4
}

struct Hop {
    pool_index: u32,                     // Index into PoolStateCache
    is_a_to_b: bool,                     // Swap direction
}
```

**Build process (startup):**

1. All pools form a directed graph: nodes = token mints, edges = pools (bidirectional)
2. DFS from base token (WSOL), enumerate all cycles back to WSOL, depth limit = 4
3. Store each cycle as a `Route`
4. Build inverted index: `pool_index -> Vec<route_index>`

### 4.3 Scale Estimates

```
Active token mints:        ~5,000
Average pools per token:   ~8 (across multiple DEXes)
Graph edges:               ~40,000

Route counts (cycles from WSOL):
  2-hop:   ~40,000
  3-hop:   ~500,000 (estimated, post-pruning)
  4-hop:   ~2,000,000 (estimated, post-pruning)

Memory:
  Route struct ≈ 40 bytes
  2.5M routes × 40 bytes   ≈ 100 MB
  Inverted index            ≈ 20 MB
  Pool state cache          ≈ 50 MB
  Total                     ≈ 170 MB (acceptable)

Per pool-update scan:
  Average affected routes   ~50
  Per-route calculation     ≈ 1-5 μs
  Total scan time           ≈ 50-250 μs ✅ microsecond-level
```

### 4.4 Pruning Strategy

**Startup pruning:**

1. Liquidity threshold: pool reserve < 0.1 SOL -> exclude
2. Fee threshold: single hop fee > 2% -> exclude
3. Same-DEX dedup: same token pair + same DEX -> keep highest liquidity only
4. 4-hop extra constraint: intermediate tokens must be top-200 liquidity mints

**Runtime dynamic pruning:**

5. Consecutive N negative-profit calculations -> lower scan priority
6. Pool with no updates for extended period -> mark as stale

---

## 5. gRPC Feed & Pipeline

### 5.1 Geyser Subscription

```rust
struct GeyserFeed {
    // Subscribe to account changes for all known pool addresses
    subscriptions: HashMap<Pubkey, DexType>,

    // New pool discovery: subscribe to DEX program transaction logs
    // Detect new pool creation -> dynamically add subscription + update route table
    program_subscriptions: Vec<Pubkey>,  // 35 DEX program IDs
}
```

**Subscription strategy:**

1. Startup: batch-fetch all known pools via RPC `getMultipleAccounts`, build initial cache + route table, subscribe all pool addresses via gRPC
2. Runtime: `AccountUpdate` -> parse -> update cache -> trigger scanner; Transaction logs -> detect new pool creation -> hot-expand subscriptions + route table

### 5.2 Pipeline Architecture

Lock-free async pipeline connected via channels:

```
┌─────────┐    ┌──────────┐    ┌─────────┐    ┌──────────┐
│ gRPC    │───→│ Parser   │───→│ Scanner │───→│ Executor │
│ Receiver│    │ Workers  │    │         │    │          │
└─────────┘    └──────────┘    └─────────┘    └──────────┘
  1 thread      N threads       1 thread       1 thread

channels:     channels:       channels:
AccountUpdate  PoolUpdate      Opportunity
(raw bytes)    (parsed state)  (route+params)
```

```rust
#[tokio::main]
async fn main() {
    let config = Config::load("config.toml");

    // Channels
    let (raw_tx, raw_rx) = flume::bounded::<AccountUpdate>(4096);
    let (parsed_tx, parsed_rx) = flume::bounded::<PoolUpdate>(4096);
    let (opp_tx, opp_rx) = flume::bounded::<Opportunity>(256);

    // Shared state
    let pool_cache = Arc::new(PoolStateCache::new());
    let route_table = Arc::new(RwLock::new(RouteTable::new()));

    // Stage 1: gRPC receiver
    tokio::spawn(geyser_feed(config.geyser_url, raw_tx));

    // Stage 2: Parser workers (CPU-bound, multi-threaded)
    for _ in 0..num_cpus::get() {
        let rx = raw_rx.clone();
        let tx = parsed_tx.clone();
        let cache = pool_cache.clone();
        tokio::spawn(parser_worker(rx, tx, cache));
    }

    // Stage 3: Scanner (single thread, holds route table)
    tokio::spawn(scanner_loop(parsed_rx, opp_tx, pool_cache, route_table));

    // Stage 4: Executor (single thread, Jito submission)
    tokio::spawn(executor_loop(opp_rx, config.jito, pool_cache));
}
```

---

## 6. Executor & Jito Integration

### 6.1 Opportunity Structure

```rust
struct Opportunity {
    route: Route,
    amount_in: u64,
    expected_profit: u64,
    min_profit: u32,
    scan_slot: u64,                      // Slot when this was calculated
}
```

### 6.2 Execution Flow

```
Opportunity received
    │
    ▼
1. Staleness check
   - Compare opportunity scan_slot with current latest slot
   - Gap > 2 slots → discard (state may have changed)
    │
    ▼
2. tx_builder: assemble transaction (~0.05 ms)
   - Select instruction (4/5/6 by hop_count)
   - Pack instruction data (<=24 bytes)
   - Arrange header accounts + pool accounts
   - Set compute budget (dynamic by hop_count)
    │
    ▼
3. jito: send Bundle (~5-10 ms)
   - Build Jito bundle (1 transaction + tip)
   - Submit to Jito block engine gRPC
   - Bundle failure = not landed = zero cost
    │
    ▼
4. Result tracking + metrics
```

### 6.3 Jito Bundle Sender

```rust
struct JitoSender {
    block_engine_url: String,
    tip_accounts: Vec<Pubkey>,           // 8 Jito tip accounts, randomly selected
    keypair: Arc<Keypair>,
}

impl JitoSender {
    fn calculate_tip(&self, expected_profit: u64) -> u64 {
        // Dynamic tip: 50-70% of profit for competitiveness
        // But guarantee operator keeps at least MIN_OPERATOR_PROFIT
        let tip = expected_profit * 60 / 100;
        tip.max(MIN_TIP).min(expected_profit - MIN_OPERATOR_PROFIT)
    }
}
```

### 6.4 End-to-End Latency

```
Event                              Cumulative
────────────────────────────────────────────
gRPC account update received       ~0 ms (slot-level)
Parser decodes pool state          ~0.01 ms
Scanner scans affected routes      ~0.05-0.25 ms
Optimizer finds optimal amount     ~0.01 ms
Staleness check                    ~0 ms
tx_builder assembles               ~0.05 ms
Jito bundle submission             ~5-10 ms
────────────────────────────────────────────
Total                              ~5-15 ms
```

No `simulateTransaction` in production path. Jito bundle failure = no on-chain landing = zero cost. The on-chain `min_profit` atomic revert is the sole safety mechanism.

The `simulator.rs` module is retained for debug/test use only.

---

## 7. Error Handling & Reliability

### 7.1 Off-Chain Error Recovery

| Scenario | Response |
|----------|----------|
| gRPC disconnect | Auto-reconnect with exponential backoff; full re-fetch pool state on reconnect (prevent missed updates); rebuild route table |
| Jito submission failure (network) | Do not retry same opportunity (state is stale); wait for next opportunity |
| Pool parse failure | Log warning + skip pool; does not affect other pools or routes |
| Route table too large (memory pressure) | Dynamically raise pruning thresholds; remove low-liquidity routes |
| Scanner calculation panic | `catch_unwind` isolation; log + skip route; engine does not crash |

### 7.2 Metrics

```rust
struct Metrics {
    // Latency
    geyser_lag_slots: Histogram,         // gRPC delay (slot difference)
    scan_duration_us: Histogram,         // Scan time
    e2e_latency_ms: Histogram,           // End-to-end latency

    // Effectiveness
    opportunities_found: Counter,         // Opportunities discovered
    bundles_sent: Counter,                // Bundles submitted
    bundles_landed: Counter,              // Successfully landed
    total_profit_lamports: Counter,       // Cumulative profit

    // Health
    active_pools: Gauge,                  // Active pool count
    active_routes: Gauge,                 // Active route count
    geyser_connected: Gauge,              // gRPC connection status
}
```

---

## 8. Supported DEXes (35)

Provided by `dex-pinocchio-cpi` as a direct dependency for the on-chain program:

| # | DEX | Category |
|---|-----|----------|
| 1 | Meteora CPMM | AMM |
| 2 | Meteora DLMM | CLMM (bin-based) |
| 3 | Meteora DAMM v2 | AMM |
| 4 | Meteora Dynamic Bonding Curve | Bonding Curve |
| 5 | Pump.fun | Bonding Curve |
| 6 | Pump.fun AMM | AMM |
| 7 | Raydium AMM v4 | AMM |
| 8 | Raydium CLMM | CLMM (tick-based) |
| 9 | Raydium CP | AMM |
| 10 | Raydium Launchlab | Launchpad |
| 11 | Orca Whirlpool | CLMM (tick-based) |
| 12 | Bonkswap | AMM |
| 13 | Boop.fun | Launchpad |
| 14 | Byreal | CLMM |
| 15 | Carrot | AMM |
| 16 | DefiTuna | Leveraged DEX |
| 17 | GooseFX GAMMA | AMM |
| 18 | Guacswap | AMM |
| 19 | Heaven | AMM |
| 20 | Helium Network | Network Token |
| 21 | HumidiFi | AMM |
| 22 | MetaDAO (Futarchy) | Governance/AMM |
| 23 | Moonit | Launchpad |
| 24 | OpenBook V2 | Order Book |
| 25 | PancakeSwap | CLMM |
| 26 | Perena | Stablecoin DEX |
| 27 | Perps | Perp DEX |
| 28 | Saber (Decimals) | Stableswap |
| 29 | SolFi V2 | AMM |
| 30 | Stabble CLMM | CLMM |
| 31 | Stabble Stable Swap | Stableswap |
| 32 | Stabble Weighted Swap | Weighted AMM |
| 33 | Vertigo | AMM |
| 34 | Virtuals | AMM |
| 35 | WooFi | AMM |

### Math Module Mapping

| PoolMath Variant | DEXes |
|------------------|-------|
| `ConstantProduct` | Raydium AMM/CP, Meteora CPMM, Pump AMM, Bonkswap, Guacswap, GooseFX, Carrot, HumidiFi, SolFi, Virtuals, WooFi |
| `Concentrated` | Raydium CLMM, Orca Whirlpool, PancakeSwap, Byreal, Stabble CLMM, DefiTuna |
| `DiscretizedLMM` | Meteora DLMM |
| `StableSwap` | Saber, Stabble Stable Swap, Perena |
| `Weighted` | Stabble Weighted Swap |
| `BondingCurve` | Pump.fun, Vertigo, Meteora DBC, Boop.fun, Moonit, Raydium Launchlab |

---

## 9. Configuration

```toml
# config.toml

[general]
base_mint = "So11111111111111111111111111111111111111112"  # WSOL
min_profit_lamports = 1000
max_hops = 4

[geyser]
url = "http://your-geyser-node:10000"
reconnect_delay_ms = 1000
reconnect_max_backoff_ms = 30000

[jito]
block_engine_url = "https://mainnet.block-engine.jito.wtf"
tip_percentage = 60           # % of expected profit
min_tip_lamports = 1000
min_operator_profit = 5000

[pruning]
min_pool_reserve_lamports = 100_000_000   # 0.1 SOL
max_single_hop_fee_bps = 200              # 2%
max_routes_per_mint = 500
top_n_mints_for_4hop = 200

[pools]
addresses = [
    # List of known pool addresses to bootstrap
    # Auto-detection will discover new pools at runtime
]
```
