# Stage 3 Completion Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Fill all 23 scaffold gaps in Stage 3 — refactor flashloan to off-chain, complete all DEX CPI wrappers, implement account meta construction, senders, ALT, and parallel signing.

**Architecture:** First refactor flashloan from on-chain CPI to off-chain top-level instructions (MarginFi). Then complete on-chain DEX CPI wiring. Then complete off-chain tx_builder, senders, ALT, and parallel signing. Order: C (flashloan refactor) → on-chain → off-chain.

**Tech Stack:** Rust, pinocchio 0.10, dex-pinocchio-cpi, solana-sdk 3.0.0, tonic + tonic-build (Jito gRPC), reqwest (Flashblock/Astralane), rayon (parallel signing)

---

## Task 1: Flashloan refactor — remove on-chain flashloan

**Files:**
- Delete: `program/src/flashloan.rs`
- Modify: `program/src/lib.rs`
- Modify: `program/src/accounts.rs`
- Modify: `program/src/swap.rs`

**Step 1: Delete flashloan.rs**

Delete `program/src/flashloan.rs` entirely.

**Step 2: Remove `mod flashloan;` from lib.rs**

In `program/src/lib.rs`, remove the line:
```rust
mod flashloan;
```

**Step 3: Remove flashloan from accounts.rs**

In `program/src/accounts.rs`:

Remove this constant:
```rust
pub const FLASHLOAN_ACCOUNT_COUNT: usize = 3;
```

In `SwapInstruction` struct, remove:
```rust
pub use_flashloan: bool,
```

In `SwapInstruction::parse()`, remove `use_flashloan` from flags parsing (bit4) and from the returned struct. The flags byte becomes: bit0=buy_a_to_b, bit1=sell_a_to_b, bit2=buy_2022, bit3=sell_2022.

Change `pool_accounts_start()` signature — remove `use_flashloan: bool` param:
```rust
pub fn pool_accounts_start(
    hop_count: u8,
    hop_index: u8,
    hops: &[HopInfo; 4],
) -> usize {
    let mut offset = HEADER_SIZE;
    offset += (hop_count as usize - 1) * INTERMEDIATE_ACCOUNTS_PER_TOKEN;
    for i in 0..hop_index as usize {
        offset += hops[i].dex_type.pool_account_count();
    }
    offset
}
```

**Step 4: Remove flashloan from swap.rs**

In `program/src/swap.rs`:

Remove from imports: `HEADER_SIZE, INTERMEDIATE_ACCOUNTS_PER_TOKEN, FLASHLOAN_ACCOUNT_COUNT`

Remove `flashloan_slice()` helper function entirely.

Remove the flashloan borrow/repay blocks from `execute()`:
```rust
// Remove this entire block:
let fl_accounts = if ix.use_flashloan { ... };
// And this block:
if let Some(fl) = fl_accounts { ... }
```

Update `pool_accounts_start` call — remove `ix.use_flashloan` argument:
```rust
let pool_start = pool_accounts_start(
    ix.hop_count,
    i,
    &ix.hops,
);
```

**Step 5: Verify it compiles**

Run: `cd program && cargo check`

**Step 6: Commit**

```bash
git add program/
git commit -m "refactor(program): remove on-chain flashloan — moved to off-chain tx assembly"
```

---

## Task 2: Wire amount_in through DEX dispatch

**Files:**
- Modify: `program/src/swap.rs`

**Step 1: Change dispatch_swap signature**

Add `amount_in: u64` parameter:
```rust
fn dispatch_swap(hop: &HopInfo, pool_accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    match hop.dex_type {
        DexType::RaydiumAmmV4 => swap_raydium_amm(pool_accounts, amount_in),
        DexType::RaydiumCpmm => swap_raydium_cp(pool_accounts, amount_in),
        DexType::RaydiumClmm => swap_raydium_clmm(pool_accounts, amount_in),
        DexType::PumpFun => swap_pumpfun(pool_accounts, hop.is_a_to_b, amount_in),
        DexType::PumpSwap => swap_pumpswap(pool_accounts, hop.is_a_to_b, amount_in),
        DexType::Bonk => swap_bonkswap(pool_accounts, amount_in),
        DexType::MeteoraDammV2 => swap_meteora_damm_v2(pool_accounts, amount_in),
    }
}
```

**Step 2: Update execute() to pass amount_in**

First hop gets `ix.amount_in`, subsequent hops get `0` (= use full balance):
```rust
for i in 0..ix.hop_count {
    let hop = &ix.hops[i as usize];
    let pool_start = pool_accounts_start(ix.hop_count, i, &ix.hops);
    let pool_end = pool_start + hop.dex_type.pool_account_count();

    if pool_end > accounts.len() {
        return Err(ProgramError::NotEnoughAccountKeys);
    }

    let amount = if i == 0 { ix.amount_in } else { 0 };
    dispatch_swap(hop, &accounts[pool_start..pool_end], amount)?;
}
```

**Step 3: Update 3 Raydium wrappers to accept and use amount_in**

`swap_raydium_amm(accounts, amount_in)`:
```rust
fn swap_raydium_amm(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    let swap_accounts = dex_pinocchio_cpi::raydium_amm::SwapBaseInV2Accounts {
        token_program: &accounts[0],
        amm: &accounts[1],
        amm_authority: &accounts[2],
        amm_coin_vault: &accounts[3],
        amm_pc_vault: &accounts[4],
        user_source: &accounts[5],
        user_destination: &accounts[6],
        user_owner: &accounts[7],
    };
    let args = dex_pinocchio_cpi::raydium_amm::SwapBaseInV2Args {
        amount_in,
        minimum_amount_out: 0, // profit verified atomically
    };
    dex_pinocchio_cpi::raydium_amm::swap_base_in_v2(&swap_accounts, &args, &[])
}
```

`swap_raydium_cp(accounts, amount_in)`:
```rust
fn swap_raydium_cp(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    let swap_accounts = dex_pinocchio_cpi::raydium_cp::SwapBaseInputAccounts {
        payer: &accounts[0],
        authority: &accounts[1],
        amm_config: &accounts[2],
        pool_state: &accounts[3],
        input_token_account: &accounts[4],
        output_token_account: &accounts[5],
        input_vault: &accounts[6],
        output_vault: &accounts[7],
        input_token_program: &accounts[8],
        output_token_program: &accounts[9],
        input_token_mint: &accounts[10],
        output_token_mint: &accounts[11],
        observation_state: &accounts[12],
    };
    let args = dex_pinocchio_cpi::raydium_cp::SwapBaseInputArgs {
        amount_in,
        minimum_amount_out: 0,
    };
    dex_pinocchio_cpi::raydium_cp::swap_base_input(&swap_accounts, &args, &[])
}
```

`swap_raydium_clmm(accounts, amount_in)`:
```rust
fn swap_raydium_clmm(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    let swap_accounts = dex_pinocchio_cpi::raydium_clmm::SwapAccounts {
        payer: &accounts[0],
        amm_config: &accounts[1],
        pool_state: &accounts[2],
        input_token_account: &accounts[3],
        output_token_account: &accounts[4],
        input_vault: &accounts[5],
        output_vault: &accounts[6],
        observation_state: &accounts[7],
        token_program: &accounts[8],
        tick_array: &accounts[9],
    };
    let args = dex_pinocchio_cpi::raydium_clmm::SwapArgs {
        amount: amount_in,
        other_amount_threshold: 0,
        sqrt_price_limit_x64: 0,
        is_base_input: true,
    };
    dex_pinocchio_cpi::raydium_clmm::swap(&swap_accounts, &args, &[])
}
```

**Step 4: Update 4 DEX stubs to accept amount_in (still stubs for now)**

```rust
fn swap_pumpfun(_accounts: &[AccountView], _is_a_to_b: bool, _amount_in: u64) -> ProgramResult {
    // TODO Task 3: implement
    Ok(())
}

fn swap_pumpswap(_accounts: &[AccountView], _is_a_to_b: bool, _amount_in: u64) -> ProgramResult {
    // TODO Task 3: implement
    Ok(())
}

fn swap_bonkswap(_accounts: &[AccountView], _amount_in: u64) -> ProgramResult {
    // TODO Task 3: implement
    Ok(())
}

fn swap_meteora_damm_v2(_accounts: &[AccountView], _amount_in: u64) -> ProgramResult {
    // TODO Task 3: implement
    Ok(())
}
```

**Step 5: Verify it compiles**

Run: `cd program && cargo check`

**Step 6: Commit**

```bash
git add program/src/swap.rs
git commit -m "feat(program): wire amount_in through dispatch_swap to all DEX CPI wrappers"
```

---

## Task 3: Implement 4 remaining DEX CPI wrappers

**Files:**
- Modify: `program/src/swap.rs`
- Modify: `program/src/accounts.rs` (update PumpSwap pool_account_count)

**Context — CPI interfaces from dex-pinocchio-cpi:**

PumpFun (`pump_fun`):
- buy: 16 accounts — BuyArgs { amount: u64, max_sol_cost: u64, track_volume: [u8; 32] }
- sell: 14 accounts — SellArgs { amount: u64, min_sol_output: u64 }

PumpSwap (`pump_fun_amm`):
- buy: 23 accounts — BuyArgs { base_amount_out: u64, max_quote_amount_in: u64, track_volume: [u8; 32] }
- sell: 21 accounts — SellArgs { base_amount_in: u64, min_quote_amount_out: u64 }

Bonkswap (`bonkswap`):
- swap: 17 accounts — SwapArgs { delta_in: [u8; 32], price_limit: [u8; 32], x_to_y: bool }

Meteora DAMM V2 (`meteora_damm_v2`):
- swap: 14 accounts — SwapArgs { params: [u8; 32] }

**Step 1: Fix PumpSwap pool_account_count**

In `program/src/accounts.rs`, PumpSwap currently says 23. But sell is 21. Since we always allocate max (buy), keep 23:
```rust
DexType::PumpSwap => 23,  // buy=23, sell=21, allocate max
```

No change needed — it's already 23.

**Step 2: Implement swap_pumpfun**

Read `dex-pinocchio-cpi/src/pump_fun.rs` to confirm exact account field names, then implement:

```rust
fn swap_pumpfun(accounts: &[AccountView], is_a_to_b: bool, amount_in: u64) -> ProgramResult {
    if is_a_to_b {
        // Buy: SOL → Token
        let buy_accounts = dex_pinocchio_cpi::pump_fun::BuyAccounts {
            global: &accounts[0],
            fee_recipient: &accounts[1],
            mint: &accounts[2],
            bonding_curve: &accounts[3],
            associated_bonding_curve: &accounts[4],
            associated_user: &accounts[5],
            user: &accounts[6],
            system_program: &accounts[7],
            token_program: &accounts[8],
            creator_vault: &accounts[9],
            event_authority: &accounts[10],
            program: &accounts[11],
            global_volume_accumulator: &accounts[12],
            user_volume_accumulator: &accounts[13],
            fee_config: &accounts[14],
            fee_program: &accounts[15],
        };
        let args = dex_pinocchio_cpi::pump_fun::BuyArgs {
            amount: amount_in,
            max_sol_cost: u64::MAX,  // no slippage — profit verified atomically
            track_volume: [0u8; 32],
        };
        dex_pinocchio_cpi::pump_fun::buy(&buy_accounts, &args, &[])
    } else {
        // Sell: Token → SOL
        let sell_accounts = dex_pinocchio_cpi::pump_fun::SellAccounts {
            global: &accounts[0],
            fee_recipient: &accounts[1],
            mint: &accounts[2],
            bonding_curve: &accounts[3],
            associated_bonding_curve: &accounts[4],
            associated_user: &accounts[5],
            user: &accounts[6],
            system_program: &accounts[7],
            creator_vault: &accounts[8],
            token_program: &accounts[9],
            event_authority: &accounts[10],
            program: &accounts[11],
            fee_config: &accounts[12],
            fee_program: &accounts[13],
        };
        let args = dex_pinocchio_cpi::pump_fun::SellArgs {
            amount: amount_in,
            min_sol_output: 0,  // no slippage — profit verified atomically
        };
        dex_pinocchio_cpi::pump_fun::sell(&sell_accounts, &args, &[])
    }
}
```

**Step 3: Implement swap_pumpswap**

```rust
fn swap_pumpswap(accounts: &[AccountView], is_a_to_b: bool, amount_in: u64) -> ProgramResult {
    if is_a_to_b {
        // Buy: quote → base
        let buy_accounts = dex_pinocchio_cpi::pump_fun_amm::BuyAccounts {
            pool: &accounts[0],
            user: &accounts[1],
            global_config: &accounts[2],
            base_mint: &accounts[3],
            quote_mint: &accounts[4],
            user_base_token_account: &accounts[5],
            user_quote_token_account: &accounts[6],
            pool_base_token_account: &accounts[7],
            pool_quote_token_account: &accounts[8],
            protocol_fee_recipient: &accounts[9],
            protocol_fee_recipient_token_account: &accounts[10],
            base_token_program: &accounts[11],
            quote_token_program: &accounts[12],
            system_program: &accounts[13],
            associated_token_program: &accounts[14],
            event_authority: &accounts[15],
            program: &accounts[16],
            coin_creator_vault_ata: &accounts[17],
            coin_creator_vault_authority: &accounts[18],
            global_volume_accumulator: &accounts[19],
            user_volume_accumulator: &accounts[20],
            fee_config: &accounts[21],
            fee_program: &accounts[22],
        };
        let args = dex_pinocchio_cpi::pump_fun_amm::BuyArgs {
            base_amount_out: amount_in,
            max_quote_amount_in: u64::MAX,
            track_volume: [0u8; 32],
        };
        dex_pinocchio_cpi::pump_fun_amm::buy(&buy_accounts, &args, &[])
    } else {
        // Sell: base → quote
        let sell_accounts = dex_pinocchio_cpi::pump_fun_amm::SellAccounts {
            pool: &accounts[0],
            user: &accounts[1],
            global_config: &accounts[2],
            base_mint: &accounts[3],
            quote_mint: &accounts[4],
            user_base_token_account: &accounts[5],
            user_quote_token_account: &accounts[6],
            pool_base_token_account: &accounts[7],
            pool_quote_token_account: &accounts[8],
            protocol_fee_recipient: &accounts[9],
            protocol_fee_recipient_token_account: &accounts[10],
            base_token_program: &accounts[11],
            quote_token_program: &accounts[12],
            system_program: &accounts[13],
            associated_token_program: &accounts[14],
            event_authority: &accounts[15],
            program: &accounts[16],
            coin_creator_vault_ata: &accounts[17],
            coin_creator_vault_authority: &accounts[18],
            fee_config: &accounts[19],
            fee_program: &accounts[20],
        };
        let args = dex_pinocchio_cpi::pump_fun_amm::SellArgs {
            base_amount_in: amount_in,
            min_quote_amount_out: 0,
        };
        dex_pinocchio_cpi::pump_fun_amm::sell(&sell_accounts, &args, &[])
    }
}
```

**Step 4: Implement swap_bonkswap**

```rust
fn swap_bonkswap(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    let swap_accounts = dex_pinocchio_cpi::bonkswap::SwapAccounts {
        state: &accounts[0],
        pool: &accounts[1],
        token_x: &accounts[2],
        token_y: &accounts[3],
        pool_x_account: &accounts[4],
        pool_y_account: &accounts[5],
        swapper_x_account: &accounts[6],
        swapper_y_account: &accounts[7],
        swapper: &accounts[8],
        referrer_x_account: &accounts[9],
        referrer_y_account: &accounts[10],
        referrer: &accounts[11],
        program_authority: &accounts[12],
        system_program: &accounts[13],
        token_program: &accounts[14],
        associated_token_program: &accounts[15],
        rent: &accounts[16],
    };
    // delta_in is [u8; 32] — encode u64 as little-endian in first 8 bytes
    let mut delta_in = [0u8; 32];
    delta_in[..8].copy_from_slice(&amount_in.to_le_bytes());
    let args = dex_pinocchio_cpi::bonkswap::SwapArgs {
        delta_in,
        price_limit: [0u8; 32], // no limit
        x_to_y: true,           // direction set by account ordering from off-chain
    };
    dex_pinocchio_cpi::bonkswap::swap(&swap_accounts, &args, &[])
}
```

**Note:** Bonkswap's `x_to_y` direction is determined by account ordering from off-chain. The executor places accounts in the correct order so `x_to_y: true` is always correct. If this assumption is wrong, add `is_a_to_b` param — but verify with dex-pinocchio-cpi first.

**Step 5: Implement swap_meteora_damm_v2**

```rust
fn swap_meteora_damm_v2(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    let swap_accounts = dex_pinocchio_cpi::meteora_damm_v2::SwapAccounts {
        pool_authority: &accounts[0],
        pool: &accounts[1],
        input_token_account: &accounts[2],
        output_token_account: &accounts[3],
        token_a_vault: &accounts[4],
        token_b_vault: &accounts[5],
        token_a_mint: &accounts[6],
        token_b_mint: &accounts[7],
        payer: &accounts[8],
        token_a_program: &accounts[9],
        token_b_program: &accounts[10],
        referral_token_account: &accounts[11],
        event_authority: &accounts[12],
        program: &accounts[13],
    };
    // params is [u8; 32] — encode amount_in as u64 LE in first 8 bytes
    // Exact encoding depends on Meteora's swap params format
    let mut params = [0u8; 32];
    params[..8].copy_from_slice(&amount_in.to_le_bytes());
    let args = dex_pinocchio_cpi::meteora_damm_v2::SwapArgs { params };
    dex_pinocchio_cpi::meteora_damm_v2::swap(&swap_accounts, &args, &[])
}
```

**Important:** The Bonkswap `delta_in` and Meteora `params` encodings are [u8; 32]. The implementer MUST read the actual dex-pinocchio-cpi source to verify the correct byte encoding. The u64 LE in first 8 bytes is an educated guess — verify before committing.

**Step 6: Verify it compiles**

Run: `cd program && cargo check`

**Step 7: Commit**

```bash
git add program/src/swap.rs program/src/accounts.rs
git commit -m "feat(program): implement all 7 DEX CPI wrappers with amount_in"
```

---

## Task 4: Off-chain — remove flashloan flag from encoder + update account layout

**Files:**
- Modify: `executor/src/tx_builder.rs`
- Modify: `executor/src/config.rs`

**Step 1: Update encode_instruction_data — remove flashloan bit**

In `executor/src/tx_builder.rs`, in `encode_instruction_data()`:

Remove the line:
```rust
// TODO: set flashloan bit from config
```

The flags byte is now: bit0=buy_a_to_b, bit1=sell_a_to_b, bit2=buy_2022, bit3=sell_2022. No bit4.

**Step 2: Add flashloan config**

In `executor/src/config.rs`, add to `ExecutorConfig`:
```rust
pub flashloan: Option<FlashloanConfig>,
```

Add new struct:
```rust
#[derive(Debug, Deserialize)]
pub struct FlashloanConfig {
    pub enabled: bool,
}
```

**Step 3: Update config.toml**

Add to `executor/config.toml`:
```toml
[executor.flashloan]
enabled = false
```

**Step 4: Verify it compiles**

Run: `cd executor && cargo check`

**Step 5: Commit**

```bash
git add executor/
git commit -m "refactor(executor): remove flashloan bit from instruction encoding, add flashloan config"
```

---

## Task 5: Off-chain — implement build_account_metas

**Files:**
- Modify: `executor/src/tx_builder.rs`

**Step 1: Implement build_account_metas**

Replace the empty `Vec::new()` stub with actual account construction:

```rust
fn build_account_metas(&self, opp: &Opportunity) -> Vec<AccountMeta> {
    let hop_count = opp.route.hops.len();
    let mut metas = Vec::new();

    // Fixed header (8 accounts)
    metas.push(AccountMeta::new(self.payer_pubkey, true));           // [0] Payer (signer)
    metas.push(AccountMeta::new_readonly(opp.route.base_mint, false)); // [1] Base mint
    // [2] User base token ATA — derive from payer + base_mint
    let user_base_ata = spl_associated_token_account::get_associated_token_address(
        &self.payer_pubkey, &opp.route.base_mint,
    );
    metas.push(AccountMeta::new(user_base_ata, false));
    // [3] Fee collector (random)
    let fee_collector = crate::anti_fp::random_fee_collector(&self.fee_collectors);
    metas.push(AccountMeta::new(fee_collector, false));
    // [4-7] Program IDs
    metas.push(AccountMeta::new_readonly(spl_token::ID, false));
    metas.push(AccountMeta::new_readonly(spl_token_2022::ID, false));
    metas.push(AccountMeta::new_readonly(spl_associated_token_account::ID, false));
    metas.push(AccountMeta::new_readonly(solana_sdk::system_program::ID, false));

    // Intermediate token accounts: 3 × (hop_count - 1)
    for i in 1..hop_count {
        let snapshot = &opp.pool_snapshots[i - 1];
        // Intermediate mint is the output of previous hop
        let intermediate_mint = if snapshot.is_a_to_b {
            snapshot.mint_b
        } else {
            snapshot.mint_a
        };
        metas.push(AccountMeta::new_readonly(intermediate_mint, false));   // mint
        metas.push(AccountMeta::new_readonly(spl_token::ID, false));       // token program
        let intermediate_ata = spl_associated_token_account::get_associated_token_address(
            &self.payer_pubkey, &intermediate_mint,
        );
        metas.push(AccountMeta::new(intermediate_ata, false));             // user ATA
    }

    // Per-hop pool accounts from snapshots
    for snapshot in &opp.pool_snapshots {
        for acct in &snapshot.accounts {
            metas.push(AccountMeta::new(*acct, false));  // writable by default
        }
    }

    metas
}
```

**Note:** The `payer_pubkey` field needs to be added to `TxBuilder`. The `spl_associated_token_account::get_associated_token_address` and `spl_token::ID` may need additional dependencies. The implementer must:
1. Add `spl-token` and `spl-associated-token-account` to `executor/Cargo.toml` if not present
2. Check if these crates are compatible with solana-sdk v3
3. If not, use `Pubkey::find_program_address` to derive ATAs manually
4. Add `payer_pubkey: Pubkey` field to `TxBuilder`, set in `from_config()` or passed separately

**Step 2: Verify it compiles**

Run: `cd executor && cargo check`

**Step 3: Commit**

```bash
git add executor/
git commit -m "feat(executor): implement build_account_metas with full account layout"
```

---

## Task 6: Off-chain — MarginFi flashloan integration

**Files:**
- Create: `executor/src/marginfi.rs`
- Modify: `executor/src/lib.rs`
- Modify: `executor/src/tx_builder.rs`
- Modify: `executor/src/executor.rs`

**Step 1: Create marginfi.rs**

Create `executor/src/marginfi.rs` with:

```rust
use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    sysvar,
};
use std::collections::HashMap;
use std::str::FromStr;

/// MarginFi V2 program ID (mainnet)
pub const MARGINFI_PROGRAM_ID: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";

/// Discriminators (SHA256("global:<name>")[0..8]) — verify at startup
pub const START_FLASHLOAN_DISC: [u8; 8] = [14, 131, 33, 220, 81, 186, 180, 107];
pub const END_FLASHLOAN_DISC: [u8; 8] = [105, 124, 201, 106, 153, 2, 8, 156];

// Borrow/repay discriminators — implementer must verify from MarginFi IDL
pub const BORROW_DISC: [u8; 8] = [0; 8]; // TODO: compute SHA256("global:lending_account_borrow")[0..8]
pub const REPAY_DISC: [u8; 8] = [0; 8];  // TODO: compute SHA256("global:lending_account_repay")[0..8]

pub struct BankInfo {
    pub address: Pubkey,
    pub oracle: Pubkey,
    pub vault: Pubkey,
    pub vault_authority: Pubkey,
}

pub struct MarginFiState {
    pub program_id: Pubkey,
    pub group: Pubkey,
    pub account: Pubkey,
    pub banks: HashMap<Pubkey, BankInfo>,  // mint → bank info
}

impl MarginFiState {
    /// Initialize by querying all MarginFi state from RPC.
    /// Called once at executor startup.
    pub async fn init(rpc: &RpcClient, payer: &Pubkey) -> Result<Self> {
        let program_id = Pubkey::from_str(MARGINFI_PROGRAM_ID)?;

        // TODO: Query MarginFi group (well-known mainnet address)
        // TODO: Query/create marginfi_account for payer
        // TODO: Query all banks, index by mint
        // TODO: Deserialize bank data for oracle, vault, vault_authority

        Ok(Self {
            program_id,
            group: Pubkey::default(),   // TODO: replace with actual
            account: Pubkey::default(), // TODO: replace with actual
            banks: HashMap::new(),      // TODO: populate
        })
    }

    /// Build start_flashloan instruction
    pub fn build_start_flashloan_ix(&self, authority: &Pubkey, end_index: u64) -> Instruction {
        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&START_FLASHLOAN_DISC);
        data.extend_from_slice(&end_index.to_le_bytes());

        Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.account, false),
                AccountMeta::new_readonly(*authority, true),
                AccountMeta::new_readonly(sysvar::instructions::ID, false),
            ],
            data,
        }
    }

    /// Build end_flashloan instruction
    pub fn build_end_flashloan_ix(&self, authority: &Pubkey, bank_oracle_pairs: &[(Pubkey, Pubkey)]) -> Instruction {
        let data = END_FLASHLOAN_DISC.to_vec();

        let mut accounts = vec![
            AccountMeta::new(self.account, false),
            AccountMeta::new_readonly(*authority, true),
        ];
        // Remaining accounts for health check: bank + oracle per active balance
        for (bank, oracle) in bank_oracle_pairs {
            accounts.push(AccountMeta::new(*bank, false));
            accounts.push(AccountMeta::new_readonly(*oracle, false));
        }

        Instruction {
            program_id: self.program_id,
            accounts,
            data,
        }
    }

    /// Build borrow instruction
    pub fn build_borrow_ix(&self, authority: &Pubkey, mint: &Pubkey, dest_ata: &Pubkey, amount: u64) -> Result<Instruction> {
        let bank = self.banks.get(mint).ok_or_else(|| anyhow::anyhow!("No bank for mint {}", mint))?;

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&BORROW_DISC);
        data.extend_from_slice(&amount.to_le_bytes());

        Ok(Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new_readonly(self.group, false),
                AccountMeta::new(self.account, false),
                AccountMeta::new_readonly(*authority, true),
                AccountMeta::new(bank.address, false),
                AccountMeta::new(*dest_ata, false),
                AccountMeta::new_readonly(bank.vault_authority, false),
                AccountMeta::new(bank.vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data,
        })
    }

    /// Build repay instruction
    pub fn build_repay_ix(&self, authority: &Pubkey, mint: &Pubkey, source_ata: &Pubkey, amount: u64) -> Result<Instruction> {
        let bank = self.banks.get(mint).ok_or_else(|| anyhow::anyhow!("No bank for mint {}", mint))?;

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&REPAY_DISC);
        data.extend_from_slice(&amount.to_le_bytes());

        Ok(Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new_readonly(self.group, false),
                AccountMeta::new(self.account, false),
                AccountMeta::new_readonly(*authority, true),
                AccountMeta::new(bank.address, false),
                AccountMeta::new(*source_ata, false),
                AccountMeta::new(bank.vault, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ],
            data,
        })
    }
}
```

**Step 2: Add `pub mod marginfi;` to lib.rs**

**Step 3: Wire MarginFi into tx_builder**

Add to `TxBuilder`:
```rust
flashloan_enabled: bool,
marginfi_state: Option<Arc<MarginFiState>>,
```

In `build()`, if `flashloan_enabled`:
```rust
// Build 5-instruction flashloan tx
let start_ix = marginfi.build_start_flashloan_ix(&payer_pubkey, 4); // end_flashloan at IX 4
let borrow_ix = marginfi.build_borrow_ix(&payer_pubkey, &base_mint, &user_base_ata, opp.amount_in)?;
let arb_ix = self.build_arb_instruction(opp);
let repay_ix = marginfi.build_repay_ix(&payer_pubkey, &base_mint, &user_base_ata, opp.amount_in)?;
let end_ix = marginfi.build_end_flashloan_ix(&payer_pubkey, &[(bank.address, bank.oracle)]);
// ixs = [start, borrow, arb, repay, end]
```

**Step 4: Wire MarginFi init into executor.rs**

In `Executor::new()`, if flashloan enabled:
```rust
let marginfi_state = if config.executor.flashloan.as_ref().map_or(false, |f| f.enabled) {
    Some(Arc::new(MarginFiState::init(&rpc, &payer.pubkey()).await?))
} else {
    None
};
```

**Step 5: Verify it compiles**

Run: `cd executor && cargo check`

**Step 6: Commit**

```bash
git add executor/
git commit -m "feat(executor): add MarginFi flashloan integration (off-chain tx assembly)"
```

---

## Task 7: Jito gRPC sender — embed proto + implement

**Files:**
- Create: `executor/proto/packet.proto`
- Create: `executor/proto/shared.proto`
- Create: `executor/proto/bundle.proto`
- Create: `executor/proto/auth.proto`
- Create: `executor/proto/searcher.proto`
- Create: `executor/build.rs`
- Modify: `executor/Cargo.toml` (add tonic-build, prost, prost-types)
- Modify: `executor/src/sender/jito.rs`

**Step 1: Create proto files**

Create `executor/proto/packet.proto`:
```protobuf
syntax = "proto3";
package packet;

message PacketFlags {
  bool discard = 1;
  bool forwarded = 2;
  bool repair = 3;
  bool simple_vote_tx = 4;
  bool tracer_packet = 5;
  bool from_staked_node = 6;
}

message Meta {
  uint64 size = 1;
  string addr = 2;
  uint32 port = 3;
  PacketFlags flags = 4;
  uint64 sender_stake = 5;
}

message Packet {
  bytes data = 1;
  Meta meta = 2;
}
```

Create `executor/proto/shared.proto`:
```protobuf
syntax = "proto3";
package shared;

import "google/protobuf/timestamp.proto";

message Header {
  google.protobuf.Timestamp ts = 1;
}
```

Create `executor/proto/bundle.proto`:
```protobuf
syntax = "proto3";
package bundle;

import "packet.proto";
import "shared.proto";

message Bundle {
  shared.Header header = 2;
  repeated packet.Packet packets = 3;
}
```

Create `executor/proto/auth.proto`:
```protobuf
syntax = "proto3";
package auth;

message Role {
  enum Value {
    RELAYER = 0;
    SEARCHER = 1;
    VALIDATOR = 2;
    SHREDSTREAM_SUBSCRIBER = 3;
  }
}

message GenerateAuthChallengeRequest {
  Role.Value role = 1;
  string pubkey = 2;
}

message GenerateAuthChallengeResponse {
  string challenge = 1;
}

message GenerateAuthTokensRequest {
  string challenge = 1;
  string signed_challenge = 2;
  string client_pubkey = 3;
}

message GenerateAuthTokensResponse {
  Token access_token = 1;
  Token refresh_token = 2;
}

message RefreshAccessTokenRequest {
  string refresh_token = 1;
}

message RefreshAccessTokenResponse {
  Token access_token = 1;
}

message Token {
  string value = 1;
  google.protobuf.Timestamp expires_at_utc = 2;
}

import "google/protobuf/timestamp.proto";

service AuthService {
  rpc GenerateAuthChallenge(GenerateAuthChallengeRequest) returns (GenerateAuthChallengeResponse) {}
  rpc GenerateAuthTokens(GenerateAuthTokensRequest) returns (GenerateAuthTokensResponse) {}
  rpc RefreshAccessToken(RefreshAccessTokenRequest) returns (RefreshAccessTokenResponse) {}
}
```

Create `executor/proto/searcher.proto`:
```protobuf
syntax = "proto3";
package searcher;

import "bundle.proto";

message SendBundleRequest {
  bundle.Bundle bundle = 1;
}

message SendBundleResponse {
  string uuid = 1;
}

service SearcherService {
  rpc SendBundle(SendBundleRequest) returns (SendBundleResponse) {}
}
```

**Step 2: Create build.rs**

Create `executor/build.rs`:
```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false) // client only
        .compile_protos(
            &[
                "proto/packet.proto",
                "proto/shared.proto",
                "proto/bundle.proto",
                "proto/auth.proto",
                "proto/searcher.proto",
            ],
            &["proto/"],
        )?;
    Ok(())
}
```

**Step 3: Add build dependencies to Cargo.toml**

Add to `executor/Cargo.toml`:
```toml
prost = "0.13"
prost-types = "0.13"

[build-dependencies]
tonic-build = "0.12"
```

Note: `tonic` is already in dependencies. Make sure tonic and tonic-build versions are compatible.

**Step 4: Implement JitoSender**

Replace `executor/src/sender/jito.rs`:
```rust
use anyhow::Result;
use solana_sdk::transaction::VersionedTransaction;
use tonic::transport::Channel;

// Generated proto modules
pub mod proto {
    pub mod packet { tonic::include_proto!("packet"); }
    pub mod shared { tonic::include_proto!("shared"); }
    pub mod bundle { tonic::include_proto!("bundle"); }
    pub mod searcher {
        tonic::include_proto!("searcher");
    }
    pub mod auth { tonic::include_proto!("auth"); }
}

use proto::searcher::searcher_service_client::SearcherServiceClient;

#[derive(Clone)]
pub struct JitoSender {
    endpoint: String,
    client: Option<SearcherServiceClient<Channel>>,
}

impl JitoSender {
    pub fn new(endpoint: String) -> Self {
        Self { endpoint, client: None }
    }

    /// Pre-connect gRPC channel at startup
    pub async fn connect(&mut self) -> Result<()> {
        let channel = Channel::from_shared(self.endpoint.clone())?
            .connect()
            .await?;
        self.client = Some(SearcherServiceClient::new(channel));
        log::info!("Jito gRPC connected: {}", self.endpoint);
        Ok(())
    }

    /// Send a bundle containing a single transaction
    pub async fn send_bundle(&self, tx: &VersionedTransaction) -> Result<()> {
        let client = self.client.as_ref()
            .ok_or_else(|| anyhow::anyhow!("Jito not connected"))?;

        let tx_bytes = bincode::serialize(tx)?;
        let packet = proto::packet::Packet {
            data: tx_bytes.clone(),
            meta: Some(proto::packet::Meta {
                size: tx_bytes.len() as u64,
                addr: String::new(),
                port: 0,
                flags: None,
                sender_stake: 0,
            }),
        };
        let bundle = proto::bundle::Bundle {
            header: None,
            packets: vec![packet],
        };
        let request = proto::searcher::SendBundleRequest {
            bundle: Some(bundle),
        };

        let mut client = client.clone();
        let _response = client.send_bundle(request).await?;
        log::debug!("Jito bundle sent to {}", self.endpoint);
        Ok(())
    }
}
```

**Note:** Auth flow (GenerateAuthChallenge → sign → GenerateAuthTokens) should be done at startup and token refreshed periodically. For now, the basic send_bundle is implemented. The implementer should add auth as a follow-up or include it in connect().

**Step 5: Update sender/mod.rs**

Update `MultiSender::from_config` to call `connect()` on each JitoSender. Since `connect` is async, `from_config` should become `async fn from_config` or a separate `init()` method.

**Step 6: Verify it compiles**

Run: `cd executor && cargo check`

**Step 7: Commit**

```bash
git add executor/
git commit -m "feat(executor): implement Jito gRPC sender with embedded proto"
```

---

## Task 8: Flashblock + Astralane sender implementation

**Files:**
- Modify: `executor/src/sender/flashblock.rs`
- Modify: `executor/src/sender/astralane.rs`

**Step 1: Implement FlashblockSender**

Replace `executor/src/sender/flashblock.rs`:
```rust
use anyhow::Result;
use base64::Engine as _;
use solana_sdk::transaction::VersionedTransaction;

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

    pub async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<()> {
        let tx_bytes = bincode::serialize(tx)?;
        let tx_base64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [tx_base64, {"encoding": "base64"}]
        });

        let resp = self.client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Flashblock HTTP {}: {}", status, text);
        }

        log::debug!("Flashblock sent to {}", self.endpoint);
        Ok(())
    }
}
```

**Step 2: Implement AstralaneSender**

Replace `executor/src/sender/astralane.rs`:
```rust
use anyhow::Result;
use base64::Engine as _;
use solana_sdk::transaction::VersionedTransaction;

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

    pub async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<()> {
        let tx_bytes = bincode::serialize(tx)?;
        let tx_base64 = base64::engine::general_purpose::STANDARD.encode(&tx_bytes);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendTransaction",
            "params": [tx_base64, {"encoding": "base64"}]
        });

        let resp = self.client
            .post(&self.endpoint)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("Astralane HTTP {}: {}", status, text);
        }

        log::debug!("Astralane sent to {}", self.endpoint);
        Ok(())
    }
}
```

**Step 3: Add `bincode` and `serde_json` to Cargo.toml if not present**

```toml
bincode = "1.3"
serde_json = "1.0"
```

**Step 4: Update sender/mod.rs — change Transaction to VersionedTransaction**

All references to `solana_sdk::transaction::Transaction` in sender/mod.rs → `VersionedTransaction`.

**Step 5: Verify it compiles**

Run: `cd executor && cargo check`

**Step 6: Commit**

```bash
git add executor/
git commit -m "feat(executor): implement Flashblock and Astralane JSON-RPC senders"
```

---

## Task 9: ALT + VersionedTransaction migration

**Files:**
- Modify: `executor/src/tx_builder.rs`
- Modify: `executor/src/alt.rs`
- Modify: `executor/src/executor.rs`
- Modify: `executor/Cargo.toml`

**Step 1: Expand alt.rs**

```rust
use anyhow::Result;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    pubkey::Pubkey,
};
use solana_client::nonblocking::rpc_client::RpcClient;

pub struct Tier0Alt {
    pub address: Pubkey,
    pub account: AddressLookupTableAccount,
}

impl Tier0Alt {
    /// Load ALT from RPC at startup, cache in memory
    pub async fn load(rpc: &RpcClient, address: Pubkey) -> Result<Self> {
        let account_data = rpc.get_account(&address).await?;
        let lookup_table = AddressLookupTableAccount {
            key: address,
            addresses: Self::deserialize_alt(&account_data.data)?,
        };
        log::info!("ALT loaded: {} ({} entries)", address, lookup_table.addresses.len());
        Ok(Self {
            address,
            account: lookup_table,
        })
    }

    fn deserialize_alt(data: &[u8]) -> Result<Vec<Pubkey>> {
        // ALT data: 56 bytes header + 32 bytes per address
        if data.len() < 56 {
            anyhow::bail!("ALT data too short");
        }
        let addresses_data = &data[56..];
        let count = addresses_data.len() / 32;
        let mut addresses = Vec::with_capacity(count);
        for i in 0..count {
            let start = i * 32;
            let pubkey = Pubkey::try_from(&addresses_data[start..start + 32])
                .map_err(|e| anyhow::anyhow!("Invalid pubkey in ALT: {:?}", e))?;
            addresses.push(pubkey);
        }
        Ok(addresses)
    }
}
```

**Step 2: Migrate tx_builder to VersionedTransaction**

In `executor/src/tx_builder.rs`:

Change `TxPair`:
```rust
use solana_sdk::transaction::VersionedTransaction;

pub struct TxPair {
    pub jito_tx: Option<VersionedTransaction>,
    pub swqos_tx: Option<VersionedTransaction>,
}
```

Add `alt: Arc<Tier0Alt>` field to `TxBuilder`.

In `build()`, replace `Transaction::new_signed_with_payer` with:
```rust
use solana_sdk::message::v0::MessageV0;
use solana_sdk::message::VersionedMessage;

let msg = MessageV0::try_compile(
    &payer.pubkey(),
    &ixs,
    &[self.alt.account.clone()],
    recent_blockhash,
)?;
let versioned_msg = VersionedMessage::V0(msg);
let tx = VersionedTransaction::try_new(versioned_msg, &[payer])?;
```

**Step 3: Load ALT in executor startup**

In `Executor::new()`:
```rust
let alt_address: Pubkey = config.executor.alt_address.parse()?;
let alt = Arc::new(Tier0Alt::load(&rpc, alt_address).await?);
```

Pass ALT to TxBuilder.

**Step 4: Verify it compiles**

Run: `cd executor && cargo check`

**Step 5: Commit**

```bash
git add executor/
git commit -m "feat(executor): migrate to VersionedTransaction v0 with ALT support"
```

---

## Task 10: Parallel signing with rayon

**Files:**
- Modify: `executor/src/tx_builder.rs`
- Modify: `executor/Cargo.toml`

**Step 1: Add rayon dependency**

Add to `executor/Cargo.toml`:
```toml
rayon = "1.10"
```

**Step 2: Implement parallel signing in build()**

In `tx_builder.rs`, replace sequential signing with:

```rust
use rayon;

// After building both messages:
let (jito_tx, swqos_tx) = rayon::join(
    || {
        if !self.jito_enabled { return None; }
        // ... build jito message ...
        let msg = MessageV0::try_compile(&payer.pubkey(), &jito_ixs, &[self.alt.account.clone()], recent_blockhash).ok()?;
        VersionedTransaction::try_new(VersionedMessage::V0(msg), &[payer]).ok()
    },
    || {
        if !self.swqos_enabled { return None; }
        // ... build swqos message ...
        let msg = MessageV0::try_compile(&payer.pubkey(), &swqos_ixs, &[self.alt.account.clone()], recent_blockhash).ok()?;
        VersionedTransaction::try_new(VersionedMessage::V0(msg), &[payer]).ok()
    },
);

TxPair { jito_tx, swqos_tx }
```

**Note:** The `payer: &Keypair` needs to be `Send + Sync` for rayon. `Keypair` should be wrapped in `Arc` or passed by reference within `std::thread::scope`. The implementer must verify that `Keypair` (or its signing method) is thread-safe.

**Step 3: Verify it compiles**

Run: `cd executor && cargo check`

**Step 4: Commit**

```bash
git add executor/
git commit -m "feat(executor): parallel ed25519 signing with rayon::join"
```

---

## Task 11: Pre-initialize all connections and caches

**Files:**
- Modify: `executor/src/executor.rs`
- Modify: `executor/src/sender/mod.rs`
- Modify: `executor/src/anti_fp.rs`

**Step 1: Pre-parse Jito tip accounts**

In `executor/src/anti_fp.rs`, change JITO_TIP_ACCOUNTS from `[&str; 8]` to pre-parsed `[Pubkey; 8]`:
```rust
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::LazyLock;

static JITO_TIP_PUBKEYS: LazyLock<[Pubkey; 8]> = LazyLock::new(|| {
    [
        Pubkey::from_str("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5").unwrap(),
        Pubkey::from_str("HFqU5x63VTqvQss8hp11i4bVqkfRtQ7NmXwkiNPLz4xG").unwrap(),
        Pubkey::from_str("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY").unwrap(),
        Pubkey::from_str("ADaUMid9yfUytqMBgopwjb2DTLSLo4G9hp12gJZTm1Xw").unwrap(),
        Pubkey::from_str("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh").unwrap(),
        Pubkey::from_str("ADuUkR4vqLUMWXxW9gh6D6L8pMSga2WWP4N4G2Cj6ixc").unwrap(),
        Pubkey::from_str("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL6JR3").unwrap(),
        Pubkey::from_str("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT").unwrap(),
    ]
});

pub fn random_tip_account() -> Pubkey {
    let mut rng = rand::thread_rng();
    let idx = rng.gen_range(0..8);
    JITO_TIP_PUBKEYS[idx]
}
```

**Step 2: Pre-connect all senders at startup**

Change `MultiSender::from_config` to `async fn from_config` or add `async fn init(&mut self)`:
```rust
impl MultiSender {
    pub async fn from_config(config: &ExecutorConfigFile) -> Self {
        // ... create senders as before ...
        // Pre-connect Jito gRPC channels
        for sender in &mut jito_senders {
            if let Err(e) = sender.connect().await {
                log::warn!("Failed to pre-connect Jito {}: {}", sender.endpoint, e);
            }
        }
        // reqwest::Client already has connection pool, DNS will resolve on first use
        // Optionally: send a warm-up request to each HTTP endpoint
        // ...
    }
}
```

**Step 3: Update Executor::new() to be async-aware**

Ensure all pre-initialization happens in `Executor::new()` or a separate `init()` before the main loop starts.

**Step 4: Verify it compiles**

Run: `cd executor && cargo check`

**Step 5: Commit**

```bash
git add executor/
git commit -m "feat(executor): pre-initialize all connections, caches, and parsed constants"
```

---

## Task 12: Fix compilation and verify all crates

**Files:**
- Any files with compilation issues

**Step 1: Check program crate**

```bash
cd program && cargo check 2>&1
```

Fix all errors.

**Step 2: Check executor crate**

```bash
cd executor && cargo check --all-targets 2>&1
```

Fix all errors. Warnings are OK.

**Step 3: Check engine crate (should be unaffected)**

```bash
cd engine && cargo check 2>&1
```

**Step 4: Commit fixes**

```bash
git commit -am "fix: resolve all compilation issues across crates"
```

---

## Task 13: Update full pipeline example

**Files:**
- Modify: `executor/examples/full_pipeline.rs`

**Step 1: Update example to reflect new architecture**

Update the example to show:
- Executor initialization with MarginFi state pre-query (if flashloan enabled)
- ALT pre-loading
- Sender pre-connection
- The full startup → run flow

**Step 2: Verify example compiles**

```bash
cd executor && cargo check --all-targets
```

**Step 3: Commit**

```bash
git add executor/examples/
git commit -m "docs(executor): update full pipeline example for new architecture"
```
