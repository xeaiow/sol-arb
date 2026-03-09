# Stage 1: Data Layer Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Extend solana-streamer to auto-discover pools via transaction events, subscribe to pool account data via gRPC, decode raw bytes into PoolMath, and emit PoolUpdate events via channel.

**Architecture:** Dual-mode gRPC subscription — transaction events for pool discovery, account data subscription for real-time state tracking. Existing event parsers preserved as pool discovery engine. New pool module added for state decoding and caching.

**Tech Stack:** Rust, yellowstone-grpc-client 10.2.0, solana-sdk 3.0.0, solana-client 3.1.9, dashmap 6.1.0, tokio 1.50.0, borsh 1.6.0

**Key Insight — Reserves Storage:**
- **Direct reserves in pool state:** PumpFun (virtual/real reserves), Bonk (virtual/real reserves), Raydium CLMM (sqrt_price + liquidity)
- **Reserves in separate vault token accounts:** Raydium AMM V4 (token_coin/token_pc), Raydium CPMM (token_0_vault/token_1_vault), PumpSwap (pool_base_token_account/pool_quote_token_account), Meteora DAMM v2

For vault-based DEXes, we must subscribe to vault token accounts AND pool accounts. Vault token account balance = reserves.

---

## Task 1: Define PoolMath and PoolState structs

**Files:**
- Create: `solana-streamer/src/pool/mod.rs`
- Create: `solana-streamer/src/pool/state.rs`
- Modify: `solana-streamer/src/lib.rs`

**Step 1: Create pool module with state definitions**

Create `src/pool/mod.rs`:
```rust
pub mod state;
```

Create `src/pool/state.rs`:
```rust
use solana_sdk::pubkey::Pubkey;

/// DEX type identifier matching system design doc
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
        fee_rate: u32,
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
            PoolMath::ConstantProduct { reserve_a, reserve_b, fee_numerator, fee_denominator } => {
                let (r_in, r_out) = if is_a_to_b {
                    (*reserve_a as f64, *reserve_b as f64)
                } else {
                    (*reserve_b as f64, *reserve_a as f64)
                };
                let fee = *fee_numerator as f64 / *fee_denominator as f64;
                let amt = amount_in as f64 * (1.0 - fee);
                let out = (r_out * amt) / (r_in + amt);
                out as u64
            }
            PoolMath::BondingCurve { virtual_token_reserves, virtual_sol_reserves, .. } => {
                // PumpFun: SOL → Token (buy) or Token → SOL (sell)
                let (r_in, r_out) = if is_a_to_b {
                    (*virtual_sol_reserves as f64, *virtual_token_reserves as f64)
                } else {
                    (*virtual_token_reserves as f64, *virtual_sol_reserves as f64)
                };
                let amt = amount_in as f64 * 0.99; // 1% fee
                let out = (r_out * amt) / (r_in + amt);
                out as u64
            }
            PoolMath::Concentrated { sqrt_price_x64, liquidity, .. } => {
                // Simplified single-tick quote (accurate for small amounts)
                let price = (*sqrt_price_x64 as f64 / (1u128 << 64) as f64).powi(2);
                if is_a_to_b {
                    (amount_in as f64 * price) as u64
                } else {
                    (amount_in as f64 / price) as u64
                }
            }
        }
    }
}

/// Unified pool state
#[derive(Debug, Clone)]
pub struct PoolState {
    pub address: Pubkey,
    pub dex_type: DexType,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Option<Pubkey>,        // For vault-based DEXes
    pub vault_b: Option<Pubkey>,        // For vault-based DEXes
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
    pub math: PoolMath,
    pub slot: u64,
}
```

**Step 2: Export pool module from lib.rs**

Modify `src/lib.rs` — add after existing modules:
```rust
pub mod pool;
```

**Step 3: Verify it compiles**

Run: `cd solana-streamer && cargo check`
Expected: compiles with no errors

**Step 4: Commit**

```bash
git add src/pool/mod.rs src/pool/state.rs src/lib.rs
git commit -m "feat(pool): add PoolMath, PoolState, PoolUpdate structs"
```

---

## Task 2: Build PoolStateCache

**Files:**
- Create: `solana-streamer/src/pool/cache.rs`
- Modify: `solana-streamer/src/pool/mod.rs`

**Step 1: Create cache module**

Create `src/pool/cache.rs`:
```rust
use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;

use super::state::{DexType, PoolMath, PoolState, PoolUpdate};

/// Thread-safe pool state cache with update notification
pub struct PoolStateCache {
    pools: DashMap<Pubkey, PoolState>,
    /// Reverse index: vault address → pool address (for vault-based DEXes)
    vault_to_pool: DashMap<Pubkey, Pubkey>,
    update_tx: mpsc::Sender<PoolUpdate>,
}

impl PoolStateCache {
    pub fn new(update_tx: mpsc::Sender<PoolUpdate>) -> Self {
        Self {
            pools: DashMap::new(),
            vault_to_pool: DashMap::new(),
            update_tx,
        }
    }

    /// Insert a newly discovered pool
    pub fn insert(&self, pool: PoolState) {
        // Register vault → pool mapping for vault-based DEXes
        if let Some(vault_a) = &pool.vault_a {
            self.vault_to_pool.insert(*vault_a, pool.address);
        }
        if let Some(vault_b) = &pool.vault_b {
            self.vault_to_pool.insert(*vault_b, pool.address);
        }
        self.pools.insert(pool.address, pool);
    }

    /// Check if pool is already tracked
    pub fn contains(&self, address: &Pubkey) -> bool {
        self.pools.contains_key(address)
    }

    /// Get pool state (read-only)
    pub fn get(&self, address: &Pubkey) -> Option<dashmap::mapref::one::Ref<Pubkey, PoolState>> {
        self.pools.get(address)
    }

    /// Lookup pool address by vault address
    pub fn pool_by_vault(&self, vault: &Pubkey) -> Option<Pubkey> {
        self.vault_to_pool.get(vault).map(|r| *r.value())
    }

    /// Update pool math from new account data and emit PoolUpdate
    pub fn update_math(&self, address: &Pubkey, math: PoolMath, slot: u64) {
        if let Some(mut pool) = self.pools.get_mut(address) {
            pool.math = math.clone();
            pool.last_updated_slot = slot;

            let update = PoolUpdate {
                pool_address: pool.address,
                dex_type: pool.dex_type,
                mint_a: pool.mint_a,
                mint_b: pool.mint_b,
                math,
                slot,
            };

            // Non-blocking send — drop update if channel full
            let _ = self.update_tx.try_send(update);
        }
    }

    /// Update reserves for vault-based pool (called when vault token account changes)
    pub fn update_vault_balance(&self, vault_address: &Pubkey, balance: u64, is_vault_a: bool, slot: u64) {
        let pool_address = match self.pool_by_vault(vault_address) {
            Some(addr) => addr,
            None => return,
        };

        if let Some(mut pool) = self.pools.get_mut(&pool_address) {
            match &mut pool.math {
                PoolMath::ConstantProduct { reserve_a, reserve_b, .. } => {
                    if is_vault_a {
                        *reserve_a = balance;
                    } else {
                        *reserve_b = balance;
                    }
                }
                _ => return, // Only ConstantProduct pools use vaults
            }
            pool.last_updated_slot = slot;

            let update = PoolUpdate {
                pool_address: pool.address,
                dex_type: pool.dex_type,
                mint_a: pool.mint_a,
                mint_b: pool.mint_b,
                math: pool.math.clone(),
                slot,
            };
            let _ = self.update_tx.try_send(update);
        }
    }

    /// Number of tracked pools
    pub fn len(&self) -> usize {
        self.pools.len()
    }

    /// Check if vault address belongs to vault_a or vault_b
    pub fn is_vault_a(&self, vault_address: &Pubkey) -> Option<bool> {
        let pool_address = self.pool_by_vault(vault_address)?;
        let pool = self.pools.get(&pool_address)?;
        if pool.vault_a.as_ref() == Some(vault_address) {
            Some(true)
        } else {
            Some(false)
        }
    }

    /// Get all vault addresses that need subscription
    pub fn all_vault_addresses(&self) -> Vec<Pubkey> {
        self.vault_to_pool.iter().map(|r| *r.key()).collect()
    }

    /// Get all pool addresses
    pub fn all_pool_addresses(&self) -> Vec<Pubkey> {
        self.pools.iter().map(|r| *r.key()).collect()
    }
}
```

**Step 2: Add cache to pool module**

Modify `src/pool/mod.rs`:
```rust
pub mod cache;
pub mod state;
```

**Step 3: Verify it compiles**

Run: `cd solana-streamer && cargo check`
Expected: compiles with no errors

**Step 4: Commit**

```bash
git add src/pool/cache.rs src/pool/mod.rs
git commit -m "feat(pool): add PoolStateCache with vault reverse index"
```

---

## Task 3: Build pool decoders for 5 ConstantProduct DEXes

**Files:**
- Create: `solana-streamer/src/pool/decoder/mod.rs`
- Create: `solana-streamer/src/pool/decoder/raydium_amm_v4.rs`
- Create: `solana-streamer/src/pool/decoder/raydium_cpmm.rs`
- Create: `solana-streamer/src/pool/decoder/pumpswap.rs`
- Create: `solana-streamer/src/pool/decoder/bonk.rs`
- Create: `solana-streamer/src/pool/decoder/meteora_damm_v2.rs`
- Modify: `solana-streamer/src/pool/mod.rs`

These decoders convert existing parsed account events (DexEvent) into PoolState. They reuse the existing Borsh-deserialized structs.

**Step 1: Create decoder dispatch module**

Create `src/pool/decoder/mod.rs`:
```rust
pub mod raydium_amm_v4;
pub mod raydium_cpmm;
pub mod pumpswap;
pub mod bonk;
pub mod meteora_damm_v2;

use solana_sdk::pubkey::Pubkey;
use crate::streaming::event_parser::DexEvent;
use super::state::{DexType, PoolState};

/// Try to extract PoolState from a DexEvent account event.
/// Returns None if the event is not a pool account event.
pub fn pool_state_from_event(event: &DexEvent) -> Option<PoolState> {
    match event {
        DexEvent::RaydiumAmmV4AmmInfoAccountEvent(e) => raydium_amm_v4::decode(e),
        DexEvent::RaydiumCpmmPoolStateAccountEvent(e) => raydium_cpmm::decode(e),
        DexEvent::PumpSwapPoolAccountEvent(e) => pumpswap::decode(e),
        DexEvent::BonkPoolStateAccountEvent(e) => bonk::decode(e),
        _ => None,
    }
}

/// Try to extract PoolState from raw account bytes when we know the DEX type.
/// Used for initial state fetch via RPC getAccountInfo.
pub fn pool_state_from_bytes(dex_type: DexType, address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    match dex_type {
        DexType::RaydiumAmmV4 => raydium_amm_v4::decode_bytes(address, data),
        DexType::RaydiumCpmm => raydium_cpmm::decode_bytes(address, data),
        DexType::PumpSwap => pumpswap::decode_bytes(address, data),
        DexType::Bonk => bonk::decode_bytes(address, data),
        DexType::MeteoraDammV2 => meteora_damm_v2::decode_bytes(address, data),
        _ => None, // PumpFun and CLMM handled in separate tasks
    }
}
```

**Step 2: Create Raydium AMM V4 decoder**

Create `src/pool/decoder/raydium_amm_v4.rs`:
```rust
use solana_sdk::pubkey::Pubkey;
use crate::streaming::event_parser::protocols::raydium_amm_v4::RaydiumAmmV4AmmInfoAccountEvent;
use crate::streaming::event_parser::protocols::raydium_amm_v4::types::amm_info_decode;
use crate::pool::state::{DexType, PoolMath, PoolState};

/// Decode from existing parsed event
pub fn decode(event: &RaydiumAmmV4AmmInfoAccountEvent) -> Option<PoolState> {
    let info = &event.amm_info;
    Some(PoolState {
        address: event.pubkey,
        dex_type: DexType::RaydiumAmmV4,
        mint_a: info.coin_mint,
        mint_b: info.pc_mint,
        vault_a: Some(info.token_coin),
        vault_b: Some(info.token_pc),
        math: PoolMath::ConstantProduct {
            reserve_a: 0, // Reserves live in vault token accounts, updated separately
            reserve_b: 0,
            fee_numerator: info.fees.swap_fee_numerator,
            fee_denominator: info.fees.swap_fee_denominator,
        },
        last_updated_slot: event.metadata.slot,
    })
}

/// Decode from raw account bytes (for RPC getAccountInfo)
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    // Raydium AMM V4 uses discriminator [6] at offset 0, no 8-byte prefix
    let info = amm_info_decode(data)?;
    Some(PoolState {
        address: *address,
        dex_type: DexType::RaydiumAmmV4,
        mint_a: info.coin_mint,
        mint_b: info.pc_mint,
        vault_a: Some(info.token_coin),
        vault_b: Some(info.token_pc),
        math: PoolMath::ConstantProduct {
            reserve_a: 0,
            reserve_b: 0,
            fee_numerator: info.fees.swap_fee_numerator,
            fee_denominator: info.fees.swap_fee_denominator,
        },
        last_updated_slot: 0,
    })
}
```

**Step 3: Create Raydium CPMM decoder**

Create `src/pool/decoder/raydium_cpmm.rs`:
```rust
use solana_sdk::pubkey::Pubkey;
use crate::streaming::event_parser::protocols::raydium_cpmm::RaydiumCpmmPoolStateAccountEvent;
use crate::streaming::event_parser::protocols::raydium_cpmm::types::pool_state_decode;
use crate::pool::state::{DexType, PoolMath, PoolState};

pub fn decode(event: &RaydiumCpmmPoolStateAccountEvent) -> Option<PoolState> {
    let ps = &event.pool_state;
    Some(PoolState {
        address: event.pubkey,
        dex_type: DexType::RaydiumCpmm,
        mint_a: ps.token_0_mint,
        mint_b: ps.token_1_mint,
        vault_a: Some(ps.token_0_vault),
        vault_b: Some(ps.token_1_vault),
        math: PoolMath::ConstantProduct {
            reserve_a: 0, // Reserves in vault accounts
            reserve_b: 0,
            fee_numerator: 25,       // Default 0.25%, actual from amm_config
            fee_denominator: 10000,
        },
        last_updated_slot: event.metadata.slot,
    })
}

pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < 8 + 629 { return None; }
    let ps = pool_state_decode(&data[8..])?;
    Some(PoolState {
        address: *address,
        dex_type: DexType::RaydiumCpmm,
        mint_a: ps.token_0_mint,
        mint_b: ps.token_1_mint,
        vault_a: Some(ps.token_0_vault),
        vault_b: Some(ps.token_1_vault),
        math: PoolMath::ConstantProduct {
            reserve_a: 0,
            reserve_b: 0,
            fee_numerator: 25,
            fee_denominator: 10000,
        },
        last_updated_slot: 0,
    })
}
```

**Step 4: Create PumpSwap decoder**

Create `src/pool/decoder/pumpswap.rs`:
```rust
use solana_sdk::pubkey::Pubkey;
use crate::streaming::event_parser::protocols::pumpswap::PumpSwapPoolAccountEvent;
use crate::streaming::event_parser::protocols::pumpswap::types::pool_decode;
use crate::pool::state::{DexType, PoolMath, PoolState};

pub fn decode(event: &PumpSwapPoolAccountEvent) -> Option<PoolState> {
    let pool = &event.pool;
    Some(PoolState {
        address: event.pubkey,
        dex_type: DexType::PumpSwap,
        mint_a: pool.base_mint,
        mint_b: pool.quote_mint,
        vault_a: Some(pool.pool_base_token_account),
        vault_b: Some(pool.pool_quote_token_account),
        math: PoolMath::ConstantProduct {
            reserve_a: 0, // Reserves in vault accounts
            reserve_b: 0,
            fee_numerator: 25,       // PumpSwap LP fee from global config
            fee_denominator: 10000,
        },
        last_updated_slot: event.metadata.slot,
    })
}

pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < 8 + 187 { return None; }
    let pool = pool_decode(&data[8..])?;
    Some(PoolState {
        address: *address,
        dex_type: DexType::PumpSwap,
        mint_a: pool.base_mint,
        mint_b: pool.quote_mint,
        vault_a: Some(pool.pool_base_token_account),
        vault_b: Some(pool.pool_quote_token_account),
        math: PoolMath::ConstantProduct {
            reserve_a: 0,
            reserve_b: 0,
            fee_numerator: 25,
            fee_denominator: 10000,
        },
        last_updated_slot: 0,
    })
}
```

**Step 5: Create Bonk decoder**

Create `src/pool/decoder/bonk.rs`:
```rust
use solana_sdk::pubkey::Pubkey;
use crate::streaming::event_parser::protocols::bonk::BonkPoolStateAccountEvent;
use crate::streaming::event_parser::protocols::bonk::types::pool_state_decode;
use crate::pool::state::{DexType, PoolMath, PoolState};

/// Bonk stores reserves directly in pool state (virtual_base/virtual_quote)
pub fn decode(event: &BonkPoolStateAccountEvent) -> Option<PoolState> {
    let ps = &event.pool_state;
    Some(PoolState {
        address: event.pubkey,
        dex_type: DexType::Bonk,
        mint_a: ps.base_mint,
        mint_b: ps.quote_mint,
        vault_a: Some(ps.base_vault),
        vault_b: Some(ps.quote_vault),
        math: PoolMath::ConstantProduct {
            reserve_a: ps.virtual_base,
            reserve_b: ps.virtual_quote,
            fee_numerator: 100,      // 1% typical Bonk fee
            fee_denominator: 10000,
        },
        last_updated_slot: event.metadata.slot,
    })
}

pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < 8 + 264 { return None; }
    let ps = pool_state_decode(&data[8..])?;
    Some(PoolState {
        address: *address,
        dex_type: DexType::Bonk,
        mint_a: ps.base_mint,
        mint_b: ps.quote_mint,
        vault_a: Some(ps.base_vault),
        vault_b: Some(ps.quote_vault),
        math: PoolMath::ConstantProduct {
            reserve_a: ps.virtual_base,
            reserve_b: ps.virtual_quote,
            fee_numerator: 100,
            fee_denominator: 10000,
        },
        last_updated_slot: 0,
    })
}
```

**Step 6: Create Meteora DAMM v2 decoder stub**

Create `src/pool/decoder/meteora_damm_v2.rs`:
```rust
use solana_sdk::pubkey::Pubkey;
use crate::pool::state::{DexType, PoolMath, PoolState};

/// Meteora DAMM v2 pool state decoder
/// Note: The existing streamer has no account parser for Meteora DAMM v2.
/// We decode from raw bytes using known layout.
///
/// Meteora DAMM v2 pool layout (offset from byte 8, after discriminator):
/// Refer to: https://github.com/nicetomeetyou1/meteora-pool-parser
pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    // Meteora DAMM v2 pool account is large (~900+ bytes)
    if data.len() < 200 { return None; }

    // TODO: Implement exact byte offsets once Meteora IDL is verified
    // For now, extract mints and vault addresses from known positions
    // Layout: discriminator(8) + lp_mint(32) + token_a_mint(32) + token_b_mint(32)
    //         + a_vault(32) + b_vault(32) + ...

    let offset = 8; // Skip discriminator
    let _lp_mint = Pubkey::try_from(&data[offset..offset+32]).ok()?;
    let token_a_mint = Pubkey::try_from(&data[offset+32..offset+64]).ok()?;
    let token_b_mint = Pubkey::try_from(&data[offset+64..offset+96]).ok()?;
    let a_vault = Pubkey::try_from(&data[offset+96..offset+128]).ok()?;
    let b_vault = Pubkey::try_from(&data[offset+128..offset+160]).ok()?;

    Some(PoolState {
        address: *address,
        dex_type: DexType::MeteoraDammV2,
        mint_a: token_a_mint,
        mint_b: token_b_mint,
        vault_a: Some(a_vault),
        vault_b: Some(b_vault),
        math: PoolMath::ConstantProduct {
            reserve_a: 0, // Reserves in vault accounts
            reserve_b: 0,
            fee_numerator: 25,
            fee_denominator: 10000,
        },
        last_updated_slot: 0,
    })
}
```

**Step 7: Update pool mod.rs**

Modify `src/pool/mod.rs`:
```rust
pub mod cache;
pub mod decoder;
pub mod state;
```

**Step 8: Verify it compiles**

Run: `cd solana-streamer && cargo check`
Expected: compiles (may have warnings about unused imports — acceptable at this stage)

**Step 9: Commit**

```bash
git add src/pool/decoder/
git commit -m "feat(pool): add decoders for 5 ConstantProduct DEXes"
```

---

## Task 4: Build PumpFun and Raydium CLMM decoders

**Files:**
- Create: `solana-streamer/src/pool/decoder/pumpfun.rs`
- Create: `solana-streamer/src/pool/decoder/raydium_clmm.rs`
- Modify: `solana-streamer/src/pool/decoder/mod.rs`

**Step 1: Create PumpFun decoder**

Create `src/pool/decoder/pumpfun.rs`:
```rust
use solana_sdk::pubkey::Pubkey;
use crate::streaming::event_parser::protocols::pumpfun::PumpFunBondingCurveAccountEvent;
use crate::streaming::event_parser::protocols::pumpfun::types::bonding_curve_decode;
use crate::pool::state::{DexType, PoolMath, PoolState};

/// PumpFun bonding curve — reserves stored directly in pool state
pub fn decode(event: &PumpFunBondingCurveAccountEvent) -> Option<PoolState> {
    let bc = &event.bonding_curve;

    // Skip completed bonding curves (migrated to AMM)
    if bc.complete { return None; }

    Some(PoolState {
        address: event.pubkey,
        dex_type: DexType::PumpFun,
        mint_a: Pubkey::default(), // Token mint — needs to be resolved from trade events
        mint_b: solana_sdk::pubkey!("So11111111111111111111111111111111111111112"), // SOL
        vault_a: None, // PumpFun stores reserves directly
        vault_b: None,
        math: PoolMath::BondingCurve {
            virtual_token_reserves: bc.virtual_token_reserves,
            virtual_sol_reserves: bc.virtual_sol_reserves,
            real_token_reserves: bc.real_token_reserves,
            real_sol_reserves: bc.real_sol_reserves,
            complete: bc.complete,
        },
        last_updated_slot: event.metadata.slot,
    })
}

pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < 8 + 48 { return None; }
    let bc = bonding_curve_decode(&data[8..])?;
    if bc.complete { return None; }

    Some(PoolState {
        address: *address,
        dex_type: DexType::PumpFun,
        mint_a: Pubkey::default(),
        mint_b: solana_sdk::pubkey!("So11111111111111111111111111111111111111112"),
        vault_a: None,
        vault_b: None,
        math: PoolMath::BondingCurve {
            virtual_token_reserves: bc.virtual_token_reserves,
            virtual_sol_reserves: bc.virtual_sol_reserves,
            real_token_reserves: bc.real_token_reserves,
            real_sol_reserves: bc.real_sol_reserves,
            complete: bc.complete,
        },
        last_updated_slot: 0,
    })
}
```

**Step 2: Create Raydium CLMM decoder**

Create `src/pool/decoder/raydium_clmm.rs`:
```rust
use solana_sdk::pubkey::Pubkey;
use crate::streaming::event_parser::protocols::raydium_clmm::RaydiumClmmPoolStateAccountEvent;
use crate::streaming::event_parser::protocols::raydium_clmm::types::pool_state_decode;
use crate::pool::state::{DexType, PoolMath, PoolState};

pub fn decode(event: &RaydiumClmmPoolStateAccountEvent) -> Option<PoolState> {
    let ps = &event.pool_state;
    Some(PoolState {
        address: event.pubkey,
        dex_type: DexType::RaydiumClmm,
        mint_a: ps.token_mint0,
        mint_b: ps.token_mint1,
        vault_a: Some(ps.token_vault0),
        vault_b: Some(ps.token_vault1),
        math: PoolMath::Concentrated {
            sqrt_price_x64: ps.sqrt_price_x64,
            liquidity: ps.liquidity,
            tick_current: ps.tick_current,
            fee_rate: 0, // From amm_config, resolved separately
        },
        last_updated_slot: event.metadata.slot,
    })
}

pub fn decode_bytes(address: &Pubkey, data: &[u8]) -> Option<PoolState> {
    if data.len() < 8 + 1536 { return None; }
    let ps = pool_state_decode(&data[8..])?;
    Some(PoolState {
        address: *address,
        dex_type: DexType::RaydiumClmm,
        mint_a: ps.token_mint0,
        mint_b: ps.token_mint1,
        vault_a: Some(ps.token_vault0),
        vault_b: Some(ps.token_vault1),
        math: PoolMath::Concentrated {
            sqrt_price_x64: ps.sqrt_price_x64,
            liquidity: ps.liquidity,
            tick_current: ps.tick_current,
            fee_rate: 0,
        },
        last_updated_slot: 0,
    })
}
```

**Step 3: Update decoder mod.rs — add to dispatch**

Add to `pool_state_from_event` match:
```rust
DexEvent::PumpFunBondingCurveAccountEvent(e) => pumpfun::decode(e),
DexEvent::RaydiumClmmPoolStateAccountEvent(e) => raydium_clmm::decode(e),
```

Add to `pool_state_from_bytes` match:
```rust
DexType::PumpFun => pumpfun::decode_bytes(address, data),
DexType::RaydiumClmm => raydium_clmm::decode_bytes(address, data),
```

Add module declarations:
```rust
pub mod pumpfun;
pub mod raydium_clmm;
```

**Step 4: Verify it compiles**

Run: `cd solana-streamer && cargo check`
Expected: compiles

**Step 5: Commit**

```bash
git add src/pool/decoder/pumpfun.rs src/pool/decoder/raydium_clmm.rs src/pool/decoder/mod.rs
git commit -m "feat(pool): add PumpFun bonding curve and Raydium CLMM decoders"
```

---

## Task 5: Build pool discovery engine

**Files:**
- Create: `solana-streamer/src/pool/discovery.rs`
- Modify: `solana-streamer/src/pool/mod.rs`

This module extracts pool address + mints from transaction events (swap/create) and registers newly discovered pools.

**Step 1: Create discovery module**

Create `src/pool/discovery.rs`:
```rust
use solana_sdk::pubkey::Pubkey;
use crate::streaming::event_parser::DexEvent;
use super::state::DexType;

/// Info extracted from a transaction event for pool discovery
#[derive(Debug, Clone)]
pub struct DiscoveredPool {
    pub address: Pubkey,
    pub dex_type: DexType,
    pub mint_a: Option<Pubkey>,
    pub mint_b: Option<Pubkey>,
}

/// Extract pool discovery info from a DexEvent.
/// Returns None if the event is not useful for discovery (e.g., config events).
pub fn discover_pool(event: &DexEvent) -> Option<DiscoveredPool> {
    match event {
        // === Raydium AMM V4 ===
        DexEvent::RaydiumAmmV4SwapEvent(e) => Some(DiscoveredPool {
            address: e.amm,
            dex_type: DexType::RaydiumAmmV4,
            mint_a: None, // Mints resolved from account data
            mint_b: None,
        }),
        DexEvent::RaydiumAmmV4Initialize2Event(e) => Some(DiscoveredPool {
            address: e.amm,
            dex_type: DexType::RaydiumAmmV4,
            mint_a: None,
            mint_b: None,
        }),

        // === Raydium CPMM ===
        DexEvent::RaydiumCpmmSwapEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::RaydiumCpmm,
            mint_a: Some(e.input_token_mint),
            mint_b: Some(e.output_token_mint),
        }),
        DexEvent::RaydiumCpmmInitializeEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::RaydiumCpmm,
            mint_a: Some(e.token_0_mint),
            mint_b: Some(e.token_1_mint),
        }),

        // === Raydium CLMM ===
        DexEvent::RaydiumClmmSwapEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::RaydiumClmm,
            mint_a: None,
            mint_b: None,
        }),
        DexEvent::RaydiumClmmSwapV2Event(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::RaydiumClmm,
            mint_a: Some(e.input_vault_mint),
            mint_b: Some(e.output_vault_mint),
        }),
        DexEvent::RaydiumClmmCreatePoolEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::RaydiumClmm,
            mint_a: Some(e.token_mint0),
            mint_b: Some(e.token_mint1),
        }),

        // === PumpFun ===
        DexEvent::PumpFunTradeEvent(e) => Some(DiscoveredPool {
            address: e.bonding_curve,
            dex_type: DexType::PumpFun,
            mint_a: Some(e.mint),
            mint_b: Some(solana_sdk::pubkey!("So11111111111111111111111111111111111111112")),
        }),
        DexEvent::PumpFunCreateTokenEvent(e) => Some(DiscoveredPool {
            address: e.bonding_curve,
            dex_type: DexType::PumpFun,
            mint_a: Some(e.mint),
            mint_b: Some(solana_sdk::pubkey!("So11111111111111111111111111111111111111112")),
        }),

        // === PumpSwap ===
        DexEvent::PumpSwapBuyEvent(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::PumpSwap,
            mint_a: Some(e.base_mint),
            mint_b: Some(e.quote_mint),
        }),
        DexEvent::PumpSwapSellEvent(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::PumpSwap,
            mint_a: Some(e.base_mint),
            mint_b: Some(e.quote_mint),
        }),
        DexEvent::PumpSwapCreatePoolEvent(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::PumpSwap,
            mint_a: Some(e.base_mint),
            mint_b: Some(e.quote_mint),
        }),

        // === Bonk ===
        DexEvent::BonkTradeEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::Bonk,
            mint_a: Some(e.base_token_mint),
            mint_b: Some(e.quote_token_mint),
        }),
        DexEvent::BonkPoolCreateEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::Bonk,
            mint_a: None,
            mint_b: None,
        }),

        // === Meteora DAMM V2 ===
        DexEvent::MeteoraDammV2SwapEvent(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::MeteoraDammV2,
            mint_a: Some(e.token_a_mint),
            mint_b: Some(e.token_b_mint),
        }),
        DexEvent::MeteoraDammV2Swap2Event(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::MeteoraDammV2,
            mint_a: Some(e.token_a_mint),
            mint_b: Some(e.token_b_mint),
        }),
        DexEvent::MeteoraDammV2InitializePoolEvent(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::MeteoraDammV2,
            mint_a: Some(e.token_a_mint),
            mint_b: Some(e.token_b_mint),
        }),

        _ => None,
    }
}
```

**Step 2: Update pool mod.rs**

Modify `src/pool/mod.rs`:
```rust
pub mod cache;
pub mod decoder;
pub mod discovery;
pub mod state;
```

**Step 3: Verify it compiles**

Run: `cd solana-streamer && cargo check`
Expected: compiles. Some field names may need adjustment based on exact event struct fields — fix any compile errors by checking the actual event structs in `protocols/*/events.rs`.

**Step 4: Commit**

```bash
git add src/pool/discovery.rs src/pool/mod.rs
git commit -m "feat(pool): add pool discovery engine from tx events"
```

---

## Task 6: Build the arbitrage streamer integration

**Files:**
- Create: `solana-streamer/src/pool/streamer.rs`
- Modify: `solana-streamer/src/pool/mod.rs`

This is the main integration module that wires everything together: receives DexEvents, discovers pools, fetches initial state via RPC, updates cache, and manages dynamic gRPC account subscriptions.

**Step 1: Create streamer module**

Create `src/pool/streamer.rs`:
```rust
use std::sync::Arc;
use log::{info, warn, debug};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;

use crate::streaming::event_parser::DexEvent;
use crate::streaming::grpc::AccountPretty;
use super::cache::PoolStateCache;
use super::decoder;
use super::discovery::{self, DiscoveredPool};
use super::state::{DexType, PoolMath, PoolState, PoolUpdate};

/// Configuration for the pool streamer
pub struct PoolStreamerConfig {
    /// RPC endpoint for initial pool state fetch
    pub rpc_url: String,
    /// Channel buffer size for PoolUpdate events
    pub update_channel_size: usize,
}

impl Default for PoolStreamerConfig {
    fn default() -> Self {
        Self {
            rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
            update_channel_size: 4096,
        }
    }
}

/// Main pool streamer — integrates discovery, decoding, and caching
pub struct PoolStreamer {
    cache: Arc<PoolStateCache>,
    rpc: Arc<RpcClient>,
    /// Addresses pending subscription (accumulated between subscription updates)
    pending_subscriptions: Arc<tokio::sync::Mutex<Vec<String>>>,
}

impl PoolStreamer {
    pub fn new(config: PoolStreamerConfig) -> (Self, mpsc::Receiver<PoolUpdate>) {
        let (update_tx, update_rx) = mpsc::channel(config.update_channel_size);
        let cache = Arc::new(PoolStateCache::new(update_tx));
        let rpc = Arc::new(RpcClient::new(config.rpc_url));

        let streamer = Self {
            cache,
            rpc,
            pending_subscriptions: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        };

        (streamer, update_rx)
    }

    /// Get shared reference to the cache
    pub fn cache(&self) -> Arc<PoolStateCache> {
        self.cache.clone()
    }

    /// Process a DexEvent — called from the gRPC callback
    /// Handles both pool discovery (from tx events) and state updates (from account events)
    pub async fn on_event(&self, event: DexEvent) {
        // 1. Try pool discovery from transaction events
        if let Some(discovered) = discovery::discover_pool(&event) {
            self.handle_discovery(discovered).await;
        }

        // 2. Try direct pool state update from account events
        if let Some(pool_state) = decoder::pool_state_from_event(&event) {
            self.handle_account_update(pool_state);
        }

        // 3. Handle vault token account balance updates
        if let DexEvent::TokenAccountEvent(ref token_event) = event {
            self.handle_token_account_update(
                &token_event.pubkey,
                token_event.amount,
                token_event.metadata.slot,
            );
        }
    }

    /// Handle a newly discovered pool
    async fn handle_discovery(&self, discovered: DiscoveredPool) {
        // Skip if already tracked
        if self.cache.contains(&discovered.address) {
            return;
        }

        debug!("Discovered new pool: {} ({:?})", discovered.address, discovered.dex_type);

        // Fetch initial account data via RPC
        match self.rpc.get_account_data(&discovered.address).await {
            Ok(data) => {
                if let Some(mut pool_state) = decoder::pool_state_from_bytes(
                    discovered.dex_type,
                    &discovered.address,
                    &data,
                ) {
                    // Patch mints from discovery event if decoder couldn't extract them
                    if pool_state.mint_a == Pubkey::default() {
                        if let Some(mint) = discovered.mint_a {
                            pool_state.mint_a = mint;
                        }
                    }
                    if pool_state.mint_b == Pubkey::default() {
                        if let Some(mint) = discovered.mint_b {
                            pool_state.mint_b = mint;
                        }
                    }

                    // Queue vault accounts for subscription
                    let mut pending = self.pending_subscriptions.lock().await;
                    pending.push(discovered.address.to_string());
                    if let Some(vault_a) = &pool_state.vault_a {
                        pending.push(vault_a.to_string());
                    }
                    if let Some(vault_b) = &pool_state.vault_b {
                        pending.push(vault_b.to_string());
                    }

                    info!(
                        "Pool registered: {} {:?} ({} / {})",
                        pool_state.address, pool_state.dex_type, pool_state.mint_a, pool_state.mint_b
                    );

                    // For vault-based pools, also fetch initial vault balances
                    if let Some(vault_a) = pool_state.vault_a {
                        self.fetch_vault_balance(&vault_a, &pool_state.address, true).await;
                    }
                    if let Some(vault_b) = pool_state.vault_b {
                        self.fetch_vault_balance(&vault_b, &pool_state.address, false).await;
                    }

                    self.cache.insert(pool_state);
                }
            }
            Err(e) => {
                warn!("Failed to fetch pool {}: {}", discovered.address, e);
            }
        }
    }

    /// Fetch initial vault token account balance
    async fn fetch_vault_balance(&self, vault: &Pubkey, _pool: &Pubkey, is_vault_a: bool) {
        match self.rpc.get_account_data(vault).await {
            Ok(data) => {
                if data.len() >= 72 {
                    // SPL Token account: amount at offset 64
                    let balance = u64::from_le_bytes(data[64..72].try_into().unwrap_or([0; 8]));
                    self.cache.update_vault_balance(vault, balance, is_vault_a, 0);
                }
            }
            Err(e) => {
                debug!("Failed to fetch vault {}: {}", vault, e);
            }
        }
    }

    /// Handle a pool state account update (from gRPC account subscription)
    fn handle_account_update(&self, pool_state: PoolState) {
        if self.cache.contains(&pool_state.address) {
            // Update existing pool's math
            self.cache.update_math(
                &pool_state.address,
                pool_state.math,
                pool_state.last_updated_slot,
            );
        } else {
            // New pool discovered via account subscription
            self.cache.insert(pool_state);
        }
    }

    /// Handle token account balance change (vault update for ConstantProduct pools)
    fn handle_token_account_update(&self, token_account: &Pubkey, balance: u64, slot: u64) {
        if let Some(is_a) = self.cache.is_vault_a(token_account) {
            self.cache.update_vault_balance(token_account, balance, is_a, slot);
        }
    }

    /// Drain pending subscription addresses (call before update_subscription)
    pub async fn drain_pending_subscriptions(&self) -> Vec<String> {
        let mut pending = self.pending_subscriptions.lock().await;
        std::mem::take(&mut *pending)
    }

    /// Pool count
    pub fn pool_count(&self) -> usize {
        self.cache.len()
    }
}
```

**Step 2: Update pool mod.rs**

Modify `src/pool/mod.rs`:
```rust
pub mod cache;
pub mod decoder;
pub mod discovery;
pub mod state;
pub mod streamer;
```

**Step 3: Verify it compiles**

Run: `cd solana-streamer && cargo check`
Expected: compiles. Fix any field name mismatches with actual event structs.

**Step 4: Commit**

```bash
git add src/pool/streamer.rs src/pool/mod.rs
git commit -m "feat(pool): add PoolStreamer integration (discovery + decode + cache)"
```

---

## Task 7: Create example program

**Files:**
- Create: `solana-streamer/examples/pool_streamer_example.rs`

**Step 1: Create example**

Create `examples/pool_streamer_example.rs`:
```rust
use std::sync::Arc;
use solana_streamer_sdk::pool::streamer::{PoolStreamer, PoolStreamerConfig};
use solana_streamer_sdk::pool::state::PoolMath;
use solana_streamer_sdk::streaming::yellowstone_grpc::YellowstoneGrpc;
use solana_streamer_sdk::streaming::event_parser::common::types::Protocol;
use solana_streamer_sdk::streaming::common::subscription::{TransactionFilter, AccountFilter};
use log::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Configuration
    let grpc_endpoint = std::env::var("GRPC_ENDPOINT")
        .unwrap_or_else(|_| "https://your-grpc-endpoint:443".to_string());
    let grpc_token = std::env::var("GRPC_TOKEN").ok();
    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

    // Create pool streamer
    let config = PoolStreamerConfig {
        rpc_url,
        update_channel_size: 4096,
    };
    let (pool_streamer, mut update_rx) = PoolStreamer::new(config);
    let pool_streamer = Arc::new(pool_streamer);

    // Spawn update consumer (this is what Stage 2 would consume)
    tokio::spawn(async move {
        while let Some(update) = update_rx.recv().await {
            match &update.math {
                PoolMath::ConstantProduct { reserve_a, reserve_b, fee_numerator, fee_denominator } => {
                    info!(
                        "[slot {}] Pool {} ({:?}) ConstantProduct: reserve_a={}, reserve_b={}, fee={}/{}",
                        update.slot, update.pool_address, update.dex_type,
                        reserve_a, reserve_b, fee_numerator, fee_denominator
                    );
                }
                PoolMath::BondingCurve { virtual_token_reserves, virtual_sol_reserves, .. } => {
                    info!(
                        "[slot {}] Pool {} ({:?}) BondingCurve: token={}, sol={}",
                        update.slot, update.pool_address, update.dex_type,
                        virtual_token_reserves, virtual_sol_reserves
                    );
                }
                PoolMath::Concentrated { sqrt_price_x64, liquidity, tick_current, .. } => {
                    info!(
                        "[slot {}] Pool {} ({:?}) CLMM: sqrt_price={}, liquidity={}, tick={}",
                        update.slot, update.pool_address, update.dex_type,
                        sqrt_price_x64, liquidity, tick_current
                    );
                }
            }
        }
    });

    // Create gRPC client
    let grpc = YellowstoneGrpc::new(&grpc_endpoint, grpc_token.as_deref())?;

    // Subscribe to all 7 DEX protocols for transaction events (pool discovery)
    let protocols = vec![
        Protocol::PumpFun,
        Protocol::PumpSwap,
        Protocol::Bonk,
        Protocol::RaydiumCpmm,
        Protocol::RaydiumClmm,
        Protocol::RaydiumAmmV4,
        Protocol::MeteoraDammV2,
    ];

    let transaction_filter = vec![TransactionFilter {
        account_include: vec![],  // Protocols list handles filtering
        account_exclude: vec![],
        account_required: vec![],
    }];

    let account_filter = vec![]; // Start empty, dynamically added

    let streamer = pool_streamer.clone();
    grpc.subscribe_events_immediate(
        protocols,
        None,
        transaction_filter,
        account_filter,
        None, // No event type filter — we want all events
        None,
        move |event| {
            let streamer = streamer.clone();
            // on_event is async but callback is sync — spawn task
            tokio::spawn(async move {
                streamer.on_event(event).await;
            });
        },
    ).await?;

    // Periodically update gRPC subscription with newly discovered pool/vault accounts
    let streamer_ref = pool_streamer.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
            let new_accounts = streamer_ref.drain_pending_subscriptions().await;
            if !new_accounts.is_empty() {
                info!("Adding {} accounts to subscription, total pools: {}",
                    new_accounts.len(), streamer_ref.pool_count());
                // Note: update_subscription replaces filters, so we need to
                // include ALL previously subscribed accounts.
                // For production, maintain a cumulative account list.
                // This example just demonstrates the flow.
            }
        }
    });

    info!("Pool streamer running. Discovering pools from transaction events...");

    // Keep running
    tokio::signal::ctrl_c().await?;
    info!("Shutting down. Discovered {} pools total.", pool_streamer.pool_count());

    Ok(())
}
```

**Step 2: Verify it compiles**

Run: `cd solana-streamer && cargo check --example pool_streamer_example`
Expected: compiles (won't run without a real gRPC endpoint)

**Step 3: Commit**

```bash
git add examples/pool_streamer_example.rs
git commit -m "feat(pool): add pool_streamer_example demonstrating full data flow"
```

---

## Task 8: Fix compilation issues and integration test

**Files:**
- Various files from Tasks 1-7

**Step 1: Run full cargo check and fix all errors**

Run: `cd solana-streamer && cargo check 2>&1`

Common issues to fix:
- Event struct field names that don't match (check each protocol's `events.rs`)
- Missing trait implementations (Clone, Debug)
- Module visibility (pub vs pub(crate))
- Import paths for protocol types

**Step 2: Fix each error systematically**

For each compile error, check the exact field name in the protocol's events.rs and types.rs, then update the decoder or discovery module.

**Step 3: Run cargo check on example too**

Run: `cd solana-streamer && cargo check --example pool_streamer_example`

**Step 4: Run existing tests to ensure no regressions**

Run: `cd solana-streamer && cargo test`
Expected: all existing tests pass (we only added new modules, didn't modify existing code)

**Step 5: Commit**

```bash
git add -A
git commit -m "fix: resolve compilation issues across pool module"
```

---

## Task Summary

| Task | Description | Est. Time |
|------|-------------|-----------|
| 1 | PoolMath, PoolState, PoolUpdate structs | 5 min |
| 2 | PoolStateCache with vault reverse index | 5 min |
| 3 | 5 ConstantProduct DEX decoders | 10 min |
| 4 | PumpFun + Raydium CLMM decoders | 5 min |
| 5 | Pool discovery engine (tx event → pool address) | 5 min |
| 6 | PoolStreamer integration (wire everything) | 10 min |
| 7 | Example program | 5 min |
| 8 | Fix compilation + integration test | 10 min |

**Total: ~55 min**

## Key Design Notes for Implementer

1. **update_subscription() REPLACES filters, doesn't merge.** You must maintain a cumulative list of all subscribed account addresses and pass the full list each time.

2. **Vault-based pools need TWO subscriptions**: the pool account (for mints, fees) AND the vault token accounts (for reserves/balances). Token account balance is at byte offset 64 (u64 LE) for both SPL Token and Token-2022.

3. **PumpFun bonding curve doesn't store the token mint.** It must be resolved from the trade event's `mint` field and patched into the PoolState.

4. **Meteora DAMM v2 has no existing account parser** in solana-streamer. The byte layout in the decoder is approximate — verify against the actual IDL during implementation.

5. **The callback in subscribe_events_immediate is sync** (`Fn`, not `async Fn`). Use `tokio::spawn` to bridge to async code (like RPC calls in pool discovery).

6. **Existing code is NOT modified** except adding `pub mod pool;` to lib.rs. All new code is additive.
