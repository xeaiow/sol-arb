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
| Address Lookup Table | Multi-tier ALT strategy (static + per-DEX + dynamic hot pool) |
| Token-2022 | Full support: per-mint program detection, conditional Memo injection, correct ATA derivation |
| ATA Management | Base ATAs eager at startup; route token ATAs lazy-created on-chain |
| Base Mint | Multi-base: SOL + USDC + USD1 with bridge pool for mixed-mode routes |
| Anti-Fingerprinting | CU jitter, fee collector rotation, tip account rotation |
| Flashloan | Zero-capital execution via on-chain flashloan vaults |

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
│       ├── token.rs               # Token-2022 aware transfer + balance read
│       ├── ata.rs                 # Lazy ATA creation (create_idempotent CPI)
│       └── error.rs                # Minimal error enum
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
│       │   ├── pool_state.rs       # Unified pool state structures
│       │   └── token_program.rs    # Token-2022 detection, memo injection logic
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
│       ├── alt/                    # Address Lookup Table management
│       │   ├── mod.rs
│       │   ├── manager.rs          # ALT creation, extension, deactivation, cleanup
│       │   ├── selector.rs         # Greedy ALT selection per transaction
│       │   └── index.rs            # Reverse index: account_address -> (alt, index)
│       │
│       └── utils/
│           ├── mod.rs
│           ├── ata.rs              # ATA management (eager base + lazy route)
│           ├── blockhash.rs        # Background blockhash cache (10s refresh)
│           ├── fingerprint.rs      # Anti-fingerprinting: CU jitter, collector rotation
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
              ├─ bit 2: buy_token_is_2022
              ├─ bit 3: sell_token_is_2022
              ├─ bit 4: use_flashloan
              ├─ bit 5: no_failure_mode (succeed silently if no profit)
              └─ bit 7: is_simulate
Bytes 4-11:   amount_in (u64 LE)
Bytes 12-15:  min_profit (u32 LE)
```

**3-hop (18 bytes):** adds `mid_dex_type` (1 byte) + `mid_flags` (1 byte).

**4-hop (20 bytes):** adds `mid1_dex_type`, `mid1_flags`, `mid2_dex_type`, `mid2_flags` (4 bytes).

### 3.3 Account Layout

```
┌─────────── Header (shared, passed once) ────────────────────┐
│ [0] Payer (signer)                                           │
│ [1] Base mint (WSOL, or USDC/USD1 in flashloan mode)         │
│ [2] User base token account                                  │
│ [3] Fee collector (profit destination, randomly rotated)      │
│ [4] SPL Token Program                                        │
│ [5] Token-2022 Program                                       │
│ [6] Memo Program                                             │
│ [7] Associated Token Program (for lazy ATA creation)          │
│ [8] System Program                                            │
│ [9..9+N] Per intermediate token: (mint, token_program, ata)   │
├───────────── Optional: Flashloan Extension ─────────────────┤
│ [+0] Vault authority (PDA)                                    │
│ [+1] Vault token account                                      │
├───────────── Optional: Bridge Extension (mixed-mode) ───────┤
│ [+0] Stable mint (USDC or USD1)                               │
│ [+1] User stable token account                                │
│ [+2] Bridge pool program ID                                   │
│ [+3] Bridge pool authority                                    │
│ [+4] Sysvar Instructions                                      │
│ [+5] Bridge pool state                                        │
│ [+6] Bridge vault A                                           │
│ [+7] Bridge vault B                                           │
└──────────────────────────────────────────────────────────────┘
┌─────── Pool Accounts (sequential) ───────────────────────────┐
│ [hop1] pool_accounts[0..hop1_count]                           │
│ [hop2] pool_accounts[0..hop2_count]                           │
│ ...                                                           │
└──────────────────────────────────────────────────────────────┘
```

Account splitting uses chained `split_at()` calls with `POOL_COUNTS[dex_type]` constant lookup. Zero traversal, zero search.

**Header sizes:**
- 2-hop: 12 accounts (9 base + 3 for intermediate token)
- 3-hop: 15 accounts (9 base + 3 × 2 intermediate tokens)
- 4-hop: 18 accounts (9 base + 3 × 3 intermediate tokens)
- +2 if flashloan enabled
- +8 if mixed-mode bridge needed

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

### 3.8 No-Failure Mode

When `no_failure_mode` flag is set (bit 5 of flags byte), the program succeeds silently when no profitable arbitrage is found, instead of reverting with `ArbitrageFailed`.

```rust
// In profit verification:
if final_bal <= initial + params.min_profit as u64 {
    if params.no_failure_mode {
        return Ok(()); // Succeed silently, no swap happened worth keeping
    }
    return Err(ArbitrageFailed.into());
}
```

**Use case**: Speculative transactions sent during uncertain conditions. Without this flag, every unprofitable attempt costs Jito tip (bundle landed but reverted). With this flag, the transaction lands successfully but does nothing — still costs base fee but avoids the appearance of failed transactions in on-chain logs.

**Default**: Off. Only enable when the expected hit rate is low and you want to avoid failure noise.

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
    base_mint: Pubkey,                  // SOL, USDC, or USD1
    token_program_a: Pubkey,            // SPL Token or Token-2022 for mint_a
    token_program_b: Pubkey,            // SPL Token or Token-2022 for mint_b
    needs_memo: bool,                   // Token-2022 + CLMM = needs Memo Program
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
    is_token_2022: bool,                 // Intermediate token uses Token-2022
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
2. ALT selection (~0.01 ms)
   - Collect all accounts needed for this route
   - AltSelector greedy picks best 1-3 ALTs (Tier 0 always included)
   - If NeedsEphemeral: queue Tier 3 creation, skip this opportunity
    │
    ▼
3. tx_builder: assemble V0 transaction (~0.05 ms)
   - Select instruction (4/5/6 by hop_count)
   - Pack instruction data (<=24 bytes)
   - Arrange header accounts + pool accounts
   - Compile with selected ALTs (Message::try_compile with address_lookup_tables)
   - Set compute budget (dynamic by hop_count)
    │
    ▼
4. jito: send Bundle (~5-10 ms)
   - Build Jito bundle (1 transaction + tip)
   - Submit to Jito block engine gRPC
   - Bundle failure = not landed = zero cost
    │
    ▼
5. Result tracking + metrics
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

## 9. Address Lookup Table (ALT) Strategy

### 9.1 Why ALT Is Mandatory

Solana transactions have a **1232-byte** size limit. Each account address costs 32 bytes without ALT, but only **1 byte** (index) with ALT.

```
Without ALT:
  2-hop: 28 accounts × 32 = 896 bytes  → borderline
  3-hop: 37 accounts × 32 = 1184 bytes → barely fits, no room for tip ix
  4-hop: 50 accounts × 32 = 1600 bytes → ❌ exceeds limit

With ALT:
  4-hop: 50 accounts × 1 = 50 bytes + ALT ref (32 bytes) = 82 bytes ✅
```

**Without ALT, 3-hop is unreliable and 4-hop is impossible.** ALT is not optional.

### 9.2 Multi-Tier ALT Architecture

A single transaction can reference **up to 4 ALTs** (practical limit by tx size). We use a tiered strategy where each tier serves a different purpose:

```
┌─────────────────────────────────────────────────────┐
│ Tier 0: Global Static ALT (1 table, never changes)  │
│ - 35 DEX program IDs                                │
│ - SPL Token Program, Token-2022, Memo, System       │
│ - WSOL mint                                         │
│ - Jito tip accounts (8)                             │
│ - Our program ID                                    │
│ - Common authorities (Raydium, Meteora, Orca...)    │
│ ≈ 60 entries                                        │
├─────────────────────────────────────────────────────┤
│ Tier 1: Per-DEX Pool ALTs (up to 35 tables)         │
│ - One ALT per major DEX                             │
│ - Contains pool-specific accounts for that DEX      │
│   (pool states, vaults, configs, oracles...)        │
│ - Updated when new pools discovered                 │
│ ≈ 200-256 entries each                              │
├─────────────────────────────────────────────────────┤
│ Tier 2: Hot Token ALTs (on-demand, few)              │
│ - Created ONLY when ALL conditions met:              │
│   1. Token has ≥3 profitable 4-hop opportunities     │
│      in the past N minutes                           │
│   2. Those opportunities were degraded/dropped       │
│      due to insufficient ALT coverage                │
│   3. Token has active pools across ≥3 DEXes          │
│ - Contains all pool accounts across DEXes for       │
│   that token's routes + user ATA                    │
│ - Expected count: ~5-10 (USDC, USDT, major tokens) │
│ ≈ 100-256 entries each                              │
├─────────────────────────────────────────────────────┤
│ Tier 3: Ephemeral ALTs (short-lived, opportunity)    │
│ - Created on-demand for rare/exotic routes          │
│ - Covers pools not in any existing ALT              │
│ - Deactivated + closed after use to reclaim rent    │
│ ≈ variable entries                                  │
└─────────────────────────────────────────────────────┘
```

### 9.3 ALT Selection Per Transaction Scenario

Each transaction selects **1-3 ALTs** depending on the route:

#### Scenario A: 2-hop, both pools in common DEXes

```
Route: WSOL →[Raydium CLMM pool_X]→ TOKEN_A →[Meteora DLMM pool_Y]→ WSOL

ALTs used:
  1. Tier 0 (Global Static)     → program IDs, token programs, WSOL mint
  2. Tier 1 (Raydium)           → pool_X accounts (pool_state, vaults, tick_arrays, config)
  3. Tier 1 (Meteora DLMM)      → pool_Y accounts (pair, reserves, bin_arrays, oracle)

Total ALT refs: 3 × 32 = 96 bytes
Accounts via ALT index: ~28 × 1 = 28 bytes
Remaining (signer, user ATAs): ~3 × 32 = 96 bytes (signers can't be in ALT)
Total: 96 + 28 + 96 + ix_data ≈ 250 bytes ✅
```

#### Scenario B: 3-hop, high-frequency token (Tier 2 exists)

```
Route: WSOL →[pool_X]→ USDC →[pool_Y]→ TOKEN_B →[pool_Z]→ WSOL

USDC has been promoted to Tier 2 (meets all 3 criteria: frequent 4-hop opps,
ALT drops observed, pools across ≥3 DEXes).

ALTs used:
  1. Tier 0 (Global Static)     → program IDs, common addresses
  2. Tier 2 (USDC)              → all USDC-related pool accounts, USDC mint, user USDC ATA
  3. Tier 1 (DEX for pool_Z)    → pool_Z specific accounts

Total: ~3 ALTs, all accounts resolved via index ✅

If USDC does NOT have a Tier 2 ALT yet:
  → Use Tier 0 + Tier 1 (DEX for pool_X) + Tier 1 (DEX for pool_Z)
  → pool_Y accounts go as raw addresses if space permits
  → If this route gets dropped due to tx size, it counts toward
    USDC's promotion metric (condition 2)
```

#### Scenario C: 4-hop, mixed DEXes

```
Route: WSOL →[Raydium CP]→ A →[Orca Whirlpool]→ B →[Meteora CPMM]→ C →[Pump AMM]→ WSOL

ALTs used:
  1. Tier 0 (Global Static)     → program IDs, common addresses
  2. Tier 2 (Token A Hot)       → if Token A is high-frequency, covers pools touching A
  3. Tier 1 (DEX with most pools in route) → e.g., Meteora if it has most accounts

Remaining pool accounts not in any ALT:
  → Check if they fit as raw 32-byte addresses within tx size budget
  → If not: create Tier 3 ephemeral ALT (async, may delay by 1 slot)
```

#### Scenario D: Exotic route, no ALT coverage

```
Route: WSOL →[Vertigo pool]→ RARE_TOKEN →[HumidiFi pool]→ WSOL

Neither pool is in any Tier 1/2 ALT (low frequency DEXes, rare token).

Strategy:
  1. Tier 0 (Global Static)     → program IDs (Vertigo, HumidiFi are in here)
  2. Try raw addresses           → 2 pools × ~6 accounts = 12 × 32 = 384 bytes
     + header 9 × 32 = 288 bytes (minus those in Tier 0)
     Total ≈ ~400 bytes → fits within 1232 ✅ (2-hop exotic routes usually fit)

  If it doesn't fit (unlikely for 2-hop):
  3. Create Tier 3 ephemeral ALT → adds 1 slot latency
```

#### Scenario E: New pool just discovered

```
gRPC detects new pool creation for TOKEN_X on Raydium CLMM.

1. Pool accounts are NOT yet in any ALT
2. First opportunity using this pool:
   → Use Tier 0 + raw addresses if tx fits (likely for 2-hop)
   → Queue ALT extension: add pool accounts to Tier 1 (Raydium) ALT
3. ALT extension lands (~1-2 slots later)
4. Subsequent opportunities use the updated ALT ✅

This means new pool discovery has zero latency for 2-hop,
and 1-2 slot warmup delay for 3/4-hop routes.
```

### 9.4 ALT Selection Algorithm

```rust
struct AltSelector {
    global_alt: Pubkey,                              // Tier 0
    dex_alts: HashMap<DexType, Pubkey>,              // Tier 1: dex_type -> ALT address
    token_alts: HashMap<Pubkey, Pubkey>,             // Tier 2: mint -> ALT address
    ephemeral_alts: LruCache<RouteSignature, Pubkey>, // Tier 3: route -> ALT address

    // Reverse index: account_address -> which ALT contains it
    account_to_alt: HashMap<Pubkey, (Pubkey, u8)>,   // (alt_address, index_in_alt)
}

impl AltSelector {
    fn select_alts(&self, route: &Route, all_accounts: &[Pubkey]) -> AltSelection {
        // 1. Always include Tier 0
        let mut selected = vec![self.global_alt];
        let mut resolved = HashSet::new();

        // Mark accounts resolved by Tier 0
        for acc in all_accounts {
            if let Some((alt, _)) = self.account_to_alt.get(acc) {
                if *alt == self.global_alt {
                    resolved.insert(*acc);
                }
            }
        }

        // 2. Score each candidate ALT by how many unresolved accounts it covers
        let mut candidates: Vec<(Pubkey, usize)> = Vec::new();

        // Check Tier 2 (token ALTs) for intermediate tokens in route
        for hop in &route.hops {
            let pool = &self.pool_cache[hop.pool_index];
            let non_base_mint = if pool.mint_a == WSOL { pool.mint_b } else { pool.mint_a };
            if let Some(&alt) = self.token_alts.get(&non_base_mint) {
                let coverage = self.count_coverage(alt, all_accounts, &resolved);
                candidates.push((alt, coverage));
            }
        }

        // Check Tier 1 (DEX ALTs) for each DEX in route
        for hop in &route.hops {
            let pool = &self.pool_cache[hop.pool_index];
            if let Some(&alt) = self.dex_alts.get(&pool.dex_type) {
                let coverage = self.count_coverage(alt, all_accounts, &resolved);
                candidates.push((alt, coverage));
            }
        }

        // 3. Greedy selection: pick ALT with highest coverage, repeat (max 2 more)
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        candidates.dedup_by_key(|c| c.0);

        for (alt, _) in candidates.iter().take(2) {
            selected.push(*alt);
            // Mark newly resolved accounts
            for acc in all_accounts {
                if let Some((a, _)) = self.account_to_alt.get(acc) {
                    if *a == *alt { resolved.insert(*acc); }
                }
            }
        }

        // 4. Check if remaining unresolved accounts fit as raw addresses
        let unresolved: Vec<Pubkey> = all_accounts.iter()
            .filter(|a| !resolved.contains(a))
            .cloned().collect();

        let raw_bytes = unresolved.len() * 32;
        let alt_ref_bytes = selected.len() * 32;
        let resolved_bytes = (all_accounts.len() - unresolved.len()) * 1;
        let estimated_tx_size = raw_bytes + alt_ref_bytes + resolved_bytes + TX_OVERHEAD;

        if estimated_tx_size > MAX_TX_SIZE {
            // Need Tier 3 ephemeral ALT
            return AltSelection::NeedsEphemeral {
                alts: selected,
                missing_accounts: unresolved
            };
        }

        AltSelection::Ready {
            alts: selected,
            raw_accounts: unresolved
        }
    }
}
```

### 9.5 ALT Lifecycle Management

```rust
// client/src/alt/manager.rs

struct AltManager {
    authority: Arc<Keypair>,
    rpc: RpcClient,
}

impl AltManager {
    /// Startup: create or load existing ALTs
    async fn initialize(&self, pool_cache: &PoolStateCache) -> Result<AltSelector> {
        // 1. Create/load Tier 0 (Global Static)
        let global = self.ensure_global_alt().await?;

        // 2. Create/load Tier 1 (Per-DEX)
        let mut dex_alts = HashMap::new();
        for dex_type in DexType::all() {
            let pools = pool_cache.pools_by_dex(dex_type);
            if pools.len() > 0 {
                let alt = self.ensure_dex_alt(dex_type, &pools).await?;
                dex_alts.insert(dex_type, alt);
            }
        }

        // 3. Load existing Tier 2 (Hot Tokens) from persisted state
        //    Do NOT pre-create at startup. Tier 2 is built on-demand at runtime
        //    when the promotion criteria are met (see promote_hot_tokens).
        let token_alts = self.load_persisted_token_alts().await?;

        Ok(AltSelector::new(global, dex_alts, token_alts))
    }

    /// Runtime: extend ALT when new pool discovered
    async fn on_new_pool(&self, pool: &PoolState, selector: &mut AltSelector) {
        // Add pool accounts to the appropriate Tier 1 (DEX) ALT
        if let Some(&alt) = selector.dex_alts.get(&pool.dex_type) {
            let new_accounts: Vec<Pubkey> = pool.accounts.iter()
                .filter(|a| !selector.account_to_alt.contains_key(a))
                .cloned().collect();

            if !new_accounts.is_empty() {
                self.extend_alt(alt, &new_accounts).await.ok();
                // Update reverse index
                for (i, acc) in new_accounts.iter().enumerate() {
                    selector.account_to_alt.insert(*acc, (alt, (existing_len + i) as u8));
                }
            }
        }
    }

    /// Runtime: promote token to Tier 2 when criteria met
    async fn maybe_promote_to_tier2(
        &self,
        mint: &Pubkey,
        metrics: &Metrics,
        selector: &mut AltSelector,
    ) {
        // Skip if already has Tier 2 ALT
        if selector.token_alts.contains_key(mint) { return; }

        // All 3 conditions must be met:
        // 1. ≥3 profitable 4-hop opportunities in past 5 minutes
        let opp_count = metrics.profitable_4hop_count(mint, Duration::from_secs(300));
        if opp_count < 3 { return; }

        // 2. At least 1 opportunity was dropped/degraded due to ALT shortage
        let dropped = metrics.alt_dropped_count(mint, Duration::from_secs(300));
        if dropped < 1 { return; }

        // 3. Token has active pools across ≥3 distinct DEXes
        let dex_count = self.pool_cache.distinct_dex_count(mint);
        if dex_count < 3 { return; }

        // All conditions met → create Tier 2 ALT
        let pools = self.pool_cache.pools_by_mint(mint);
        if let Ok(alt) = self.create_token_alt(mint, &pools).await {
            selector.token_alts.insert(*mint, alt);
            self.persist_token_alt(mint, &alt).await.ok();
            tracing::info!(%mint, dex_count, opp_count, "Promoted token to Tier 2 ALT");
        }
    }

    /// Periodic: demote Tier 2 ALTs that are no longer justified
    async fn demote_stale_tier2(&self, metrics: &Metrics, selector: &mut AltSelector) {
        // If a Tier 2 ALT hasn't been used in any landed bundle for 30 min,
        // deactivate + close to reclaim rent
        let stale: Vec<Pubkey> = selector.token_alts.iter()
            .filter(|(mint, _)| {
                metrics.last_tier2_use(mint).elapsed() > Duration::from_secs(1800)
            })
            .map(|(mint, _)| *mint)
            .collect();

        for mint in stale {
            if let Some(alt) = selector.token_alts.remove(&mint) {
                self.deactivate_and_close(alt).await.ok();
                self.remove_persisted_token_alt(&mint).await.ok();
                tracing::info!(%mint, "Demoted stale Tier 2 ALT");
            }
        }
    }

    /// Periodic: deactivate + close unused Tier 3 ephemeral ALTs
    async fn cleanup_ephemeral_alts(&self, selector: &mut AltSelector) {
        // ALTs need deactivation cooldown (~512 slots ≈ 3.5 min) before closing
        // Close reclaims rent (~0.002 SOL per ALT)
        for (sig, alt) in selector.ephemeral_alts.iter() {
            if alt.last_used.elapsed() > Duration::from_secs(300) {
                self.deactivate_and_close(alt.address).await.ok();
            }
        }
    }
}
```

### 9.6 ALT Cost Analysis

```
Creation cost:
  - Create ALT:      ~0.003 SOL (rent-exempt minimum)
  - Extend (per 30 addresses): ~0.001 SOL (rent increase)

Steady-state ALTs:
  Tier 0: 1 table                   = 0.003 SOL
  Tier 1: ~15 active DEX tables     = 0.045 SOL
  Tier 2: ~5-10 on-demand tables    = 0.045-0.090 SOL
  Tier 3: ~5 ephemeral (rotating)   = 0.015 SOL (reclaimed on close)
  ─────────────────────────────────────────────
  Total rent locked:                ≈ 0.11-0.15 SOL

Runtime cost:
  - ALT extend transaction: ~5000 lamports (negligible)
  - Tier 3 create+close cycle: ~0.003 SOL (fully reclaimed)
  - Net ongoing cost: essentially zero
```

### 9.7 Constraints & Edge Cases

**Signer accounts cannot be in ALT.** The payer (index 0) must always be a raw 32-byte address in the transaction. This is 1 account = 32 bytes overhead per transaction, unavoidable.

**ALT must be active for ≥1 slot before use.** Newly created or extended ALTs require waiting for the extension transaction to confirm. For Tier 1/2 this is a one-time cost. For Tier 3 ephemeral ALTs, this adds ~400ms latency (1 slot).

**Max 256 entries per ALT.** If a DEX has >256 unique pool accounts, it needs multiple Tier 1 ALTs. In practice, we only store the top-256 most liquid pools per DEX.

**ALT deactivation cooldown: ~512 slots (~3.5 min).** Tier 3 ephemeral ALTs cannot be closed immediately. The manager tracks deactivation state and closes after cooldown.

**Transaction can reference up to ~4 ALTs** (practical limit). The Tier 0 always occupies 1 slot, leaving 3 for Tier 1/2/3 selection. The greedy selection algorithm optimizes coverage within this constraint.

---

## 10. Token-2022 Support

Token-2022 (SPL Token 2022) is increasingly prevalent on Solana. Many new tokens use it for transfer fees, interest-bearing mechanics, and other extensions. Not supporting it means missing a significant portion of arbitrage opportunities.

### 10.1 Detection (Off-Chain)

```rust
// feed/token_program.rs

const TOKEN_2022_PROGRAM_ID: Pubkey = /* TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb */;

struct MintInfo {
    address: Pubkey,
    token_program: Pubkey,          // spl_token::ID or TOKEN_2022_PROGRAM_ID
    needs_memo: bool,               // true if Token-2022 + CLMM pool
}

fn detect_token_program(mint_account: &Account) -> Pubkey {
    if mint_account.owner == spl_token::ID {
        spl_token::ID
    } else if mint_account.owner == TOKEN_2022_PROGRAM_ID {
        TOKEN_2022_PROGRAM_ID
    } else {
        panic!("Unknown token program for mint");
    }
}
```

This is detected once per mint during pool initialization by checking the mint account's `owner` field on-chain.

### 10.2 Memo Program Injection

CLMM-style pools require the **Memo Program** as an additional account. The rules differ per DEX:

- **Whirlpool**: **ALWAYS** needs Memo Program, regardless of token type
- **DLMM, Raydium CLMM, PancakeSwap, Byreal, Stabble CLMM**: Need Memo Program **only when token uses Token-2022**
- **AMM-style pools**: Never need Memo Program

```rust
// Conditional memo injection during account assembly
fn needs_memo_program(dex_type: u8, token_program: &Pubkey) -> bool {
    // Whirlpool ALWAYS needs memo, regardless of token program
    if dex_type == DEX_WHIRLPOOL {
        return true;
    }
    // Other CLMM pools: only when Token-2022
    if *token_program == spl_token::ID {
        return false;
    }
    matches!(dex_type,
        DEX_METEORA_DLMM | DEX_RAYDIUM_CLMM |
        DEX_PANCAKESWAP | DEX_BYREAL | DEX_STABBLE_CLMM
    )
}
```

**Impact on account layout**: When memo is needed, it is already in the header (index 5). The on-chain program reads a flag from instruction data to know whether to include it in the CPI call.

### 10.3 ATA Derivation

Token-2022 ATAs live at **different addresses** than SPL Token ATAs for the same mint:

```rust
// WRONG: always uses SPL Token program
let ata = get_associated_token_address(&wallet, &mint);

// CORRECT: uses the actual token program for this mint
let ata = get_associated_token_address_with_program_id(&wallet, &mint, &token_program);
```

This propagates everywhere: pool state caching, route table, transaction building, ALT entries.

### 10.4 On-Chain Token-2022 Handling

```rust
// program/src/token.rs

/// Read token balance - works for both SPL Token and Token-2022
/// Both store amount at byte offset 64 in the account data
#[inline(always)]
pub unsafe fn read_balance(account: &AccountInfo) -> u64 {
    // Same layout for both programs: offset 64 = amount field
    core::ptr::read_unaligned(account.data_ptr().add(64) as *const u64)
}

/// Determine which token program to use for CPI
/// The token program account is passed in the header
#[inline(always)]
fn select_token_program(header: &[AccountInfo], flags: u8) -> &AccountInfo {
    if flags & FLAG_TOKEN_2022 != 0 {
        &header[4]  // Token-2022 Program
    } else {
        &header[3]  // SPL Token Program
    }
}
```

**CU impact**: Zero additional CU. The balance layout is identical for both programs. The only difference is which program ID is passed to the CPI call.

### 10.5 Instruction Data Flag Extension

Add a Token-2022 flag per hop in the existing flags byte:

```
Byte 3 (2-hop flags):
  bit 0: is_buy_token_a
  bit 1: is_sell_token_a
  bit 2: buy_token_is_2022        ← NEW
  bit 3: sell_token_is_2022       ← NEW
  bit 7: is_simulate
```

This tells the on-chain program which token program to use for each hop's CPI, with no runtime detection overhead.

---

## 11. ATA Management

### 11.1 Two-Tier Strategy

```
Tier 1: Base Token ATAs — Eager (startup)
  - WSOL, USDC, USD1
  - Created at bot startup via create_associated_token_account_idempotent
  - These are always needed, so pre-create unconditionally

Tier 2: Route Token ATAs — Lazy (on-chain)
  - All intermediate tokens in swap routes
  - Created by the on-chain program during first execution
  - Uses create_associated_token_account_idempotent CPI
```

### 11.2 Why Lazy ATA Creation

```
Problem with eager creation:
  - 5,000 active tokens × 1 ATA each = 5,000 creation transactions at startup
  - Each costs ~0.002 SOL rent = 10 SOL total locked
  - Most tokens may never be traded
  - New tokens discovered at runtime need immediate ATA creation

Lazy creation solves all of these:
  - Only create ATA when actually executing a swap
  - On-chain create_idempotent is safe (no-op if exists)
  - Cost: ~20,000 extra CU on FIRST trade of each token only
  - Subsequent trades: 0 extra CU (ATA already exists)
```

### 11.3 On-Chain Lazy ATA

```rust
// program/src/ata.rs

use pinocchio::instruction::{AccountMeta, Instruction};

/// Create ATA if it doesn't exist. No-op if it already exists.
/// Uses the Associated Token Program's create_idempotent instruction.
#[inline(always)]
pub fn ensure_ata(
    payer: &AccountInfo,
    wallet: &AccountInfo,
    mint: &AccountInfo,
    ata: &AccountInfo,
    token_program: &AccountInfo,
    system_program: &AccountInfo,
) -> ProgramResult {
    // If account has data, ATA already exists → skip
    if ata.data_len() > 0 {
        return Ok(());
    }

    // CPI to Associated Token Program: create_idempotent
    let ix = Instruction {
        program_id: &ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: &[
            AccountMeta::writable_signer(payer.key()),
            AccountMeta::writable(ata.key()),
            AccountMeta::readonly(wallet.key()),
            AccountMeta::readonly(mint.key()),
            AccountMeta::readonly(system_program.key()),
            AccountMeta::readonly(token_program.key()),
        ],
        data: &[1], // 1 = create_idempotent instruction
    };
    invoke::<6>(&ix, &[payer, ata, wallet, mint, system_program, token_program])
}
```

### 11.4 Account Layout Impact

The header already includes per-hop token accounts. The Associated Token Program + System Program must be in the header for lazy creation:

```
Updated Header (canonical, see Section 3.3 for full layout):
  [0] Payer (signer)
  [1] Base mint (WSOL)
  [2] User base token account
  [3] Fee collector
  [4] SPL Token Program
  [5] Token-2022 Program
  [6] Memo Program
  [7] Associated Token Program        ← needed for lazy ATA
  [8] System Program                  ← needed for lazy ATA
  [9..9+N] Per intermediate token: (mint, token_program_for_mint, ata)
```

**Header sizes (updated):**
- 2-hop: 12 accounts (9 base + 3 for intermediate token)
- 3-hop: 15 accounts (9 base + 3 × 2 intermediate tokens)
- 4-hop: 18 accounts (9 base + 3 × 3 intermediate tokens)
- +2 if flashloan, +8 if mixed-mode bridge

---

## 12. Mixed-Mode Base Mint (SOL/USDC/USD1 Bridge)

### 12.1 Problem

Not all pools use SOL (WSOL) as the base token. Many pools pair against USDC or USD1. If a token has pools on both SOL-based and USDC-based DEXes, ignoring this means missing cross-base arbitrage opportunities.

```
Example: TOKEN_X
  Pool A (Pump.fun):    TOKEN_X / SOL
  Pool B (some DEX):    TOKEN_X / USDC

Without bridge: these two pools cannot form an arbitrage route
With bridge:    SOL →[Pool A]→ TOKEN_X →[Pool B]→ USDC →[Bridge]→ SOL ✅
```

### 12.2 Bridge Pool Design

A **bridge pool** is a high-liquidity SOL/USDC (or SOL/USD1) pool used to convert between base mints within a single atomic transaction.

```rust
// Hardcoded bridge pools (Raydium V4, highest liquidity SOL/stable pairs)
const SOL_USDC_BRIDGE: BridgePool = BridgePool {
    pool: pubkey!("58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2"),
    program: RAYDIUM_AMM_V4,
    usdc_vault: pubkey!("..."),
    sol_vault: pubkey!("..."),
};

const SOL_USD1_BRIDGE: BridgePool = BridgePool {
    pool: pubkey!("FaDoeere161VKUFqcrQGEM8it6kSCHKrLyq7wWyPvBkPq"),
    program: RAYDIUM_AMM_V4,
    usd1_vault: pubkey!("..."),
    sol_vault: pubkey!("..."),
};
```

### 12.3 Off-Chain: Route Graph Extension

The route graph must model base mint as part of the edge:

```rust
struct PoolEdge {
    pool_index: u32,
    mint_a: Pubkey,
    mint_b: Pubkey,
    base_mint: Pubkey,      // SOL, USDC, or USD1
    dex_type: u8,
}
```

When building routes, the graph allows transitions through bridge pools:

```
WSOL → [SOL-base pool] → TOKEN → [USDC-base pool] → USDC → [Bridge] → WSOL
```

The bridge hop is treated as a regular hop in the route (counts toward 4-hop limit).

### 12.4 Off-Chain: Detection

During pool initialization, detect the base mint for each pool:

```rust
fn detect_base_mint(pool: &PoolState) -> Pubkey {
    let sol = WSOL_MINT;
    let usdc = USDC_MINT;
    let usd1 = USD1_MINT;

    if pool.mint_a == sol || pool.mint_b == sol { sol }
    else if pool.mint_a == usdc || pool.mint_b == usdc { usdc }
    else if pool.mint_a == usd1 || pool.mint_b == usd1 { usd1 }
    else { Pubkey::default() } // No recognized base → skip pool
}
```

### 12.5 On-Chain: Bridge Execution

The bridge swap is just another CPI hop. No special on-chain logic needed — the off-chain engine treats the bridge as hop N in the route, and the on-chain program executes it like any other swap.

### 12.6 Account Layout for Mixed-Mode

When a route uses mixed base mints, additional accounts are needed:

```
Extended Header (mixed-mode):
  [...base header...]
  [+0] USDC mint (or USD1 mint)
  [+1] User USDC token account (or USD1)
  [+2] Bridge pool program ID
  [+3] Bridge pool authority
  [+4] Bridge pool state
  [+5] Bridge vault A
  [+6] Bridge vault B
```

These bridge accounts go into the Tier 0 ALT (they are static, known addresses).

### 12.7 Profit Calculation Adjustment

Mixed-mode routes need an extra step in profit calculation:

```
Standard route: profit = final_WSOL - initial_WSOL
Mixed route:    profit = final_WSOL - initial_WSOL
                (bridge conversion is already part of the hop chain,
                 so final balance is always in WSOL regardless of path)
```

No change needed in the on-chain profit check — it always compares WSOL balances.

---

## 13. Anti-Fingerprinting

MEV bots are high-value targets for detection by validators, competing bots, and sandwich attackers. Multiple fingerprinting countermeasures reduce the bot's on-chain signature.

### 13.1 CU Limit Jitter

```rust
// executor/tx_builder.rs

fn compute_budget_ix(base_cu: u32) -> Instruction {
    // Add random 0-999 to make each tx's CU limit unique
    let jittered_cu = base_cu + (rand::random::<u32>() % 1000);
    ComputeBudgetInstruction::set_compute_unit_limit(jittered_cu)
}
```

Without jitter, every 2-hop transaction has identical CU limit = trivially fingerprintable.

### 13.2 Fee Collector Rotation

```rust
// executor/tx_builder.rs

const FEE_COLLECTORS_SOL: [Pubkey; 3] = [/* 3 different addresses */];
const FEE_COLLECTOR_USDC: Pubkey = /* dedicated USDC collector */;
const FEE_COLLECTOR_FLASHLOAN: Pubkey = /* dedicated flashloan collector */;

fn select_fee_collector(base_mint: &Pubkey, use_flashloan: bool) -> Pubkey {
    if use_flashloan {
        FEE_COLLECTOR_FLASHLOAN  // Flashloan has its own collector
    } else if *base_mint == USDC_MINT {
        FEE_COLLECTOR_USDC
    } else {
        FEE_COLLECTORS_SOL[rand::random::<usize>() % FEE_COLLECTORS_SOL.len()]
    }
}
```

Rotating fee collectors prevents pattern matching on the fee destination. Multiple collector wallets aggregate profits that can be consolidated off-chain.

### 13.3 Jito Tip Account Rotation

Already in the design (8 tip accounts, randomly selected). This is standard Jito practice.

### 13.4 Flashloan Vault Rotation

```rust
const FLASHLOAN_VAULT_AUTHORITIES: [Pubkey; 2] = [/* 2 vault authorities */];

fn select_vault(base_mint: &Pubkey) -> (Pubkey, usize) {
    if *base_mint == USDC_MINT {
        (FLASHLOAN_VAULT_AUTHORITIES[0], 0) // USDC always vault 0
    } else {
        let idx = rand::random::<usize>() % 2;
        (FLASHLOAN_VAULT_AUTHORITIES[idx], idx)
    }
}
```

Two SOL flashloan vaults randomly selected. Different vaults produce different transaction signatures.

### 13.5 Combined Effect

```
Without anti-fingerprinting:
  CU limit:      always 200,000        → 1 pattern
  Fee collector:  always 0xABC...       → 1 pattern
  Tip account:    always 0xDEF...       → 1 pattern
  Vault:          always 0x123...       → 1 pattern
  Combined signatures: 1 × 1 × 1 × 1 = 1 unique pattern (trivially detectable)

With anti-fingerprinting:
  CU limit:      200,000-200,999       → 1,000 variants
  Fee collector:  3 rotation            → 3 variants
  Tip account:    8 rotation            → 8 variants
  Vault:          2 rotation            → 2 variants
  Combined signatures: 1000 × 3 × 8 × 2 = 48,000 unique patterns
```

---

## 14. Flashloan Integration

Flashloan enables **zero-capital arbitrage**: borrow the input amount, execute the arb, repay the loan + fee, keep the profit. If the arb fails, the entire transaction reverts including the loan — zero cost.

### 14.1 Architecture

```
┌─────────────────────────────────────────────────────┐
│ Single Atomic Transaction                            │
│                                                      │
│  1. Borrow X SOL from flashloan vault               │
│  2. Execute arbitrage (2/3/4-hop swaps)              │
│  3. Repay X SOL + protocol fee to vault              │
│  4. Verify: remaining profit ≥ min_profit            │
│                                                      │
│  If ANY step fails → entire TX reverts → zero cost   │
└─────────────────────────────────────────────────────┘
```

### 14.2 Instruction Data Extension

```
Existing flags byte, add flashloan bit:

Byte 3 (flags):
  bit 0: is_buy_token_a
  bit 1: is_sell_token_a
  bit 2: buy_token_is_2022
  bit 3: sell_token_is_2022
  bit 4: use_flashloan               ← NEW
  bit 7: is_simulate
```

### 14.3 Account Layout Extension

When flashloan is enabled, 2 extra accounts are added to the header:

```
Flashloan Header Extension:
  [+0] Vault authority (PDA)
  [+1] Vault token account (PDA-derived or ATA, depends on vault index)
```

These go into the Tier 0 ALT since they are static addresses.

### 14.4 Vault Token Account Derivation

Two vault patterns (matching the reference implementation):

```rust
fn select_vault(base_mint: &Pubkey) -> (Pubkey, usize) {
    if *base_mint == USDC_MINT {
        // USDC: always vault 0 (PDA-derived token account)
        (FLASHLOAN_VAULT_AUTHORITIES[0], 0)
    } else {
        // SOL: randomly select between 2 vaults (anti-fingerprinting)
        let idx = rand::random::<usize>() % FLASHLOAN_VAULT_AUTHORITIES.len();
        (FLASHLOAN_VAULT_AUTHORITIES[idx], idx)
    }
}

fn derive_vault_token_account(vault_index: usize, base_mint: &Pubkey) -> Pubkey {
    if vault_index == 0 {
        // Vault 0: PDA-derived
        let (pda, _) = Pubkey::find_program_address(
            &[b"vault_token_account", base_mint.as_ref()],
            &EXECUTOR_PROGRAM_ID,
        );
        pda
    } else {
        // Vault 1: standard ATA
        get_associated_token_address(&FLASHLOAN_VAULT_AUTHORITIES[1], base_mint)
    }
}
```

### 14.5 On-Chain Flashloan Flow

```rust
#[inline(always)]
fn execute_2hop_flashloan(accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let params = parse_2hop_params(data);

    let (header, pools) = accounts.split_at(HEADER_2HOP_FLASHLOAN);
    let vault_authority = &header[VAULT_AUTHORITY_IDX];
    let vault_token = &header[VAULT_TOKEN_IDX];

    // 1. Record vault balance before borrow
    let vault_before = unsafe { read_balance(vault_token) };

    // 2. Transfer from vault to user (CPI to token program)
    transfer_from_vault(vault_authority, vault_token, &header[2], params.amount_in)?;

    // 3. Execute arb (same as non-flashloan)
    let initial = unsafe { read_balance(header[2]) };
    dispatch_swap(params.buy_dex, header, buy_accs, params.amount_in, params.buy_flags)?;
    let mid = unsafe { read_balance(header[TOKEN_ATA_IDX]) };
    dispatch_swap(params.sell_dex, header, sell_accs, mid, params.sell_flags)?;
    let final_bal = unsafe { read_balance(header[2]) };

    // 4. Repay: transfer amount_in back to vault
    transfer_to_vault(&header[0], &header[2], vault_token, params.amount_in)?;

    // 5. Verify profit
    let profit = final_bal - initial;
    if profit <= params.min_profit as u64 {
        return Err(ArbitrageFailed.into());
    }

    Ok(())
}
```

### 14.6 Capital Requirements Comparison

```
Without flashloan:
  - Need to hold SOL for every trade
  - Capital locked = max concurrent trade size
  - Typical: 1-10 SOL per bot instance

With flashloan:
  - Only need SOL for: Jito tips + tx fees
  - Capital locked ≈ 0.01 SOL
  - Can execute arbs of any size (limited by vault liquidity)
```

---

## 15. DEX-Specific Special Handling

Several DEXes require protocol-specific handling beyond standard CPI.

> **Implementation note**: Detailed per-DEX specs (exact byte offsets for pool parsing, complete account sequences, PDA derivation seeds, instruction discriminators) will be documented in separate files under `docs/dex/` during implementation. Each DEX gets its own spec file (e.g., `docs/dex/pump-fun.md`, `docs/dex/raydium-clmm.md`).

### 15.1 Pump.fun Ecosystem

Pump.fun has the most complex account requirements:

```
Standard accounts:
  - program_id, pool, global_config, event_authority
  - coin_creator_vault_ata (PDA: creator's token account)
  - coin_creator_vault_authority
  - protocol_fee_recipient + protocol_fee_recipient_ata
  - base_vault, quote_vault

Additional infrastructure:
  - global_volume_accumulator (PDA: [b"global_volume_accumulator"])
  - user_volume_accumulator (PDA: [b"user_volume_accumulator", wallet])
  - fee_program (pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ)
  - fee_config (5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx)

Mayhem mode:
  - is_mayhem_mode flag on pool data → switches fee wallet
  - Must detect and route fees to correct wallet
```

**Off-chain must pre-derive** all PDA accounts for Pump.fun pools. These are per-wallet and per-pool, not just per-pool.

### 15.2 Humidifi XOR-Encoded Pool Data

Humidifi deliberately obfuscates pool data with XOR encoding:

```rust
const XOR_KEYS: [u64; 4] = [
    0xfb5c_e87a_ae44_3c38,
    0x04a2_1784_51ba_c3c7,
    0x04a1_1787_51b9_c3c6,
    0x04a0_1786_51b8_c3c5,
];

fn decode_pubkey(encoded: &[u8; 32]) -> Pubkey {
    let mut decoded = [0u8; 32];
    for i in 0..4 {
        let chunk = u64::from_le_bytes(encoded[i*8..(i+1)*8].try_into().unwrap());
        let decrypted = chunk ^ XOR_KEYS[i];
        decoded[i*8..(i+1)*8].copy_from_slice(&decrypted.to_le_bytes());
    }
    Pubkey::new_from_array(decoded)
}
```

This must be applied during pool parsing to extract real vault and authority addresses.

### 15.3 Meteora DAMM Vault-in-Vault

Meteora Dynamic AMM pools use an indirect vault structure:

```
Pool account → pool.token_a_vault → vault_object → vault_object.token_vault
                                                     ↑ this is the REAL token vault
```

During pool initialization, an extra account fetch is needed:
1. Fetch pool account → get `token_a_vault` and `token_b_vault` addresses
2. Fetch vault accounts → get `vault.token_vault` for each
3. Use the inner `token_vault` addresses in the CPI

### 15.4 Vertigo PDA-Derived Vaults

Vertigo pools derive vaults via PDA instead of storing them:

```rust
let (vault_a, _) = Pubkey::find_program_address(
    &[b"vault", pool.as_ref(), mint_a.as_ref()],
    &VERTIGO_PROGRAM_ID,
);
```

Plus a unique `pool_owner` field that must be passed to the on-chain instruction.

### 15.5 PancakeSwap / Byreal CLMM Reuse

PancakeSwap and Byreal use the **exact same pool data layout** as Raydium CLMM. The off-chain parser can reuse Raydium CLMM deserialization with different program IDs:

```rust
fn parse_clmm_pool(data: &[u8], program_id: &Pubkey) -> PoolState {
    // Same layout for Raydium CLMM, PancakeSwap, and Byreal
    let pool = RaydiumClmmLayout::deserialize(data);
    PoolState {
        dex_type: match program_id {
            RAYDIUM_CLMM_ID => DEX_RAYDIUM_CLMM,
            PANCAKESWAP_ID => DEX_PANCAKESWAP,
            BYREAL_ID => DEX_BYREAL,
            _ => unreachable!(),
        },
        ..pool.into()
    }
}
```

### 15.6 Heaven Dual-Base Support

Heaven pools can have either SOL or USDC as base (unlike most DEXes that are SOL-only):

```rust
fn is_heaven_supported(pool: &HeavenPool) -> bool {
    pool.mint_a == SOL_MINT || pool.mint_a == USDC_MINT ||
    pool.mint_b == SOL_MINT || pool.mint_b == USDC_MINT
}
```

This must be handled in the mixed-mode detection (Chapter 12).

---

## 16. Blockhash Management

### 16.1 Background Refresh

```rust
// utils/blockhash.rs

struct BlockhashCache {
    hash: Arc<RwLock<Hash>>,
}

impl BlockhashCache {
    async fn start_refresh_loop(&self, rpc: RpcClient) {
        let cache = self.hash.clone();
        tokio::spawn(async move {
            loop {
                match rpc.get_latest_blockhash().await {
                    Ok(hash) => {
                        *cache.write().await = hash;
                    }
                    Err(e) => {
                        tracing::warn!("Blockhash refresh failed: {}", e);
                        // Keep using old hash, valid for ~60-90s
                    }
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
    }

    async fn get(&self) -> Hash {
        *self.hash.read().await
    }
}
```

### 16.2 Why 10 Seconds

```
Solana blockhash validity: ~60-90 seconds (150 slots × 400ms)
Refresh interval: 10 seconds
Max staleness: 10 seconds (worst case: refresh just failed + next attempt)
Safety margin: 50-80 seconds before expiry

This is conservative. Could reduce to 5s for lower staleness,
but 10s minimizes RPC calls while staying well within validity.
```

### 16.3 Integration with Pipeline

The blockhash cache is shared across all pipeline stages via `Arc<RwLock<Hash>>`. The executor reads it just before building the transaction — no RPC call in the hot path.

```
Impact on latency: eliminates ~20-50ms RPC call from the execution path
```

---

## 17. Configuration

```toml
# config.toml

[general]
base_mints = [
    "So11111111111111111111111111111111111111112",   # WSOL (primary)
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", # USDC (bridge)
    "F1uxquJMBWPqd8RfaBEp3bz4JBNTLBkPfkPxbmGfVNMN", # USD1 (bridge)
]
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

[alt]
global_alt_address = ""              # Empty = create on first run, then persist
# Tier 2 promotion criteria (ALL must be met)
tier2_min_profitable_4hop_opps = 3   # ≥3 profitable 4-hop opps in window
tier2_min_alt_drops = 1              # ≥1 opportunity dropped due to ALT shortage
tier2_min_distinct_dexes = 3         # Token must have pools across ≥3 DEXes
tier2_eval_window_sec = 300          # Evaluation window (5 min)
tier2_demote_after_sec = 1800        # Demote if unused for 30 min
ephemeral_cleanup_interval_sec = 300 # Clean up unused Tier 3 ALTs every 5 min
max_alts_per_tx = 3                  # Max ALTs referenced per transaction (1 reserved for Tier 0)

[flashloan]
enabled = true
vault_authorities = [
    "5LFpzqgsxrSfhKwbaFiAEJ2kbc9QyimjKueswsyU4T3o",
    "4B2yxi8n7jr8w3K7cssokLNJZ6k2NjiwKwLdQ8L9dbAA",
]

[anti_fingerprint]
cu_jitter_range = 1000               # Random 0..N added to CU limit
fee_collectors_sol = [
    "GPpkDpzCDmYJY5qNhYmM14c7rct1zmkjWc2CjR5g7RZ1",
    "J6c7noBHvWju4mMA3wXt3igbBSp2m9ATbA6cjMtAUged",
    "BjsfwxDu7GX7RRW6oSRTpMkASdXAgCcHnXEcatqSfuuY",
]
fee_collector_usdc = "GzVRuLF349u78FHpr8KbqMhrZ1aDxnhSF59JWiZ6tbgt"

[blockhash]
refresh_interval_sec = 10

[wallet]
private_key = "$WALLET_PRIVATE_KEY"  # Supports $ENV_VAR substitution

[pools]
addresses = [
    # List of known pool addresses to bootstrap
    # Auto-detection will discover new pools at runtime
]
```
