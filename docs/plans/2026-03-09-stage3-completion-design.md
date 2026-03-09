# Stage 3 Completion Design

> Fill all 23 scaffold gaps in the on-chain program and off-chain executor. Refactor flashloan from on-chain CPI to off-chain transaction-level instructions.

## Core Principle

All immutable data (MarginFi accounts, ALT contents, gRPC channels, HTTP clients) is pre-loaded at startup. The arbitrage hot path has zero RPC queries, zero DNS lookups, zero allocations where avoidable.

## Design Decisions

| Decision | Choice |
|----------|--------|
| Flashloan architecture | Off-chain top-level instructions (all Solana flashloan protocols block CPI) |
| Flashloan provider | MarginFi (0% fee) |
| amount_in strategy | First hop receives amount_in, subsequent hops receive 0 (= use full balance) |
| Jito gRPC | Self-embedded proto + tonic-build (minimal, no third-party crate) |
| Flashblock/Astralane | Standard JSON-RPC sendTransaction |
| Transaction format | VersionedTransaction v0 with Address Lookup Table |
| Signing | rayon::join parallel ed25519 for two variants |
| Token-2022 flags | Deferred (PoolSnapshot lacks is_token_2022 field, needs Stage 2 change) |

---

## Section 1: Flashloan Architecture Refactor

### Why

All production Solana flashloan protocols (MarginFi, Kamino, Solend, Flash Loan Mastery) block CPI via `get_stack_height()` and/or instruction introspection. Flash borrow/repay must be top-level transaction instructions.

### On-chain changes (delete/simplify)

- Delete `program/src/flashloan.rs`
- Remove `mod flashloan;` from `lib.rs`
- Remove from `accounts.rs`: `FLASHLOAN_ACCOUNT_COUNT`, `use_flashloan` field in `SwapInstruction`, flashloan bit (bit4) from flags parsing, `use_flashloan` param from `pool_accounts_start()`
- Remove from `swap.rs`: `flashloan_slice()` helper, flashloan borrow/repay calls, `FLASHLOAN_ACCOUNT_COUNT` import

### Off-chain changes (new MarginFi integration)

New file `executor/src/marginfi.rs`:

```
MarginFi Program ID: MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA

start_flashloan discriminator: SHA256("global:lending_account_start_flashloan")[0..8]
end_flashloan discriminator: SHA256("global:lending_account_end_flashloan")[0..8]
(Verify by computing at build time or in init)
```

**MarginFiState** (pre-queried at startup):
```rust
pub struct MarginFiState {
    pub group: Pubkey,
    pub account: Pubkey,
    pub banks: HashMap<Pubkey, BankInfo>,  // mint → bank info
}

pub struct BankInfo {
    pub address: Pubkey,
    pub oracle: Pubkey,
    pub vault: Pubkey,
    pub vault_authority: Pubkey,
}
```

Startup init flow:
1. Query MarginFi group (well-known mainnet address)
2. Query/create marginfi_account for payer wallet
3. Query all banks, index by mint
4. Deserialize each bank for oracle, vault, vault_authority
5. Cache in MarginFiState

**Flashloan transaction layout** (when enabled):
```
IX 0: MarginFi start_flashloan       (3 accounts)
IX 1: MarginFi borrow                (8 accounts)
IX 2: arb_program.execute            (swap + verify profit)
IX 3: MarginFi repay                 (7 accounts)
IX 4: MarginFi end_flashloan         (2 + N accounts)
```

Config simplified to:
```toml
[executor.flashloan]
enabled = false
```

All MarginFi account addresses auto-queried at startup.

---

## Section 2: On-chain — Wire amount_in + Complete DEX CPIs

### amount_in strategy

- First hop: pass `ix.amount_in`
- Subsequent hops: pass `0` (use full token account balance)
- This works because intermediate ATAs start at 0 balance, so post-swap balance = output amount

### dispatch_swap signature change

```rust
fn dispatch_swap(hop: &HopInfo, pool_accounts: &[AccountView], amount_in: u64) -> ProgramResult
```

Call site in execute():
```rust
for i in 0..ix.hop_count {
    let amount = if i == 0 { ix.amount_in } else { 0 };
    dispatch_swap(hop, pool_slice, amount)?;
}
```

### 3 existing Raydium wrappers — wire amount_in

- `swap_raydium_amm`: `args.amount_in = amount_in`
- `swap_raydium_cp`: `args.amount_in = amount_in`
- `swap_raydium_clmm`: `args.amount = amount_in`

### 4 DEX stubs to implement

Read actual account structs from dex-pinocchio-cpi:

| DEX | Module | CPI fn | Direction |
|-----|--------|--------|-----------|
| PumpFun | `pump_fun` | `buy()` / `sell()` | is_a_to_b → buy or sell |
| PumpSwap | `pump_fun_amm` | `buy()` / `sell()` | is_a_to_b → buy or sell |
| Bonkswap | `bonkswap` | `swap()` | args.x_to_y = direction |
| Meteora DAMM V2 | `meteora_damm_v2` | `swap()` | direction in args |

Each follows the same pattern: construct Accounts struct + Args struct → call CPI fn.

---

## Section 3: Off-chain — build_account_metas

Account layout (flashloan removed):

```
[Fixed Header — 8 accounts]
  [0]  Payer (signer, writable)
  [1]  Base mint (readonly)
  [2]  User base token ATA (writable)
  [3]  Fee collector (writable, random from config)
  [4]  SPL Token Program (readonly)
  [5]  Token-2022 Program (readonly)
  [6]  Associated Token Program (readonly)
  [7]  System Program (readonly)

[Per Intermediate Token — 3 × (hop_count - 1)]
  [+0] Token mint (readonly)
  [+1] Token program — SPL or Token-2022 (readonly)
  [+2] User token ATA (writable)

[Per Hop Pool Accounts — from PoolSnapshot.accounts]
  Sequential by hop order, accounts from each snapshot
```

Implementation:
1. Header: payer from keypair, fixed program IDs (pre-resolved at startup), random fee collector
2. Intermediate tokens: derive from PoolSnapshot mint_a/mint_b + direction
3. Pool accounts: use PoolSnapshot.accounts directly (Stage 2 engine orders them correctly)

---

## Section 4: Off-chain — MarginFi Instruction Builders

New `executor/src/marginfi.rs` functions:

```rust
fn build_start_flashloan_ix(state: &MarginFiState, payer: &Pubkey, end_index: u64) -> Instruction
fn build_borrow_ix(state: &MarginFiState, payer: &Pubkey, mint: &Pubkey, dest_ata: &Pubkey) -> Instruction
fn build_repay_ix(state: &MarginFiState, payer: &Pubkey, mint: &Pubkey, source_ata: &Pubkey) -> Instruction
fn build_end_flashloan_ix(state: &MarginFiState, payer: &Pubkey, bank_oracle_pairs: &[(Pubkey, Pubkey)]) -> Instruction
```

tx_builder changes:
- If flashloan_enabled: build 5-instruction tx (MarginFi start → borrow → arb → repay → end)
- If not: build 1-instruction tx (arb only)
- Both modes generate Jito + SWQoS variants

---

## Section 5: Sender Implementation

### Jito gRPC

- Embed minimal proto in `executor/proto/bundle.proto` (SendBundleRequest/Response only)
- `build.rs` with tonic-build
- Pre-connect gRPC channels at startup (one per region endpoint)
- send_bundle: serialize tx to base58/base64, wrap in Bundle, send via pre-established channel

### Flashblock (JSON-RPC)

- `reqwest::Client` pre-created at startup (connection pool)
- send_transaction: serialize tx base64, POST JSON-RPC `sendTransaction`
- `Authorization: Bearer <api_key>` header

### Astralane (JSON-RPC)

- Same pattern as Flashblock
- API key in header or query param

All connections/clients/DNS pre-resolved at startup.

---

## Section 6: ALT + VersionedTransaction

- Replace `Transaction` (legacy) with `VersionedTransaction` v0
- Tier 0 static ALT (~25-30 entries): DEX program IDs, token programs, mints, Jito tips, MarginFi program, arb program
- ALT address in config.toml (created once manually)
- At startup: RPC query ALT account → deserialize → cache `AddressLookupTableAccount`
- `MessageV0::try_compile()` with ALT reference
- `VersionedTransaction::try_new()`
- TxPair fields: `Option<VersionedTransaction>`
- Senders update serialization accordingly

---

## Section 7: Parallel Signing

- Two variants (Jito + SWQoS) signed in parallel using `rayon::join`
- Not `tokio::spawn` — ed25519 is CPU-bound, should not block async runtime
- rayon thread pool pre-initialized at startup
- Expected: signing latency from ~100-160 μs → ~50-80 μs

```rust
let (jito_tx, swqos_tx) = rayon::join(
    || sign_versioned(jito_msg, payer),
    || sign_versioned(swqos_msg, payer),
);
```

---

## On-chain account layout update

After flashloan removal:

```rust
// accounts.rs changes:
// - Remove: FLASHLOAN_ACCOUNT_COUNT
// - Remove: use_flashloan from SwapInstruction
// - Remove: use_flashloan param from pool_accounts_start()
// - Remove: bit4 (flashloan) from flags parsing
// - Flags byte becomes: bit0=buy_a_to_b, bit1=sell_a_to_b, bit2=buy_2022, bit3=sell_2022

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

## Off-chain instruction encoding update

```
[0]     discriminator (hop_count - 2)
[1]     buy_dex_type
[2]     sell_dex_type
[3]     flags (bit0=buy_a_to_b, bit1=sell_a_to_b, bit2=buy_2022, bit3=sell_2022)
            (bit4 removed — no flashloan flag)
[4+]    mid_dex + mid_flags (for 3/4-hop)
[N..N+8]   amount_in (u64 LE)
[N+8..N+12] min_profit (u32 LE)
```
