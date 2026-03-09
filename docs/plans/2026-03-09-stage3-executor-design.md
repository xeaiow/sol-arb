# Stage 3: Executor Design

> Consume Opportunity from Stage 2, build atomic arbitrage transactions, submit via multiple channels.

## Goal

Off-chain executor + on-chain Pinocchio program. Executor builds two transaction variants (Jito bundle / SWQoS), submits concurrently via Jito gRPC + Flashblock + Astralane. On-chain program executes multi-hop swaps via dex-pinocchio-cpi and reverts if profit < min_profit.

## Design Decisions

| Decision | Choice |
|----------|--------|
| Architecture | executor/ + program/ independent crates |
| On-chain framework | Pinocchio native (no_std) |
| DEX support (initial) | 7 DEXes (same as Stage 1/2) |
| CPI library | dex-pinocchio-cpi (existing) |
| Submission channels | Jito gRPC + Flashblock + Astralane, each independently toggleable |
| Jito method | gRPC sendBundle, all regions concurrent |
| Astralane method | JSON-RPC sendTransaction, all region endpoints concurrent |
| ALT strategy | Tier 0 static ALT only |
| Flashloan | MarginFi (0% fee), manually toggled via config |
| Anti-fingerprint | CU jitter + random tip account + random fee collector |
| CU estimation | Per-DEX lookup table |
| Staleness | Hardcoded > 2 slots → discard |
| Signing | 2 variants parallel ed25519 |

## Architecture

```
Stage 2 Engine
     │
     │ Opportunity channel (mpsc)
     ▼
┌─────────────────────────────────────────────────┐
│  executor/ crate (off-chain)                     │
│                                                  │
│  1. Staleness check (slot gap > 2 → discard)     │
│  2. tx_builder: Opportunity → 2 tx variants      │
│     ├─ Variant A: Jito bundle (tip instruction)  │
│     └─ Variant B: SWQoS (priority fee in CU)     │
│  3. ALT: Tier 0 static ALT for account compress  │
│  4. Anti-fingerprint: CU jitter + random tip/fee  │
│  5. Parallel ed25519 signing (2 variants)         │
│  6. Multi-sender concurrent submit:               │
│     ├─ Jito gRPC sendBundle (N regions)           │
│     ├─ Flashblock sendTransaction                 │
│     └─ Astralane sendTransaction (10 regions)     │
└─────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────┐
│  program/ crate (on-chain, Pinocchio)            │
│                                                  │
│  1. Parse instruction data (≤20 bytes)            │
│  2. Parse account array (header + per-hop pools)  │
│  3. If flashloan: CPI MarginFi flash_borrow       │
│  4. Sequential CPI DEX swaps (dex-pinocchio-cpi)  │
│  5. If flashloan: CPI MarginFi flash_repay        │
│  6. Verify profit ≥ min_profit, else revert        │
└─────────────────────────────────────────────────┘
```

## Latency Budget

| Step | Time |
|------|------|
| Staleness check | < 1 μs |
| Instruction data + account layout | ~10-15 μs |
| ed25519 signing ×2 (parallel) | ~50-80 μs |
| Wire format serialization ×2 | ~5-10 μs |
| **tx_builder total** | **~70-100 μs** |
| Multi-channel concurrent submit | ~5-10 ms (network) |
| **End-to-end total** | **~5-10 ms** |

## On-Chain Program Design

### Instruction Format

| Discriminator | Type | Data Length |
|---|---|---|
| 0 | 2-hop | 16 bytes |
| 1 | 3-hop | 18 bytes |
| 2 | 4-hop | 20 bytes |

**2-hop (16 bytes):**
```
[0]     discriminator (u8)
[1]     buy_dex_type (u8)
[2]     sell_dex_type (u8)
[3]     flags (u8)
          bit 0: is_buy_a_to_b
          bit 1: is_sell_a_to_b
          bit 2: buy_token_is_2022
          bit 3: sell_token_is_2022
          bit 4: use_flashloan
[4-11]  amount_in (u64 LE)
[12-15] min_profit (u32 LE)
```

**3-hop (18 bytes):** adds `mid_dex_type (u8)` + `mid_flags (u8)` after byte 3.

**4-hop (20 bytes):** adds `mid1_dex_type + mid1_flags` + `mid2_dex_type + mid2_flags` after byte 3.

### Account Layout

```
[Fixed Header — 8 accounts]
  [0]  Payer (signer)
  [1]  Base mint (WSOL/USDC/USD1)
  [2]  User base token ATA
  [3]  Fee collector (randomly selected)
  [4]  SPL Token Program
  [5]  Token-2022 Program
  [6]  Associated Token Program
  [7]  System Program

[Per Intermediate Token — 3 accounts × (hop_count - 1)]
  [+0] Token mint
  [+1] Token program (SPL or Token-2022)
  [+2] User token ATA

[Flashloan Accounts — 3 accounts, only if flag set]
  [+0] MarginFi program
  [+1] MarginFi bank (lending pool)
  [+2] MarginFi bank vault

[Per Hop Pool Accounts — variable per DEX, sequential]
  Arranged by hop order, count per DEX is constant lookup.
```

### Execution Flow

```
1. Parse instruction data → hop count, dex_types, flags, amount_in, min_profit
2. Parse accounts → header + intermediate tokens + flashloan + pool accounts
3. Record initial_balance = user_base_token_ata.amount
4. If flashloan: CPI MarginFi flash_borrow(amount_in)
5. For each hop:
     Match dex_type → call corresponding dex-pinocchio-cpi swap function
6. If flashloan: CPI MarginFi flash_repay(amount_in)
7. final_balance = user_base_token_ata.amount
8. profit = final_balance - initial_balance
9. assert!(profit >= min_profit as u64), else revert
```

### DexType Mapping (initial 7)

```rust
match dex_type {
    0 => raydium_amm_v4::swap(),
    1 => raydium_cpmm::swap(),
    2 => raydium_clmm::swap(),
    3 => pumpfun::buy() / sell(),
    4 => pumpswap::swap(),
    5 => bonk::swap(),
    6 => meteora_damm_v2::swap(),
}
```

### CU Estimation (per-DEX lookup)

```rust
fn estimate_cu(hops: &[Hop]) -> u32 {
    let mut cu = 100; // program overhead (Pinocchio)
    for hop in hops {
        cu += match hop.dex_type {
            RaydiumAmmV4  => 35_000,
            RaydiumCpmm   => 35_000,
            RaydiumClmm   => 80_000,
            PumpFun       => 30_000,
            PumpSwap      => 35_000,
            Bonk          => 30_000,
            MeteoraDammV2 => 45_000,
        };
    }
    cu
}
```

## Off-Chain Executor Design

### Module Structure

```
executor/
├── Cargo.toml
└── src/
    ├── lib.rs
    ├── config.rs           ← config.toml loading
    ├── tx_builder.rs       ← Opportunity → 2 tx variants
    ├── alt.rs              ← Tier 0 ALT address
    ├── anti_fp.rs          ← CU jitter, random tip/fee collector
    ├── signer.rs           ← parallel ed25519 signing
    ├── sender/
    │   ├── mod.rs          ← MultiSender concurrent logic
    │   ├── jito.rs         ← gRPC sendBundle
    │   ├── flashblock.rs   ← JSON-RPC sendTransaction
    │   └── astralane.rs    ← JSON-RPC sendTransaction × N regions
    └── executor.rs         ← main loop
```

### config.toml

```toml
[executor]
flashloan_enabled = false
program_id = "<deployed program address>"
alt_address = "<Tier 0 ALT address>"

[executor.anti_fingerprint]
cu_jitter_range = 1000
fee_collectors_sol = ["<addr1>", "<addr2>", "<addr3>"]
fee_collector_usdc = "<addr_usdc>"

[jito]
enabled = true
block_engine_urls = [
    "https://mainnet.block-engine.jito.wtf",
    "https://amsterdam.mainnet.block-engine.jito.wtf",
    "https://frankfurt.mainnet.block-engine.jito.wtf",
    "https://ny.mainnet.block-engine.jito.wtf",
    "https://tokyo.mainnet.block-engine.jito.wtf",
    "https://slc.mainnet.block-engine.jito.wtf",
]
tip_percentage = 60
min_tip_lamports = 1000
min_operator_profit_lamports = 5000

[flashblock]
enabled = true
endpoint = "https://api.flashblock.trade"
api_key = "<key>"
cu_price_percentage = 30

[astralane]
enabled = true
api_key = "<key>"
cu_price_percentage = 30
endpoints = [
    "https://fr.gateway.astralane.io/iris",
    "https://fr2.gateway.astralane.io/iris",
    "https://ams.gateway.astralane.io/iris",
    "https://ams2.gateway.astralane.io/iris",
    "https://la.gateway.astralane.io/iris",
    "https://ny.gateway.astralane.io/iris",
    "https://jp.gateway.astralane.io/iris",
    "https://sg.gateway.astralane.io/iris",
    "https://lim.gateway.astralane.io/iris",
    "https://lit.gateway.astralane.io/iris",
]
```

### Transaction Variants

**Variant A — Jito Bundle:**
- `ComputeBudgetInstruction::set_compute_unit_limit(estimated_cu + jitter)`
- Arbitrage instruction (calling on-chain program)
- SOL transfer to random Jito tip account
- NO `set_compute_unit_price`
- Wrapped in Jito bundle, sent via gRPC

**Variant B — SWQoS (Flashblock / Astralane):**
- `ComputeBudgetInstruction::set_compute_unit_limit(estimated_cu + jitter)`
- `ComputeBudgetInstruction::set_compute_unit_price(cu_price)`
- Arbitrage instruction (calling on-chain program)
- NO separate tip instruction

### Tip / Priority Fee Calculation

**Jito (Variant A):**
```
tip = expected_profit × tip_percentage / 100
tip = max(tip, min_tip_lamports)
tip = min(tip, expected_profit - min_operator_profit_lamports)
if tip > expected_profit → skip opportunity
```

**SWQoS (Variant B):**
```
fee_budget = expected_profit × cu_price_percentage / 100
cu_price = (fee_budget × 1_000_000) / estimated_cu   // micro-lamports per CU
```

### Main Loop (executor.rs)

```rust
loop {
    let opp = opp_rx.recv().await;

    // 1. Staleness check (hardcoded)
    if current_slot - opp.slot > 2 { continue; }

    // 2. Build 2 tx variants (parallel signing)
    let (tx_jito, tx_swqos) = tx_builder.build(&opp);

    // 3. Concurrent submit all enabled channels
    multi_sender.send_all(tx_jito, tx_swqos).await;
}
```

### Multi-Channel Concurrent Submit

```rust
let mut futures = Vec::new();

if config.jito.enabled {
    for url in &config.jito.block_engine_urls {
        futures.push(jito_sender.send_bundle(url, tx_jito));
    }
}
if config.flashblock.enabled {
    futures.push(flashblock_sender.send(tx_swqos));
}
if config.astralane.enabled {
    for endpoint in &config.astralane.endpoints {
        futures.push(astralane_sender.send(endpoint, tx_swqos));
    }
}

join_all(futures).await;
```

All channels fire concurrently. First to land wins; others auto-fail (same nonce, atomic).

## Anti-Fingerprint

| Item | Method | Source |
|------|--------|--------|
| CU limit | `estimated_cu + rand() % cu_jitter_range` | config: `cu_jitter_range` |
| Jito tip account | Random 1 of 8 fixed addresses | Hardcoded (Jito official) |
| Fee collector | Random 1 from config array | config: `fee_collectors_sol` |

## Flashloan (MarginFi, 0% fee)

Controlled by `config.toml`:
- `flashloan_enabled = true` → instruction flag bit 4 set, MarginFi accounts appended
- `flashloan_enabled = false` → use own funds, no extra accounts

On-chain flow when enabled:
```
1. CPI MarginFi flash_borrow(amount_in)
2. Execute arbitrage swaps (same as normal)
3. CPI MarginFi flash_repay(amount_in)  // 0% fee, repay = borrow amount
4. Verify profit ≥ min_profit
```

## ALT Strategy (Tier 0 Only)

Single static ALT containing:
- 7 DEX program IDs
- SPL Token Program, Token-2022, Associated Token, System Program, Memo Program
- WSOL mint, USDC mint, USD1 mint
- 8 Jito tip accounts
- Executor program ID
- ~25-30 entries

Created once manually, address stored in `config.toml`. Sufficient for 2/3-hop and most 4-hop transactions.

## On-Chain Program Module Structure

```
program/
├── Cargo.toml              ← pinocchio, dex-pinocchio-cpi
└── src/
    ├── lib.rs              ← entrypoint + process_instruction
    ├── accounts.rs         ← account parsing helpers
    ├── swap.rs             ← per-DEX CPI dispatch
    └── flashloan.rs        ← MarginFi borrow/repay CPI
```

## Crate Dependencies

```
program/   → dex-pinocchio-cpi (CPI calls)
executor/  → engine/ (Opportunity channel)
executor/  → solana-sdk (transaction assembly)
executor/  → tonic (Jito gRPC)
executor/  → reqwest (Flashblock, Astralane HTTP)
executor/  → toml + serde (config parsing)
```

`program/` and `executor/` do NOT depend on each other. Instruction data format (16-20 bytes) defined independently in both.

## What Stage 2 Provides

```rust
pub struct Opportunity {
    pub route: Route,              // hops with pool indices
    pub amount_in: u64,            // optimal input amount
    pub expected_profit: u64,      // profit in lamports
    pub pool_snapshots: Vec<PoolSnapshot>,
    pub slot: u64,                 // latest slot among pools
}

pub struct PoolSnapshot {
    pub address: Pubkey,
    pub dex_type: DexType,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub is_a_to_b: bool,
    pub accounts: Vec<Pubkey>,     // all CPI accounts needed
}
```
