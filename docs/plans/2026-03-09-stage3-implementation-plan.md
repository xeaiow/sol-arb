# Stage 3: Executor Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Build the on-chain Pinocchio arbitrage program and off-chain executor that consumes Opportunity from Stage 2, builds atomic arbitrage transactions, and submits via Jito/Flashblock/Astralane.

**Architecture:** Two independent crates: `program/` (on-chain Pinocchio, uses dex-pinocchio-cpi for CPI) and `executor/` (off-chain, builds transactions + multi-channel submission). The on-chain program executes multi-hop swaps and reverts if profit < min_profit. The executor builds two tx variants (Jito bundle / SWQoS) and submits all enabled channels concurrently.

**Tech Stack:** Rust, pinocchio 0.10, dex-pinocchio-cpi (path dep), solana-sdk 3.0.0, tonic (Jito gRPC), reqwest (HTTP), toml + serde (config), tokio

---

## Task 1: Create program crate skeleton

**Files:**
- Create: `program/Cargo.toml`
- Create: `program/src/lib.rs`

**Step 1: Create Cargo.toml**

Create `program/Cargo.toml`:
```toml
[package]
name = "arb-program"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "lib"]

[dependencies]
pinocchio = "0.10"
dex-pinocchio-cpi = { path = "../dex-pinocchio-cpi" }
five8_const = "0.1"

[features]
default = []
no-entrypoint = []
```

**Step 2: Create lib.rs with entrypoint + instruction dispatch**

Create `program/src/lib.rs`:
```rust
#![no_std]

use pinocchio::{
    account_info::AccountInfo,
    entrypoint,
    program_error::ProgramError,
    pubkey::Pubkey,
    ProgramResult,
};

mod accounts;
mod swap;

entrypoint!(process_instruction);

/// Instruction discriminators
const SWAP_2HOP: u8 = 0;
const SWAP_3HOP: u8 = 1;
const SWAP_4HOP: u8 = 2;

fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    if instruction_data.is_empty() {
        return Err(ProgramError::InvalidInstructionData);
    }

    match instruction_data[0] {
        SWAP_2HOP => swap::process_swap(accounts, instruction_data, 2),
        SWAP_3HOP => swap::process_swap(accounts, instruction_data, 3),
        SWAP_4HOP => swap::process_swap(accounts, instruction_data, 4),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}
```

**Step 3: Create placeholder modules**

Create `program/src/accounts.rs`:
```rust
//! Account parsing helpers — implemented in Task 2
```

Create `program/src/swap.rs`:
```rust
use pinocchio::{account_info::AccountInfo, program_error::ProgramError, ProgramResult};

/// Process a multi-hop swap. Implemented in Task 3.
pub fn process_swap(
    _accounts: &[AccountInfo],
    _data: &[u8],
    _hop_count: u8,
) -> ProgramResult {
    Err(ProgramError::InvalidInstructionData) // placeholder
}
```

**Step 4: Verify it compiles**

Run: `cd program && cargo check`

**Step 5: Commit**

```bash
git add program/
git commit -m "feat(program): add on-chain arbitrage program crate skeleton"
```

---

## Task 2: Instruction parsing and account layout

**Files:**
- Modify: `program/src/accounts.rs`

**Context:**
- 2-hop instruction: 16 bytes (discriminator + 2 dex_types + flags + amount_in + min_profit)
- 3-hop instruction: 18 bytes (adds mid_dex_type + mid_flags)
- 4-hop instruction: 20 bytes (adds mid1 + mid2 dex/flags)
- Account layout: fixed header (8) + intermediate tokens (3 each) + optional flashloan (3) + per-hop pool accounts

**Step 1: Implement instruction parsing and account layout**

Replace `program/src/accounts.rs`:
```rust
use pinocchio::{account_info::AccountInfo, program_error::ProgramError};

/// DEX type IDs (must match off-chain DexType enum)
#[derive(Clone, Copy)]
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

impl DexType {
    pub fn from_u8(v: u8) -> Result<Self, ProgramError> {
        match v {
            0 => Ok(Self::RaydiumAmmV4),
            1 => Ok(Self::RaydiumCpmm),
            2 => Ok(Self::RaydiumClmm),
            3 => Ok(Self::PumpFun),
            4 => Ok(Self::PumpSwap),
            5 => Ok(Self::Bonk),
            6 => Ok(Self::MeteoraDammV2),
            _ => Err(ProgramError::InvalidInstructionData),
        }
    }

    /// Number of accounts required for CPI swap on this DEX
    pub fn pool_account_count(&self) -> usize {
        match self {
            DexType::RaydiumAmmV4 => 8,
            DexType::RaydiumCpmm => 13,
            DexType::RaydiumClmm => 14,
            DexType::PumpFun => 16,   // buy (sell = 14, handled in swap)
            DexType::PumpSwap => 23,  // buy (sell = similar)
            DexType::Bonk => 17,
            DexType::MeteoraDammV2 => 14,
        }
    }
}

/// Parsed hop info
pub struct HopInfo {
    pub dex_type: DexType,
    pub is_a_to_b: bool,
    pub is_token_2022: bool,
}

/// Parsed instruction data
pub struct SwapInstruction {
    pub hop_count: u8,
    pub hops: [HopInfo; 4],   // max 4 hops, only hop_count used
    pub use_flashloan: bool,
    pub amount_in: u64,
    pub min_profit: u32,
}

impl SwapInstruction {
    pub fn parse(data: &[u8], hop_count: u8) -> Result<Self, ProgramError> {
        // Minimum sizes: 2-hop=16, 3-hop=18, 4-hop=20
        let min_len = 12 + (hop_count as usize) * 2;
        if data.len() < min_len {
            return Err(ProgramError::InvalidInstructionData);
        }

        // Byte layout:
        // [0] discriminator (already consumed)
        // [1] hop0_dex_type
        // [2] hop1_dex_type (last hop for 2-hop)
        // [3] flags0: bit0=hop0_a_to_b, bit1=hop1_a_to_b, bit2=hop0_2022, bit3=hop1_2022, bit4=flashloan
        // For 3-hop: [4] mid_dex_type, [5] mid_flags (bit0=a_to_b, bit1=2022)
        // For 4-hop: [4] mid1_dex, [5] mid1_flags, [6] mid2_dex, [7] mid2_flags
        // Last 12 bytes: amount_in(8) + min_profit(4)

        let buy_dex = DexType::from_u8(data[1])?;
        let sell_dex = DexType::from_u8(data[2])?;
        let flags = data[3];
        let use_flashloan = (flags >> 4) & 1 == 1;

        let mut hops = [
            HopInfo { dex_type: DexType::RaydiumAmmV4, is_a_to_b: false, is_token_2022: false },
            HopInfo { dex_type: DexType::RaydiumAmmV4, is_a_to_b: false, is_token_2022: false },
            HopInfo { dex_type: DexType::RaydiumAmmV4, is_a_to_b: false, is_token_2022: false },
            HopInfo { dex_type: DexType::RaydiumAmmV4, is_a_to_b: false, is_token_2022: false },
        ];

        // First hop (buy)
        hops[0] = HopInfo {
            dex_type: buy_dex,
            is_a_to_b: flags & 1 == 1,
            is_token_2022: (flags >> 2) & 1 == 1,
        };

        let amount_offset = match hop_count {
            2 => {
                // Last hop (sell)
                hops[1] = HopInfo {
                    dex_type: sell_dex,
                    is_a_to_b: (flags >> 1) & 1 == 1,
                    is_token_2022: (flags >> 3) & 1 == 1,
                };
                4 // amount_in starts at byte 4
            }
            3 => {
                let mid_dex = DexType::from_u8(data[4])?;
                let mid_flags = data[5];
                hops[1] = HopInfo {
                    dex_type: mid_dex,
                    is_a_to_b: mid_flags & 1 == 1,
                    is_token_2022: (mid_flags >> 1) & 1 == 1,
                };
                hops[2] = HopInfo {
                    dex_type: sell_dex,
                    is_a_to_b: (flags >> 1) & 1 == 1,
                    is_token_2022: (flags >> 3) & 1 == 1,
                };
                6
            }
            4 => {
                let mid1_dex = DexType::from_u8(data[4])?;
                let mid1_flags = data[5];
                let mid2_dex = DexType::from_u8(data[6])?;
                let mid2_flags = data[7];
                hops[1] = HopInfo {
                    dex_type: mid1_dex,
                    is_a_to_b: mid1_flags & 1 == 1,
                    is_token_2022: (mid1_flags >> 1) & 1 == 1,
                };
                hops[2] = HopInfo {
                    dex_type: mid2_dex,
                    is_a_to_b: mid2_flags & 1 == 1,
                    is_token_2022: (mid2_flags >> 1) & 1 == 1,
                };
                hops[3] = HopInfo {
                    dex_type: sell_dex,
                    is_a_to_b: (flags >> 1) & 1 == 1,
                    is_token_2022: (flags >> 3) & 1 == 1,
                };
                8
            }
            _ => return Err(ProgramError::InvalidInstructionData),
        };

        let amount_in = u64::from_le_bytes(
            data[amount_offset..amount_offset + 8]
                .try_into()
                .map_err(|_| ProgramError::InvalidInstructionData)?,
        );
        let min_profit = u32::from_le_bytes(
            data[amount_offset + 8..amount_offset + 12]
                .try_into()
                .map_err(|_| ProgramError::InvalidInstructionData)?,
        );

        Ok(SwapInstruction {
            hop_count,
            hops,
            use_flashloan,
            amount_in,
            min_profit,
        })
    }
}

/// Header account indices
pub const HEADER_SIZE: usize = 8;
pub const ACCT_PAYER: usize = 0;
pub const ACCT_BASE_MINT: usize = 1;
pub const ACCT_USER_BASE_ATA: usize = 2;
pub const ACCT_FEE_COLLECTOR: usize = 3;
pub const ACCT_SPL_TOKEN: usize = 4;
pub const ACCT_TOKEN_2022: usize = 5;
pub const ACCT_ATA_PROGRAM: usize = 6;
pub const ACCT_SYSTEM: usize = 7;

/// Per-intermediate-token accounts (3 each)
pub const INTERMEDIATE_ACCOUNTS_PER_TOKEN: usize = 3;

/// Flashloan accounts (3)
pub const FLASHLOAN_ACCOUNT_COUNT: usize = 3;

/// Calculate the starting index for pool accounts for a given hop
pub fn pool_accounts_start(
    hop_count: u8,
    use_flashloan: bool,
    hop_index: u8,
    hops: &[HopInfo],
) -> usize {
    let mut offset = HEADER_SIZE;

    // Intermediate token accounts: (hop_count - 1) × 3
    offset += (hop_count as usize - 1) * INTERMEDIATE_ACCOUNTS_PER_TOKEN;

    // Flashloan accounts
    if use_flashloan {
        offset += FLASHLOAN_ACCOUNT_COUNT;
    }

    // Pool accounts for previous hops
    for i in 0..hop_index as usize {
        offset += hops[i].dex_type.pool_account_count();
    }

    offset
}
```

**Step 2: Verify it compiles**

Run: `cd program && cargo check`

**Step 3: Commit**

```bash
git add program/src/accounts.rs
git commit -m "feat(program): add instruction parsing and account layout helpers"
```

---

## Task 3: DEX swap CPI dispatch

**Files:**
- Create: `program/src/swap.rs` (replace placeholder)

**Context:**
- dex-pinocchio-cpi module names: `raydium_amm`, `raydium_cp`, `raydium_clmm`, `pump_fun`, `pump_fun_amm`, `bonkswap`, `meteora_damm_v2`
- Each DEX has different account structs and arg structs
- The swap function reads initial balance, executes CPI swaps, verifies profit

**Step 1: Implement swap dispatch**

Replace `program/src/swap.rs`:
```rust
use pinocchio::{
    account_info::AccountInfo,
    program_error::ProgramError,
    ProgramResult,
};

use crate::accounts::{
    DexType, SwapInstruction, HopInfo,
    ACCT_USER_BASE_ATA, pool_accounts_start,
};

/// Process a multi-hop swap
pub fn process_swap(
    accounts: &[AccountInfo],
    data: &[u8],
    hop_count: u8,
) -> ProgramResult {
    let ix = SwapInstruction::parse(data, hop_count)?;

    // Read initial balance of user base token ATA
    let user_base_ata = &accounts[ACCT_USER_BASE_ATA];
    let initial_balance = read_token_balance(user_base_ata)?;

    // TODO Task 4: flashloan borrow if enabled

    // Execute each hop
    for i in 0..ix.hop_count {
        let hop = &ix.hops[i as usize];
        let pool_start = pool_accounts_start(
            ix.hop_count,
            ix.use_flashloan,
            i,
            &ix.hops,
        );
        let pool_end = pool_start + hop.dex_type.pool_account_count();

        if pool_end > accounts.len() {
            return Err(ProgramError::NotEnoughAccountKeys);
        }

        let pool_accounts = &accounts[pool_start..pool_end];
        dispatch_swap(hop, pool_accounts, ix.amount_in, i == 0)?;
    }

    // TODO Task 4: flashloan repay if enabled

    // Verify profit
    let final_balance = read_token_balance(user_base_ata)?;
    let profit = final_balance
        .checked_sub(initial_balance)
        .ok_or(ProgramError::ArithmeticOverflow)?;

    if profit < ix.min_profit as u64 {
        return Err(ProgramError::Custom(1)); // Insufficient profit
    }

    Ok(())
}

/// Dispatch CPI swap to the correct DEX
fn dispatch_swap(
    hop: &HopInfo,
    pool_accounts: &[AccountInfo],
    _amount_in: u64,
    _is_first_hop: bool,
) -> ProgramResult {
    match hop.dex_type {
        DexType::RaydiumAmmV4 => swap_raydium_amm(pool_accounts, hop),
        DexType::RaydiumCpmm => swap_raydium_cp(pool_accounts, hop),
        DexType::RaydiumClmm => swap_raydium_clmm(pool_accounts, hop),
        DexType::PumpFun => swap_pumpfun(pool_accounts, hop),
        DexType::PumpSwap => swap_pumpswap(pool_accounts, hop),
        DexType::Bonk => swap_bonkswap(pool_accounts, hop),
        DexType::MeteoraDammV2 => swap_meteora_damm_v2(pool_accounts, hop),
    }
}

/// Read SPL token account balance (offset 64, u64 LE)
fn read_token_balance(account: &AccountInfo) -> Result<u64, ProgramError> {
    let data = account.try_borrow_data()?;
    if data.len() < 72 {
        return Err(ProgramError::InvalidAccountData);
    }
    Ok(u64::from_le_bytes(
        data[64..72]
            .try_into()
            .map_err(|_| ProgramError::InvalidAccountData)?,
    ))
}

// ── Per-DEX CPI wrappers ──
// Each function maps pool_accounts[] to the DEX's expected account struct
// and invokes the CPI. Uses dex-pinocchio-cpi functions directly.

fn swap_raydium_amm(accounts: &[AccountInfo], _hop: &HopInfo) -> ProgramResult {
    // accounts[0..8] maps to SwapBaseInV2Accounts
    // Raydium AMM uses 1-byte discriminator (16)
    let swap_accounts = dex_pinocchio_cpi::raydium_amm::SwapBaseInV2Accounts {
        token_program: accounts[0].to_account_view(),
        amm: accounts[1].to_account_view(),
        amm_authority: accounts[2].to_account_view(),
        amm_coin_vault: accounts[3].to_account_view(),
        amm_pc_vault: accounts[4].to_account_view(),
        user_source: accounts[5].to_account_view(),
        user_destination: accounts[6].to_account_view(),
        user_owner: accounts[7].to_account_view(),
    };
    let args = dex_pinocchio_cpi::raydium_amm::SwapBaseInV2Args {
        amount_in: 0,             // filled from token account delta
        minimum_amount_out: 0,    // no slippage check (profit check at end)
    };
    dex_pinocchio_cpi::raydium_amm::swap_base_in_v2(&swap_accounts, &args, &[])
}

fn swap_raydium_cp(accounts: &[AccountInfo], _hop: &HopInfo) -> ProgramResult {
    // accounts[0..13] maps to SwapBaseInputAccounts
    let swap_accounts = dex_pinocchio_cpi::raydium_cp::SwapBaseInputAccounts {
        payer: accounts[0].to_account_view(),
        authority: accounts[1].to_account_view(),
        amm_config: accounts[2].to_account_view(),
        pool_state: accounts[3].to_account_view(),
        input_token_account: accounts[4].to_account_view(),
        output_token_account: accounts[5].to_account_view(),
        input_vault: accounts[6].to_account_view(),
        output_vault: accounts[7].to_account_view(),
        input_token_program: accounts[8].to_account_view(),
        output_token_program: accounts[9].to_account_view(),
        input_token_mint: accounts[10].to_account_view(),
        output_token_mint: accounts[11].to_account_view(),
        observation_state: accounts[12].to_account_view(),
    };
    let args = dex_pinocchio_cpi::raydium_cp::SwapBaseInputArgs {
        amount_in: 0,
        minimum_amount_out: 0,
    };
    dex_pinocchio_cpi::raydium_cp::swap_base_input(&swap_accounts, &args, &[])
}

fn swap_raydium_clmm(accounts: &[AccountInfo], _hop: &HopInfo) -> ProgramResult {
    // accounts[0..14] maps to SwapAccounts
    let swap_accounts = dex_pinocchio_cpi::raydium_clmm::SwapAccounts {
        pool_authority: accounts[0].to_account_view(),
        pool: accounts[1].to_account_view(),
        input_token_account: accounts[2].to_account_view(),
        output_token_account: accounts[3].to_account_view(),
        token_a_vault: accounts[4].to_account_view(),
        token_b_vault: accounts[5].to_account_view(),
        token_a_mint: accounts[6].to_account_view(),
        token_b_mint: accounts[7].to_account_view(),
        payer: accounts[8].to_account_view(),
        token_a_program: accounts[9].to_account_view(),
        token_b_program: accounts[10].to_account_view(),
        referral_token_account: accounts[11].to_account_view(),
        event_authority: accounts[12].to_account_view(),
        program: accounts[13].to_account_view(),
    };
    let args = dex_pinocchio_cpi::raydium_clmm::SwapArgs {
        amount: 0,
        other_amount_threshold: 0,
        sqrt_price_limit_x64: 0,
        is_base_input: true,
    };
    dex_pinocchio_cpi::raydium_clmm::swap(&swap_accounts, &args, &[])
}

fn swap_pumpfun(_accounts: &[AccountInfo], _hop: &HopInfo) -> ProgramResult {
    // PumpFun has buy (16 accounts) and sell (14 accounts)
    // Direction determined by hop.is_a_to_b
    // Implementation deferred — needs buy/sell branching
    Err(ProgramError::InvalidInstructionData) // TODO: implement
}

fn swap_pumpswap(_accounts: &[AccountInfo], _hop: &HopInfo) -> ProgramResult {
    // PumpSwap (pump_fun_amm) has buy (23 accounts) and sell
    // Implementation deferred — needs buy/sell branching
    Err(ProgramError::InvalidInstructionData) // TODO: implement
}

fn swap_bonkswap(_accounts: &[AccountInfo], _hop: &HopInfo) -> ProgramResult {
    // Bonkswap swap (17 accounts)
    Err(ProgramError::InvalidInstructionData) // TODO: implement
}

fn swap_meteora_damm_v2(_accounts: &[AccountInfo], _hop: &HopInfo) -> ProgramResult {
    // Meteora DAMM V2 swap (14 accounts)
    Err(ProgramError::InvalidInstructionData) // TODO: implement
}
```

**Note:** The CPI wrappers above are **scaffolds**. The actual `to_account_view()` conversion and amount handling depend on Pinocchio's `AccountInfo` ↔ `AccountView` bridging. The implementer must:
1. Check if `AccountInfo` has a `to_account_view()` method or if manual conversion is needed
2. Wire `amount_in` correctly: first hop uses `ix.amount_in`, subsequent hops use the output of the previous hop (read from intermediate token account)
3. Handle buy/sell direction for PumpFun and PumpSwap

**Step 2: Verify it compiles**

Run: `cd program && cargo check`

Fix any Pinocchio API mismatches (AccountInfo vs AccountView conversion).

**Step 3: Commit**

```bash
git add program/src/swap.rs
git commit -m "feat(program): add DEX swap CPI dispatch with 7 DEX scaffolds"
```

---

## Task 4: Flashloan support (MarginFi)

**Files:**
- Create: `program/src/flashloan.rs`
- Modify: `program/src/swap.rs` (wire flashloan into process_swap)

**Step 1: Create flashloan.rs**

Create `program/src/flashloan.rs`:
```rust
use pinocchio::{
    account_info::AccountInfo,
    program_error::ProgramError,
    ProgramResult,
};

/// MarginFi flash borrow
/// Accounts: [marginfi_program, bank, bank_vault]
pub fn flash_borrow(
    accounts: &[AccountInfo],
    amount: u64,
) -> ProgramResult {
    if accounts.len() < 3 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    // CPI to MarginFi flash_borrow
    // Discriminator and account layout depend on MarginFi's actual program interface
    // This is a scaffold — implementer must check MarginFi's IDL
    let _ = (accounts, amount);
    Ok(()) // TODO: implement actual CPI
}

/// MarginFi flash repay
/// Accounts: [marginfi_program, bank, bank_vault]
pub fn flash_repay(
    accounts: &[AccountInfo],
    amount: u64,
) -> ProgramResult {
    if accounts.len() < 3 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let _ = (accounts, amount);
    Ok(()) // TODO: implement actual CPI
}
```

**Step 2: Wire flashloan into swap.rs**

In `program/src/swap.rs`, replace the two TODO comments with:

```rust
// After initial_balance read:
if ix.use_flashloan {
    let fl_start = HEADER_SIZE + (ix.hop_count as usize - 1) * INTERMEDIATE_ACCOUNTS_PER_TOKEN;
    let fl_accounts = &accounts[fl_start..fl_start + FLASHLOAN_ACCOUNT_COUNT];
    crate::flashloan::flash_borrow(fl_accounts, ix.amount_in)?;
}

// After all hops, before profit check:
if ix.use_flashloan {
    let fl_start = HEADER_SIZE + (ix.hop_count as usize - 1) * INTERMEDIATE_ACCOUNTS_PER_TOKEN;
    let fl_accounts = &accounts[fl_start..fl_start + FLASHLOAN_ACCOUNT_COUNT];
    crate::flashloan::flash_repay(fl_accounts, ix.amount_in)?;
}
```

Add to `program/src/lib.rs`:
```rust
mod flashloan;
```

And add imports to swap.rs:
```rust
use crate::accounts::{INTERMEDIATE_ACCOUNTS_PER_TOKEN, FLASHLOAN_ACCOUNT_COUNT};
```

**Step 3: Verify it compiles**

Run: `cd program && cargo check`

**Step 4: Commit**

```bash
git add program/src/flashloan.rs program/src/swap.rs program/src/lib.rs
git commit -m "feat(program): add MarginFi flashloan borrow/repay scaffold"
```

---

## Task 5: Create executor crate skeleton with config

**Files:**
- Create: `executor/Cargo.toml`
- Create: `executor/src/lib.rs`
- Create: `executor/src/config.rs`
- Create: `executor/config.toml` (example config)

**Step 1: Create Cargo.toml**

Create `executor/Cargo.toml`:
```toml
[package]
name = "arb-executor"
version = "0.1.0"
edition = "2021"

[dependencies]
arb-engine = { path = "../engine" }
solana-streamer-sdk = { path = "../solana-streamer" }
solana-sdk = "3.0.0"
solana-client = "3.0.0"
tokio = { version = "1.50.0", features = ["full"] }
tonic = "0.12"
reqwest = { version = "0.12", features = ["json"] }
serde = { version = "1.0", features = ["derive"] }
toml = "0.8"
log = "0.4"
env_logger = "0.11"
anyhow = "1.0"
rand = "0.8"
bs58 = "0.5"
base64 = "0.22"
```

**Step 2: Create config.rs**

Create `executor/src/config.rs`:
```rust
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct ExecutorConfigFile {
    pub executor: ExecutorConfig,
    pub jito: Option<JitoConfig>,
    pub flashblock: Option<FlashblockConfig>,
    pub astralane: Option<AstralaneConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ExecutorConfig {
    pub flashloan_enabled: bool,
    pub program_id: String,
    pub alt_address: String,
    pub anti_fingerprint: AntiFingerprint,
}

#[derive(Debug, Deserialize)]
pub struct AntiFingerprint {
    pub cu_jitter_range: u32,
    pub fee_collectors_sol: Vec<String>,
    pub fee_collector_usdc: String,
}

#[derive(Debug, Deserialize)]
pub struct JitoConfig {
    pub enabled: bool,
    pub block_engine_urls: Vec<String>,
    pub tip_percentage: u32,
    pub min_tip_lamports: u64,
    pub min_operator_profit_lamports: u64,
}

#[derive(Debug, Deserialize)]
pub struct FlashblockConfig {
    pub enabled: bool,
    pub api_key: String,
    pub cu_price_percentage: u32,
    pub endpoints: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AstralaneConfig {
    pub enabled: bool,
    pub api_key: String,
    pub cu_price_percentage: u32,
    pub endpoints: Vec<String>,
}

impl ExecutorConfigFile {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = toml::from_str(&content)?;
        Ok(config)
    }
}
```

**Step 3: Create example config.toml**

Create `executor/config.toml`:
```toml
[executor]
flashloan_enabled = false
program_id = "11111111111111111111111111111111"
alt_address = "11111111111111111111111111111111"

[executor.anti_fingerprint]
cu_jitter_range = 1000
fee_collectors_sol = [
    "11111111111111111111111111111111",
    "11111111111111111111111111111111",
    "11111111111111111111111111111111",
]
fee_collector_usdc = "11111111111111111111111111111111"

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
api_key = ""
cu_price_percentage = 30
endpoints = [
    "https://fra.flashblock.trade",
    "https://ams.flashblock.trade",
    "https://nyc.flashblock.trade",
    "https://tok.flashblock.trade",
]

[astralane]
enabled = true
api_key = ""
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

**Step 4: Create lib.rs**

Create `executor/src/lib.rs`:
```rust
pub mod config;
```

**Step 5: Verify it compiles**

Run: `cd executor && cargo check`

**Step 6: Commit**

```bash
git add executor/
git commit -m "feat(executor): add crate skeleton with config.toml parsing"
```

---

## Task 6: Transaction builder

**Files:**
- Create: `executor/src/tx_builder.rs`
- Create: `executor/src/anti_fp.rs`
- Create: `executor/src/alt.rs`
- Modify: `executor/src/lib.rs`

**Step 1: Create anti_fp.rs**

Create `executor/src/anti_fp.rs`:
```rust
use rand::Rng;
use solana_sdk::pubkey::Pubkey;

/// Jito tip accounts (8 official addresses)
pub const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4bVqkfRtQ7NmXwkiNPLz4xG",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSLo4G9hp12gJZTm1Xw",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSga2WWP4N4G2Cj6ixc",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL6JR3",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// Pick a random Jito tip account
pub fn random_tip_account() -> Pubkey {
    let mut rng = rand::thread_rng();
    let idx = rng.gen_range(0..JITO_TIP_ACCOUNTS.len());
    JITO_TIP_ACCOUNTS[idx].parse().unwrap()
}

/// Pick a random fee collector from config
pub fn random_fee_collector(collectors: &[Pubkey]) -> Pubkey {
    let mut rng = rand::thread_rng();
    let idx = rng.gen_range(0..collectors.len());
    collectors[idx]
}

/// Add CU jitter
pub fn jittered_cu(base_cu: u32, jitter_range: u32) -> u32 {
    let mut rng = rand::thread_rng();
    base_cu + rng.gen_range(0..jitter_range)
}

/// Estimate CU for a route based on per-DEX lookup
pub fn estimate_cu(dex_types: &[u8]) -> u32 {
    let mut cu: u32 = 100; // program overhead
    for dex in dex_types {
        cu += match dex {
            0 => 35_000,  // RaydiumAmmV4
            1 => 35_000,  // RaydiumCpmm
            2 => 80_000,  // RaydiumClmm
            3 => 30_000,  // PumpFun
            4 => 35_000,  // PumpSwap
            5 => 30_000,  // Bonk
            6 => 45_000,  // MeteoraDammV2
            _ => 50_000,  // unknown fallback
        };
    }
    cu
}
```

**Step 2: Create alt.rs**

Create `executor/src/alt.rs`:
```rust
use solana_sdk::pubkey::Pubkey;

/// Tier 0 static ALT — contains program IDs, token programs, tip accounts, etc.
pub struct Tier0Alt {
    pub address: Pubkey,
}

impl Tier0Alt {
    pub fn new(address: Pubkey) -> Self {
        Self { address }
    }
}
```

**Step 3: Create tx_builder.rs**

Create `executor/src/tx_builder.rs`:
```rust
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_instruction,
    transaction::Transaction,
    message::Message,
    hash::Hash,
    signer::Signer,
    signature::Keypair,
};
use arb_engine::opportunity::Opportunity;

use crate::anti_fp;
use crate::config::ExecutorConfigFile;

/// Built transaction pair
pub struct TxPair {
    pub jito_tx: Option<Transaction>,     // Variant A: with tip, no CU price
    pub swqos_tx: Option<Transaction>,    // Variant B: with CU price, no tip
}

pub struct TxBuilder {
    program_id: Pubkey,
    fee_collectors: Vec<Pubkey>,
    cu_jitter_range: u32,
    jito_tip_percentage: u32,
    jito_min_tip: u64,
    jito_min_operator_profit: u64,
    swqos_cu_price_percentage: u32,
    jito_enabled: bool,
    swqos_enabled: bool, // flashblock or astralane enabled
}

impl TxBuilder {
    pub fn from_config(config: &ExecutorConfigFile) -> Self {
        let fee_collectors: Vec<Pubkey> = config
            .executor
            .anti_fingerprint
            .fee_collectors_sol
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();

        let jito = config.jito.as_ref();
        let jito_enabled = jito.map_or(false, |j| j.enabled);

        let flashblock_enabled = config.flashblock.as_ref().map_or(false, |f| f.enabled);
        let astralane_enabled = config.astralane.as_ref().map_or(false, |a| a.enabled);

        // Use higher of flashblock/astralane cu_price_percentage
        let swqos_pct = config
            .flashblock
            .as_ref()
            .map_or(30, |f| f.cu_price_percentage)
            .max(
                config
                    .astralane
                    .as_ref()
                    .map_or(30, |a| a.cu_price_percentage),
            );

        Self {
            program_id: config.executor.program_id.parse().unwrap(),
            fee_collectors,
            cu_jitter_range: config.executor.anti_fingerprint.cu_jitter_range,
            jito_tip_percentage: jito.map_or(60, |j| j.tip_percentage),
            jito_min_tip: jito.map_or(1000, |j| j.min_tip_lamports),
            jito_min_operator_profit: jito.map_or(5000, |j| j.min_operator_profit_lamports),
            swqos_cu_price_percentage: swqos_pct,
            jito_enabled,
            swqos_enabled: flashblock_enabled || astralane_enabled,
        }
    }

    /// Build two transaction variants from an Opportunity.
    pub fn build(
        &self,
        opp: &Opportunity,
        payer: &Keypair,
        recent_blockhash: Hash,
    ) -> TxPair {
        let dex_types: Vec<u8> = opp
            .pool_snapshots
            .iter()
            .map(|s| s.dex_type as u8)
            .collect();
        let base_cu = anti_fp::estimate_cu(&dex_types);
        let cu_limit = anti_fp::jittered_cu(base_cu, self.cu_jitter_range);

        // Build arbitrage instruction
        let arb_ix = self.build_arb_instruction(opp);

        // Variant A: Jito bundle
        let jito_tx = if self.jito_enabled {
            let tip = self.calculate_jito_tip(opp.expected_profit);
            if let Some(tip) = tip {
                let tip_account = anti_fp::random_tip_account();
                let mut ixs = vec![
                    ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                    arb_ix.clone(),
                    system_instruction::transfer(&payer.pubkey(), &tip_account, tip),
                ];
                let msg = Message::new(&ixs, Some(&payer.pubkey()));
                let mut tx = Transaction::new_unsigned(msg);
                tx.partial_sign(&[payer], recent_blockhash);
                Some(tx)
            } else {
                None
            }
        } else {
            None
        };

        // Variant B: SWQoS (priority fee)
        let swqos_tx = if self.swqos_enabled {
            let cu_price = self.calculate_cu_price(opp.expected_profit, base_cu);
            let ixs = vec![
                ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                ComputeBudgetInstruction::set_compute_unit_price(cu_price),
                arb_ix,
            ];
            let msg = Message::new(&ixs, Some(&payer.pubkey()));
            let mut tx = Transaction::new_unsigned(msg);
            tx.partial_sign(&[payer], recent_blockhash);
            Some(tx)
        } else {
            None
        };

        TxPair { jito_tx, swqos_tx }
    }

    fn build_arb_instruction(&self, opp: &Opportunity) -> Instruction {
        let hop_count = opp.route.hops.len() as u8;

        // Build instruction data
        let data = self.encode_instruction_data(opp, hop_count);

        // Build account metas
        let accounts = self.build_account_metas(opp);

        Instruction {
            program_id: self.program_id,
            accounts,
            data,
        }
    }

    fn encode_instruction_data(&self, opp: &Opportunity, hop_count: u8) -> Vec<u8> {
        let mut data = Vec::with_capacity(20);

        // Discriminator
        data.push(hop_count - 2); // 0=2hop, 1=3hop, 2=4hop

        // First and last DEX types
        data.push(opp.pool_snapshots[0].dex_type as u8);
        data.push(opp.pool_snapshots[hop_count as usize - 1].dex_type as u8);

        // Flags byte
        let mut flags: u8 = 0;
        if opp.pool_snapshots[0].is_a_to_b {
            flags |= 1;
        }
        if opp.pool_snapshots[hop_count as usize - 1].is_a_to_b {
            flags |= 1 << 1;
        }
        // TODO: set token_2022 bits when needed
        // TODO: set flashloan bit from config
        data.push(flags);

        // Middle hops (3-hop and 4-hop)
        for i in 1..hop_count as usize - 1 {
            data.push(opp.pool_snapshots[i].dex_type as u8);
            let mut mid_flags: u8 = 0;
            if opp.pool_snapshots[i].is_a_to_b {
                mid_flags |= 1;
            }
            data.push(mid_flags);
        }

        // amount_in (u64 LE)
        data.extend_from_slice(&opp.amount_in.to_le_bytes());

        // min_profit (u32 LE) — use 80% of expected as safety margin
        let min_profit = (opp.expected_profit * 80 / 100) as u32;
        data.extend_from_slice(&min_profit.to_le_bytes());

        data
    }

    fn build_account_metas(&self, opp: &Opportunity) -> Vec<AccountMeta> {
        // Header + intermediate + pool accounts
        // This is a scaffold — full implementation needs:
        // 1. Payer (signer)
        // 2. Base mint
        // 3. User base ATA
        // 4. Fee collector (random)
        // 5-8. Program IDs
        // 9+. Intermediate token accounts
        // N+. Flashloan accounts (if enabled)
        // M+. Per-hop pool accounts from PoolSnapshot.accounts
        Vec::new() // TODO: implement full account meta construction
    }

    fn calculate_jito_tip(&self, expected_profit: u64) -> Option<u64> {
        let tip = expected_profit * self.jito_tip_percentage as u64 / 100;
        let tip = tip.max(self.jito_min_tip);
        if tip + self.jito_min_operator_profit > expected_profit {
            return None; // Not profitable enough
        }
        Some(tip)
    }

    fn calculate_cu_price(&self, expected_profit: u64, base_cu: u32) -> u64 {
        let fee_budget = expected_profit * self.swqos_cu_price_percentage as u64 / 100;
        // micro-lamports per CU
        (fee_budget * 1_000_000) / base_cu as u64
    }
}
```

**Step 4: Update lib.rs**

```rust
pub mod alt;
pub mod anti_fp;
pub mod config;
pub mod tx_builder;
```

**Step 5: Verify it compiles**

Run: `cd executor && cargo check`

**Step 6: Commit**

```bash
git add executor/src/
git commit -m "feat(executor): add tx_builder, anti-fingerprint, and ALT module"
```

---

## Task 7: Multi-channel sender

**Files:**
- Create: `executor/src/sender/mod.rs`
- Create: `executor/src/sender/jito.rs`
- Create: `executor/src/sender/flashblock.rs`
- Create: `executor/src/sender/astralane.rs`
- Modify: `executor/src/lib.rs`

**Step 1: Create sender/mod.rs**

Create `executor/src/sender/mod.rs`:
```rust
pub mod jito;
pub mod flashblock;
pub mod astralane;

use log::{info, warn};
use solana_sdk::transaction::Transaction;
use tokio::task::JoinHandle;

use crate::config::ExecutorConfigFile;
use crate::tx_builder::TxPair;

pub struct MultiSender {
    jito_senders: Vec<jito::JitoSender>,
    flashblock_senders: Vec<flashblock::FlashblockSender>,
    astralane_senders: Vec<astralane::AstralaneSender>,
}

impl MultiSender {
    pub fn from_config(config: &ExecutorConfigFile) -> Self {
        let mut jito_senders = Vec::new();
        if let Some(jito_cfg) = &config.jito {
            if jito_cfg.enabled {
                for url in &jito_cfg.block_engine_urls {
                    jito_senders.push(jito::JitoSender::new(url.clone()));
                }
            }
        }

        let mut flashblock_senders = Vec::new();
        if let Some(fb_cfg) = &config.flashblock {
            if fb_cfg.enabled {
                for endpoint in &fb_cfg.endpoints {
                    flashblock_senders.push(flashblock::FlashblockSender::new(
                        endpoint.clone(),
                        fb_cfg.api_key.clone(),
                    ));
                }
            }
        }

        let mut astralane_senders = Vec::new();
        if let Some(ast_cfg) = &config.astralane {
            if ast_cfg.enabled {
                for endpoint in &ast_cfg.endpoints {
                    astralane_senders.push(astralane::AstralaneSender::new(
                        endpoint.clone(),
                        ast_cfg.api_key.clone(),
                    ));
                }
            }
        }

        info!(
            "MultiSender initialized: {} Jito, {} Flashblock, {} Astralane endpoints",
            jito_senders.len(),
            flashblock_senders.len(),
            astralane_senders.len(),
        );

        Self {
            jito_senders,
            flashblock_senders,
            astralane_senders,
        }
    }

    /// Send both tx variants to all enabled channels concurrently
    pub async fn send_all(&self, pair: &TxPair) {
        let mut handles: Vec<JoinHandle<()>> = Vec::new();

        // Jito: send bundle with Variant A
        if let Some(ref tx) = pair.jito_tx {
            for sender in &self.jito_senders {
                let sender = sender.clone();
                let tx = tx.clone();
                handles.push(tokio::spawn(async move {
                    if let Err(e) = sender.send_bundle(&tx).await {
                        warn!("Jito send failed: {}", e);
                    }
                }));
            }
        }

        // Flashblock: send Variant B
        if let Some(ref tx) = pair.swqos_tx {
            for sender in &self.flashblock_senders {
                let sender = sender.clone();
                let tx = tx.clone();
                handles.push(tokio::spawn(async move {
                    if let Err(e) = sender.send_transaction(&tx).await {
                        warn!("Flashblock send failed: {}", e);
                    }
                }));
            }
        }

        // Astralane: send Variant B
        if let Some(ref tx) = pair.swqos_tx {
            for sender in &self.astralane_senders {
                let sender = sender.clone();
                let tx = tx.clone();
                handles.push(tokio::spawn(async move {
                    if let Err(e) = sender.send_transaction(&tx).await {
                        warn!("Astralane send failed: {}", e);
                    }
                }));
            }
        }

        // Wait for all to complete (fire-and-forget is also fine)
        for handle in handles {
            let _ = handle.await;
        }
    }
}
```

**Step 2: Create sender/jito.rs**

Create `executor/src/sender/jito.rs`:
```rust
use solana_sdk::transaction::Transaction;
use anyhow::Result;

/// Jito gRPC bundle sender
#[derive(Clone)]
pub struct JitoSender {
    endpoint: String,
}

impl JitoSender {
    pub fn new(endpoint: String) -> Self {
        Self { endpoint }
    }

    /// Send a bundle containing a single transaction via Jito gRPC
    pub async fn send_bundle(&self, tx: &Transaction) -> Result<()> {
        // TODO: Implement Jito gRPC sendBundle
        // 1. Serialize tx to base64
        // 2. Connect to self.endpoint via tonic gRPC
        // 3. Call SendBundle RPC
        // 4. Optionally subscribe to BundleResult
        log::debug!("Jito sendBundle to {}", self.endpoint);
        Ok(())
    }
}
```

**Step 3: Create sender/flashblock.rs**

Create `executor/src/sender/flashblock.rs`:
```rust
use solana_sdk::transaction::Transaction;
use anyhow::Result;

/// Flashblock JSON-RPC sender
#[derive(Clone)]
pub struct FlashblockSender {
    endpoint: String,
    api_key: String,
    client: reqwest::Client,
}

impl FlashblockSender {
    pub fn new(endpoint: String, api_key: String) -> Self {
        Self {
            endpoint,
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// Send transaction via JSON-RPC sendTransaction
    pub async fn send_transaction(&self, tx: &Transaction) -> Result<()> {
        // TODO: Implement Flashblock sendTransaction
        // 1. Serialize tx to base64
        // 2. POST JSON-RPC { method: "sendTransaction", params: [base64_tx, {encoding: "base64"}] }
        // 3. Include api_key in header
        log::debug!("Flashblock sendTransaction to {}", self.endpoint);
        Ok(())
    }
}
```

**Step 4: Create sender/astralane.rs**

Create `executor/src/sender/astralane.rs`:
```rust
use solana_sdk::transaction::Transaction;
use anyhow::Result;

/// Astralane JSON-RPC sender
#[derive(Clone)]
pub struct AstralaneSender {
    endpoint: String,
    api_key: String,
    client: reqwest::Client,
}

impl AstralaneSender {
    pub fn new(endpoint: String, api_key: String) -> Self {
        Self {
            endpoint,
            api_key,
            client: reqwest::Client::new(),
        }
    }

    /// Send transaction via JSON-RPC sendTransaction
    pub async fn send_transaction(&self, tx: &Transaction) -> Result<()> {
        // TODO: Implement Astralane sendTransaction
        // 1. Serialize tx to base64
        // 2. POST to endpoint with api_key query param
        // 3. JSON-RPC { method: "sendTransaction", params: [base64_tx] }
        log::debug!("Astralane sendTransaction to {}", self.endpoint);
        Ok(())
    }
}
```

**Step 5: Update lib.rs**

```rust
pub mod alt;
pub mod anti_fp;
pub mod config;
pub mod sender;
pub mod tx_builder;
```

**Step 6: Verify it compiles**

Run: `cd executor && cargo check`

**Step 7: Commit**

```bash
git add executor/src/
git commit -m "feat(executor): add multi-channel sender (Jito gRPC, Flashblock, Astralane)"
```

---

## Task 8: Executor main loop

**Files:**
- Create: `executor/src/executor.rs`
- Modify: `executor/src/lib.rs`

**Step 1: Create executor.rs**

Create `executor/src/executor.rs`:
```rust
use std::sync::Arc;
use log::{info, debug, warn};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::signature::Keypair;
use tokio::sync::mpsc;

use arb_engine::opportunity::Opportunity;

use crate::config::ExecutorConfigFile;
use crate::sender::MultiSender;
use crate::tx_builder::TxBuilder;

pub struct Executor {
    config: ExecutorConfigFile,
    tx_builder: TxBuilder,
    multi_sender: MultiSender,
    opp_rx: mpsc::Receiver<Opportunity>,
    payer: Arc<Keypair>,
    rpc: Arc<RpcClient>,
}

impl Executor {
    pub fn new(
        config: ExecutorConfigFile,
        opp_rx: mpsc::Receiver<Opportunity>,
        payer: Keypair,
        rpc_url: &str,
    ) -> Self {
        let tx_builder = TxBuilder::from_config(&config);
        let multi_sender = MultiSender::from_config(&config);
        let rpc = Arc::new(RpcClient::new(rpc_url.to_string()));

        Self {
            config,
            tx_builder,
            multi_sender,
            opp_rx,
            payer: Arc::new(payer),
            rpc,
        }
    }

    pub async fn run(mut self) {
        info!("Executor started. Waiting for opportunities...");

        // Track latest slot for staleness check
        let mut latest_slot: u64 = 0;

        // Periodically refresh blockhash
        let mut recent_blockhash = self
            .rpc
            .get_latest_blockhash()
            .await
            .expect("Failed to get initial blockhash");
        let mut blockhash_age: u64 = 0;

        loop {
            let opp = match self.opp_rx.recv().await {
                Some(opp) => opp,
                None => {
                    info!("Opportunity channel closed. Shutting down.");
                    break;
                }
            };

            // Update latest slot
            if opp.slot > latest_slot {
                latest_slot = opp.slot;
            }

            // Staleness check: > 2 slots → discard
            if latest_slot > opp.slot && latest_slot - opp.slot > 2 {
                debug!("Discarding stale opportunity (slot {} vs latest {})", opp.slot, latest_slot);
                continue;
            }

            // Refresh blockhash every ~50 opportunities
            blockhash_age += 1;
            if blockhash_age > 50 {
                match self.rpc.get_latest_blockhash().await {
                    Ok(bh) => {
                        recent_blockhash = bh;
                        blockhash_age = 0;
                    }
                    Err(e) => {
                        warn!("Failed to refresh blockhash: {}", e);
                    }
                }
            }

            // Build tx pair
            let pair = self.tx_builder.build(&opp, &self.payer, recent_blockhash);

            debug!(
                "Opportunity: {} hops, profit={} lamports, submitting...",
                opp.route.hops.len(),
                opp.expected_profit,
            );

            // Submit all channels concurrently
            self.multi_sender.send_all(&pair).await;
        }
    }
}
```

**Step 2: Update lib.rs**

```rust
pub mod alt;
pub mod anti_fp;
pub mod config;
pub mod executor;
pub mod sender;
pub mod tx_builder;
```

**Step 3: Verify it compiles**

Run: `cd executor && cargo check`

**Step 4: Commit**

```bash
git add executor/src/
git commit -m "feat(executor): add main loop with staleness check and multi-channel submit"
```

---

## Task 9: Full pipeline example

**Files:**
- Create: `executor/examples/full_pipeline.rs`

**Step 1: Create full pipeline example**

Create `executor/examples/full_pipeline.rs`:
```rust
//! Full pipeline: Stage 1 (PoolStreamer) → Stage 2 (Engine) → Stage 3 (Executor)

use std::sync::Arc;
use arb_engine::config::EngineConfig;
use arb_engine::engine::Engine;
use arb_executor::config::ExecutorConfigFile;
use arb_executor::executor::Executor;
use solana_sdk::signature::Keypair;

use solana_streamer_sdk::pool::streamer::{PoolStreamer, PoolStreamerConfig};
use solana_streamer_sdk::streaming::event_parser::protocols::{
    bonk::parser::BONK_PROGRAM_ID, meteora_damm_v2::parser::METEORA_DAMM_V2_PROGRAM_ID,
    pumpfun::parser::PUMPFUN_PROGRAM_ID, pumpswap::parser::PUMPSWAP_PROGRAM_ID,
    raydium_amm_v4::parser::RAYDIUM_AMM_V4_PROGRAM_ID,
    raydium_clmm::parser::RAYDIUM_CLMM_PROGRAM_ID,
    raydium_cpmm::parser::RAYDIUM_CPMM_PROGRAM_ID,
};
use solana_streamer_sdk::streaming::event_parser::Protocol;
use solana_streamer_sdk::streaming::yellowstone_grpc::{
    AccountFilter, TransactionFilter, YellowstoneGrpc,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let grpc_endpoint = std::env::var("GRPC_ENDPOINT")
        .unwrap_or_else(|_| "https://solana-yellowstone-grpc.publicnode.com:443".to_string());
    let grpc_token = std::env::var("GRPC_TOKEN").ok();
    let rpc_url = std::env::var("RPC_URL")
        .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string());

    // Load executor config
    let config_path = std::env::var("CONFIG_PATH")
        .unwrap_or_else(|_| "executor/config.toml".to_string());
    let executor_config = ExecutorConfigFile::load(&config_path)?;

    // Load payer keypair
    let payer = Keypair::new(); // TODO: load from file

    // Stage 1: Pool Streamer
    let pool_config = PoolStreamerConfig {
        rpc_url: rpc_url.clone(),
        update_channel_size: 4096,
    };
    let (pool_streamer, update_rx) = PoolStreamer::new(pool_config);
    let pool_streamer = Arc::new(pool_streamer);

    // Stage 2: Route Engine
    let engine_config = EngineConfig::default();
    let (engine, opp_rx) = Engine::new(engine_config, update_rx);

    // Stage 3: Executor
    let executor = Executor::new(executor_config, opp_rx, payer, &rpc_url);

    // Spawn all stages
    tokio::spawn(async move { engine.run().await });
    tokio::spawn(async move { executor.run().await });

    // Start gRPC subscription
    let grpc = Arc::new(YellowstoneGrpc::new(grpc_endpoint, grpc_token)?);

    let protocols = vec![
        Protocol::PumpFun, Protocol::PumpSwap, Protocol::Bonk,
        Protocol::RaydiumCpmm, Protocol::RaydiumClmm,
        Protocol::RaydiumAmmV4, Protocol::MeteoraDammV2,
    ];

    let account_include = vec![
        PUMPFUN_PROGRAM_ID.to_string(), PUMPSWAP_PROGRAM_ID.to_string(),
        BONK_PROGRAM_ID.to_string(), RAYDIUM_CPMM_PROGRAM_ID.to_string(),
        RAYDIUM_CLMM_PROGRAM_ID.to_string(), RAYDIUM_AMM_V4_PROGRAM_ID.to_string(),
        METEORA_DAMM_V2_PROGRAM_ID.to_string(),
    ];

    let transaction_filter = TransactionFilter {
        account_include: account_include.clone(),
        account_exclude: vec![],
        account_required: vec![],
    };

    let account_filter = AccountFilter {
        account: vec![],
        owner: account_include,
        filters: vec![],
    };

    let streamer = pool_streamer.clone();
    grpc.subscribe_events_immediate(
        protocols, None,
        vec![transaction_filter], vec![account_filter],
        None, None,
        move |event| {
            let streamer = streamer.clone();
            tokio::spawn(async move { streamer.on_event(event).await; });
        },
    ).await?;

    println!("Full pipeline running. Press Ctrl+C to stop.");
    tokio::signal::ctrl_c().await?;
    println!("Shutting down.");

    Ok(())
}
```

**Step 2: Verify it compiles**

Run: `cd executor && cargo check --all-targets`

**Step 3: Commit**

```bash
git add executor/examples/
git commit -m "feat(executor): add full pipeline example (Stage 1 + 2 + 3)"
```

---

## Task 10: Fix compilation and verify

**Files:**
- Any files with compilation issues from previous tasks

**Step 1: Run full compilation**

```bash
cd program && cargo check 2>&1
cd executor && cargo check --all-targets 2>&1
cd engine && cargo check --all-targets 2>&1
cd solana-streamer && cargo check --all-targets 2>&1
```

Fix all errors.

**Step 2: Commit fixes**

```bash
git commit -am "fix: resolve compilation issues across all crates"
```
