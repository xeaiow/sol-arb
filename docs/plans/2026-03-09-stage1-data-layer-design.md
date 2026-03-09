# Stage 1: Data Layer Design

> Extend solana-streamer from "transaction event parser" to "arbitrage data layer"

## Goal

Real-time pool state tracking for 7 DEXes. Output: PoolMath state updates via channel for downstream route engine (Stage 2).

## Design Decisions

| Decision | Choice |
|----------|--------|
| Base project | solana-streamer (fork, extend) |
| Pool discovery | Transaction event-driven (existing event parsers) |
| Pool state tracking | gRPC account subscription (push) |
| Initial state fetch | One-time RPC `getAccountInfo` per new pool |
| Output format | `PoolMath` enum (matches system design doc) |
| Pool cache | `DashMap<Pubkey, PoolState>` (lock-free concurrent) |
| DEX coverage | 7: Raydium AMM V4, CPMM, CLMM, PumpFun, PumpSwap, Bonk, Meteora DAMM v2 |
| Existing code | Preserve all: gRPC infra, event parsers, ShredStream, metrics |

## Architecture

```
┌─────────────────────────────────────────────────────┐
│              solana-streamer (extended)               │
│                                                      │
│  ┌──────────────────┐    ┌────────────────────────┐ │
│  │ Transaction       │    │ Account                 │ │
│  │ Event Sub         │    │ Data Sub                │ │
│  │ (existing)        │    │ (new)                   │ │
│  │                   │    │                         │ │
│  │ 7 DEX program IDs │    │ Dynamic pool addresses  │ │
│  └────────┬──────────┘    └────────▲───────────────┘ │
│           │                        │                  │
│           ▼                        │                  │
│  ┌──────────────────┐              │                  │
│  │ Pool Discovery    │── new pool ─┘                  │
│  │ Engine            │                                │
│  │ (extract pool     │     ┌────────────────────────┐ │
│  │  addr + mint from │     │ Pool State Decoder     │ │
│  │  tx events)       │     │ (raw bytes → PoolMath) │ │
│  └──────────────────┘     │ 7 DEX layouts           │ │
│                            └────────┬───────────────┘ │
│                                     │                  │
│                            ┌────────▼───────────────┐ │
│                            │ PoolStateCache          │ │
│                            │ DashMap<Pubkey, Pool>   │ │
│                            └────────┬───────────────┘ │
│                                     │                  │
│                            ┌────────▼───────────────┐ │
│                            │ PoolUpdate channel      │ │
│                            │ (→ Stage 2 intake)      │ │
│                            └────────────────────────┘ │
└─────────────────────────────────────────────────────┘
```

## Data Flow

```
1. Startup
   → gRPC subscribe to 7 DEX program transaction events (existing)

2. Receive swap/create transaction event
   → Extract pool address + token mints (pool_discovery)
   → If new pool:
     a. getAccountInfo() for initial state (one-time RPC)
     b. Decode raw bytes → PoolMath (pool_decoder)
     c. Store in PoolStateCache
     d. update_subscription() to add account subscription

3. Receive account data update (gRPC push)
   → Decode raw bytes → update PoolMath in cache
   → Send PoolUpdate to channel (for Stage 2)
```

## Core Structures

```rust
/// Unified pool state for all DEXes
struct PoolState {
    address: Pubkey,
    dex_type: DexType,                  // Which DEX (0-6 for Stage 1)
    mint_a: Pubkey,
    mint_b: Pubkey,
    token_program_a: Pubkey,            // SPL Token or Token-2022
    token_program_b: Pubkey,
    math: PoolMath,                     // Pricing math state
    accounts: Vec<Pubkey>,              // Accounts needed for on-chain CPI
    last_updated_slot: u64,
}

/// Pool math variants (from system design doc)
enum PoolMath {
    /// x * y = k (Raydium AMM V4/CPMM, PumpSwap, Bonk, Meteora DAMM v2)
    ConstantProduct {
        reserve_a: u64,
        reserve_b: u64,
        fee_numerator: u64,
        fee_denominator: u64,
    },

    /// Tick-based CLMM (Raydium CLMM)
    Concentrated {
        sqrt_price: u128,
        liquidity: u128,
        tick_current: i32,
        tick_arrays: Vec<TickArray>,
        fee_rate: u32,
    },

    /// Bonding curve (PumpFun)
    BondingCurve {
        variant: BondingCurveType,
        params: [u64; 8],
    },
}

/// Sent to Stage 2 channel on every pool state change
struct PoolUpdate {
    pool_address: Pubkey,
    dex_type: DexType,
    mint_a: Pubkey,
    mint_b: Pubkey,
    math: PoolMath,                     // New state
    slot: u64,
}
```

## DEX Pool Math Mapping

| DEX | PoolMath Variant | Decode Complexity |
|-----|-----------------|-------------------|
| Raydium AMM V4 | ConstantProduct | Simple: read reserves + fee at known offsets |
| Raydium CPMM | ConstantProduct | Simple |
| PumpSwap | ConstantProduct | Simple |
| Bonk | ConstantProduct | Simple |
| Meteora DAMM v2 | ConstantProduct | Simple |
| PumpFun | BondingCurve | Medium: bonding curve params |
| Raydium CLMM | Concentrated | Complex: sqrt_price + tick arrays |

5 out of 7 are ConstantProduct — straightforward `reserve_a / reserve_b` extraction.

## Module Plan

### New Modules

```
src/
├── pool/                          # New: pool state layer
│   ├── mod.rs
│   ├── state.rs                   # PoolState, PoolMath, PoolUpdate structs
│   ├── cache.rs                   # PoolStateCache (DashMap wrapper)
│   ├── discovery.rs               # Extract pool addr from tx events, trigger subscription
│   └── decoder/                   # Raw bytes → PoolMath
│       ├── mod.rs                 # DexType dispatch
│       ├── raydium_amm_v4.rs      # AMM V4 account layout decode
│       ├── raydium_cpmm.rs        # CPMM account layout decode
│       ├── raydium_clmm.rs        # CLMM account layout decode
│       ├── pumpfun.rs             # Bonding curve decode
│       ├── pumpswap.rs            # PumpSwap pool decode
│       ├── bonk.rs                # Bonk pool decode
│       └── meteora_damm_v2.rs     # Meteora DAMM v2 decode
```

### Modified Modules

| Module | Change |
|--------|--------|
| `yellowstone_grpc.rs` | Add dual-mode: tx event sub + account data sub, integrate pool discovery callback |
| `lib.rs` | Export new pool module |

### Preserved (No Change)

- `streaming/grpc/` — Connection management, object pooling, reconnection
- `streaming/event_parser/` — All 7 DEX transaction event parsers (used by pool discovery)
- `streaming/shred_stream.rs` — ShredStream support
- `streaming/common/` — Config, metrics, SIMD utils

## Pool Discovery Flow (Detail)

```rust
/// Called when transaction event parser emits a DexEvent
fn on_dex_event(event: &DexEvent, cache: &PoolStateCache, grpc: &YellowstoneGrpc) {
    // Extract pool address from event
    let pool_address = match event {
        DexEvent::RaydiumAmmV4SwapEvent(e) => e.pool_id,
        DexEvent::RaydiumCpmmSwapEvent(e) => e.pool_id,
        DexEvent::PumpFunTradeEvent(e) => e.bonding_curve,
        DexEvent::PumpSwapBuyEvent(e) => e.pool,
        // ... other DEX events
        _ => return,
    };

    // Skip if already tracked
    if cache.contains(&pool_address) { return; }

    // Fetch initial state via RPC (one-time)
    let account_data = rpc.get_account_info(&pool_address).await;

    // Decode and cache
    let pool_state = decode_pool(event.dex_type(), &pool_address, &account_data);
    cache.insert(pool_address, pool_state);

    // Add to gRPC account subscription
    grpc.update_subscription(
        existing_tx_filters,
        vec![AccountFilter { account: vec![pool_address.to_string()], ..default }],
    ).await;
}
```

## Account Data Decode (Detail)

```rust
/// Decode raw account bytes into PoolMath based on DEX type
fn decode_pool(dex_type: DexType, address: &Pubkey, data: &[u8]) -> PoolState {
    let math = match dex_type {
        DexType::RaydiumAmmV4 => {
            // Raydium AMM V4 layout: reserves at known byte offsets
            PoolMath::ConstantProduct {
                reserve_a: u64::from_le_bytes(data[offset_a..offset_a+8].try_into().unwrap()),
                reserve_b: u64::from_le_bytes(data[offset_b..offset_b+8].try_into().unwrap()),
                fee_numerator: 25,       // Raydium default 0.25%
                fee_denominator: 10000,
            }
        }
        DexType::RaydiumClmm => {
            PoolMath::Concentrated {
                sqrt_price: u128::from_le_bytes(data[offset..offset+16].try_into().unwrap()),
                liquidity: u128::from_le_bytes(data[offset..offset+16].try_into().unwrap()),
                tick_current: i32::from_le_bytes(data[offset..offset+4].try_into().unwrap()),
                tick_arrays: vec![],     // Loaded separately
                fee_rate: u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()),
            }
        }
        DexType::PumpFun => {
            PoolMath::BondingCurve {
                variant: BondingCurveType::PumpFun,
                params: decode_bonding_params(data),
            }
        }
        // ... other DEXes (most are ConstantProduct)
        _ => unreachable!(),
    };

    PoolState {
        address: *address,
        dex_type,
        mint_a: Pubkey::new_from_array(data[mint_a_offset..].try_into().unwrap()),
        mint_b: Pubkey::new_from_array(data[mint_b_offset..].try_into().unwrap()),
        token_program_a: detect_token_program(&mint_a),
        token_program_b: detect_token_program(&mint_b),
        math,
        accounts: extract_pool_accounts(dex_type, data),
        last_updated_slot: 0,
    }
}
```

## On Account Update (Detail)

```rust
/// Called when gRPC pushes account data update
fn on_account_update(
    account: &AccountPretty,
    cache: &PoolStateCache,
    update_tx: &Sender<PoolUpdate>,
) {
    let pool_address = account.pubkey;

    // Lookup existing pool to get dex_type
    let Some(mut pool) = cache.get_mut(&pool_address) else { return };

    // Re-decode math from new account data
    let new_math = decode_pool_math(pool.dex_type, &account.data);

    // Update cache
    pool.math = new_math.clone();
    pool.last_updated_slot = account.slot;

    // Notify downstream (Stage 2)
    let _ = update_tx.try_send(PoolUpdate {
        pool_address,
        dex_type: pool.dex_type,
        mint_a: pool.mint_a,
        mint_b: pool.mint_b,
        math: new_math,
        slot: account.slot,
    });
}
```

## Validation Criteria

Stage 1 is complete when:

1. Start the program with only a gRPC endpoint configured
2. Program auto-discovers active pools across 7 DEXes via transaction events
3. Someone swaps on a discovered pool
4. Program outputs real-time pool state update:
   ```
   [slot 12345678] Pool 7XawhbbxtsRcQA8KTkHT9f9nc6d97... (RaydiumAmmV4)
   ConstantProduct { reserve_a: 50000000000, reserve_b: 8000000000, fee: 25/10000 }
   ```
5. All 7 DEXes produce correct PoolMath output
6. PoolUpdate channel emits updates that Stage 2 can consume

## Dependencies

No new crate dependencies needed. solana-streamer already has:
- `yellowstone-grpc-client` — gRPC streaming
- `solana-sdk` — Pubkey, account types
- `solana-client` — RPC calls (for initial state fetch)
- `dashmap` — concurrent map (for PoolStateCache)
- `spl-token` / `spl-token-2022` — token program detection
- `tokio` — async runtime, channels

## What Stage 2 Expects

Stage 2 (route engine) will consume from the `PoolUpdate` channel:
- Build token directed graph (mint = node, pool = edge)
- Pre-build route table (2/3/4-hop cycles)
- On each PoolUpdate → scan affected routes → find arbitrage opportunities
