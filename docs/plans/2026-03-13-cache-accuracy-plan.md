# Cache 準確度改善 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 減少假套利機會，提升 cache 與鏈上狀態的一致性，在發送前過濾不可獲利的交易。

**Architecture:** 四項獨立改善：(1) 事件驅動訂閱更新取代 2 秒 polling，(2) simulateTransaction 在發送前驗證，(3) CLMM tick arrays 從 3 擴展到 7，(4) 動態 slot 新鮮度檢查過濾 stale 路徑。

**Tech Stack:** Rust, tokio (Notify), solana-client (simulateTransaction), yellowstone-grpc

---

## Task 1: 事件驅動訂閱 — PoolStreamer 加 Notify

**Files:**
- Modify: `solana-streamer/src/pool/streamer.rs`
- Modify: `solana-streamer/src/pool/cache.rs`

**Step 1: 在 PoolStreamer 加三個 Notify 欄位**

在 `solana-streamer/src/pool/streamer.rs` 的 `PoolStreamer` struct 加入：

```rust
use tokio::sync::Notify;

pub struct PoolStreamer {
    // ... existing fields ...
    /// Notify: new accounts pending gRPC subscription
    subscription_notify: Arc<Notify>,
    /// Notify: new vaults pending initial balance fetch
    vault_notify: Arc<Notify>,
    /// Notify: CLMM pools need tick array reload
    tick_reload_notify: Arc<Notify>,
}
```

在 `new()` 裡初始化：

```rust
subscription_notify: Arc::new(Notify::new()),
vault_notify: Arc::new(Notify::new()),
tick_reload_notify: Arc::new(Notify::new()),
```

加三個 public getter 給 main loop 用：

```rust
pub fn subscription_notify(&self) -> Arc<Notify> {
    self.subscription_notify.clone()
}
pub fn vault_notify(&self) -> Arc<Notify> {
    self.vault_notify.clone()
}
pub fn tick_reload_notify(&self) -> Arc<Notify> {
    self.tick_reload_notify.clone()
}
```

**Step 2: 在每個 push 點觸發對應 Notify**

`handle_discovery()` — push 到 pending_subscriptions 後：
```rust
self.subscription_notify.notify_one();
```

`handle_account_update()` — push vault 到 pending_subscriptions 後：
```rust
self.subscription_notify.notify_one();
```

`handle_account_update()` — push 到 pending_vaults 後：
```rust
self.vault_notify.notify_one();
```

**Step 3: 在 cache.rs 的 tick_reload_queue push 後觸發 Notify**

`PoolStateCache` 需要持有一個 `tick_reload_notify: Arc<Notify>`。

在 `PoolStateCache::new()` 加參數：
```rust
pub fn new(update_tx: mpsc::Sender<PoolUpdate>, tick_reload_notify: Arc<Notify>) -> Self {
    Self {
        // ... existing ...
        tick_reload_notify,
    }
}
```

在 `update_math()` 裡 push 到 `tick_reload_queue` 後：
```rust
queue.push(TickArrayReloadRequest { ... });
self.tick_reload_notify.notify_one();
```

**Step 4: 更新 PoolStreamer::new() 傳 notify 給 cache**

```rust
let tick_reload_notify = Arc::new(Notify::new());
let cache = Arc::new(PoolStateCache::new(update_tx, tick_reload_notify.clone()));
```

**Step 5: Build and verify**

Run: `cargo build -p solana-streamer-sdk`
Expected: 編譯成功（full_pipeline 會暫時報錯，Task 2 修）

**Step 6: Commit**

```bash
git add solana-streamer/src/pool/streamer.rs solana-streamer/src/pool/cache.rs
git commit -m "feat: add Notify triggers for event-driven subscription updates"
```

---

## Task 2: 事件驅動訂閱 — full_pipeline 改用 Notify

**Files:**
- Modify: `executor/examples/full_pipeline.rs`

**Step 1: 取得 Notify handles**

在 gRPC subscription 設定前：

```rust
let sub_notify = pool_streamer.subscription_notify();
let vault_notify = pool_streamer.vault_notify();
let tick_notify = pool_streamer.tick_reload_notify();
```

**Step 2: subscription_updater 改用 Notify**

將：
```rust
let subscription_updater = async move {
    let mut cumulative_accounts: Vec<String> = Vec::new();
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        interval.tick().await;
        // ...
    }
};
```

改為：
```rust
let subscription_updater = async move {
    let mut cumulative_accounts: Vec<String> = Vec::new();
    loop {
        sub_notify.notified().await;

        // Drain newly discovered vault/tick array addresses
        let new_accounts = sub_streamer.drain_pending_subscriptions().await;
        if new_accounts.is_empty() {
            continue;
        }

        // Add to cumulative list (dedup via HashSet for O(1) lookup)
        let before = cumulative_accounts.len();
        for addr in &new_accounts {
            if !cumulative_accounts.contains(addr) {
                cumulative_accounts.push(addr.clone());
            }
        }
        let added = cumulative_accounts.len() - before;
        if added == 0 {
            continue;
        }

        log::info!(
            "Updating gRPC subscription: +{} accounts (total {} explicit)",
            added, cumulative_accounts.len()
        );

        let tx_filter = TransactionFilter {
            account_include: sub_program_ids.clone(),
            account_exclude: vec![],
            account_required: vec![],
        };
        let acct_filter = AccountFilter {
            account: cumulative_accounts.clone(),
            owner: sub_program_ids.clone(),
            filters: vec![],
        };
        if let Err(e) = sub_grpc.update_subscription(
            vec![tx_filter],
            vec![acct_filter],
        ).await {
            log::warn!("Failed to update gRPC subscription: {}", e);
        }
    }
};
```

**Step 3: vault_fetcher 改用 Notify**

將：
```rust
let vault_fetcher = async move {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        interval.tick().await;
        vault_streamer.flush_pending_vaults().await;
    }
};
```

改為：
```rust
let vault_fetcher = async move {
    loop {
        vault_notify.notified().await;
        vault_streamer.flush_pending_vaults().await;
    }
};
```

**Step 4: tick_reloader 拆成獨立 task**

目前 `flush_tick_reloads()` 被放在 `subscription_updater` 裡。改為獨立 task：

```rust
let tick_streamer = pool_streamer.clone();
let tick_reloader = async move {
    loop {
        tick_notify.notified().await;
        tick_streamer.flush_tick_reloads().await;
    }
};
```

在 `tokio::select!` 裡加入：
```rust
_ = tick_reloader => {
    eprintln!("Tick reloader exited");
}
```

**Step 5: Build and run**

Run: `cargo build --release --example full_pipeline`
Expected: 編譯成功

**Step 6: Commit**

```bash
git add executor/examples/full_pipeline.rs
git commit -m "feat: event-driven subscription/vault/tick updates via Notify"
```

---

## Task 3: simulateTransaction 預驗證

**Files:**
- Modify: `executor/src/executor.rs`

**Step 1: 在 build 後、send 前加入 simulate**

在 `executor.rs` 的 `run()` 方法裡，在 `let pair = self.tx_builder.build(...)` 之後，`// Skip if no tx was built` 之後，加入 simulate 邏輯：

```rust
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;

// Simulate before sending — filter out false opportunities
if let Some(ref tx) = pair.swqos_tx.as_ref().or(pair.jito_tx.as_ref()) {
    let sim_config = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::processed()),
        ..Default::default()
    };
    match self.rpc.simulate_transaction_with_config(tx, sim_config).await {
        Ok(sim_result) => {
            if let Some(err) = sim_result.value.err {
                info!(
                    "[SIMULATE] FAIL: {} | engine_profit={:.6} SOL | {} hops slot={} → skipped",
                    err,
                    opp.expected_profit as f64 / 1e9,
                    opp.route.hops.len(),
                    opp.slot,
                );
                continue;
            }
            let cu_used = sim_result.value.units_consumed.unwrap_or(0);
            info!(
                "[SIMULATE] PASS: engine_profit={:.6} SOL, CU={} | {} hops slot={} → sending",
                opp.expected_profit as f64 / 1e9,
                cu_used,
                opp.route.hops.len(),
                opp.slot,
            );
        }
        Err(e) => {
            warn!("[SIMULATE] RPC error: {} — sending anyway", e);
            // RPC failure — don't block, send anyway
        }
    }
}
```

**位置**：放在 `// Skip if no tx was built` 檢查之後、`// Test mode: send and wait` 之前。

**Step 2: Build**

Run: `cargo build -p arb-executor`
Expected: 編譯成功

**Step 3: Commit**

```bash
git add executor/src/executor.rs
git commit -m "feat: simulateTransaction pre-validation before sending"
```

---

## Task 4: CLMM tick arrays 擴展（3 → 7）

**Files:**
- Modify: `solana-streamer/src/pool/decoder/raydium_clmm.rs`

**Step 1: 修改 tick_array_start_indices 回傳 7 個**

將：
```rust
pub fn tick_array_start_indices(tick_current: i32, tick_spacing: u16) -> [i32; 3] {
    let ticks_in_array = tick_spacing as i32 * 60;
    let current_start = tick_array_start_index(tick_current, tick_spacing);
    [
        current_start - ticks_in_array, // left
        current_start,                   // current
        current_start + ticks_in_array, // right
    ]
}
```

改為：
```rust
pub fn tick_array_start_indices(tick_current: i32, tick_spacing: u16) -> [i32; 7] {
    let ticks_in_array = tick_spacing as i32 * 60;
    let current_start = tick_array_start_index(tick_current, tick_spacing);
    [
        current_start - 3 * ticks_in_array,
        current_start - 2 * ticks_in_array,
        current_start - ticks_in_array,
        current_start,
        current_start + ticks_in_array,
        current_start + 2 * ticks_in_array,
        current_start + 3 * ticks_in_array,
    ]
}
```

**Step 2: 更新所有呼叫點**

`streamer.rs` 的 `fetch_clmm_extras` 和 `flush_tick_reloads` 已經用 `for start_index in start_indices` 迭代，不需要改。

`scanner.rs` 的 `build_pool_accounts` 裡 `others.iter().take(2)` — 現在有更多 tick arrays 可用，但鏈上 CPI 一次最多傳 3 個 tick array account。保持 `take(2)`（共 3 個：1 containing + 2 nearest）不改。

**Step 3: Build and test**

Run: `cargo build -p solana-streamer-sdk && cargo test -p solana-streamer-sdk`
Expected: 編譯成功，測試通過

**Step 4: Commit**

```bash
git add solana-streamer/src/pool/decoder/raydium_clmm.rs
git commit -m "feat: expand CLMM tick arrays from 3 to 7 for better price accuracy"
```

---

## Task 5: 動態 Slot 新鮮度檢查

**Files:**
- Modify: `engine/src/scanner.rs`
- Modify: `engine/src/config.rs`

**Step 1: 在 EngineConfig 加 staleness 開關**

在 `engine/src/config.rs` 的 `EngineConfig` 加：
```rust
/// Enable dynamic staleness check (skip routes with stale pools)
pub enable_staleness_check: bool,
```

在 `Default` 裡加：
```rust
enable_staleness_check: true,
```

在 `executor/src/config.rs` 的 `EngineConfigFile` 加：
```rust
pub enable_staleness_check: Option<bool>,
```

在 `engine_config()` 方法裡加：
```rust
enable_staleness_check: ec.enable_staleness_check.unwrap_or(defaults.enable_staleness_check),
```

**Step 2: 在 scanner.rs 加 staleness 檢查函數**

在 `scanner.rs` 加：

```rust
use crate::config::WSOL_MINT;

/// Calculate max allowed stale slots based on pool reserves (SOL equivalent).
/// Large pools tolerate more staleness; small pools need fresh data.
fn max_stale_slots(pool: &PoolEntry) -> u64 {
    let sol_reserve = match &pool.math {
        PoolMath::ConstantProduct { reserve_a, reserve_b, .. } => {
            // Pick SOL side; if neither is SOL, use the larger reserve as proxy
            if pool.mint_a == WSOL_MINT {
                *reserve_a
            } else if pool.mint_b == WSOL_MINT {
                *reserve_b
            } else {
                (*reserve_a).max(*reserve_b)
            }
        }
        PoolMath::BondingCurve { virtual_sol_reserves, .. } => *virtual_sol_reserves,
        PoolMath::Concentrated { liquidity, .. } => {
            // Use liquidity as proxy (not directly SOL, but correlated)
            *liquidity / 1_000_000 // rough scale-down
        }
    };

    let sol = sol_reserve as f64 / 1e9;
    if sol > 1000.0 {
        5 // > 1000 SOL: tolerate 2 seconds
    } else if sol > 100.0 {
        3 // 100-1000 SOL: tolerate 1.2 seconds
    } else {
        1 // < 100 SOL: tolerate 400ms only
    }
}

/// Check if any pool in the route is too stale relative to current_slot.
/// Returns Some((pool_index, staleness, max_allowed)) for the first stale hop, or None.
fn find_stale_hop(route: &Route, graph: &TokenGraph, current_slot: u64) -> Option<(u32, u64, u64)> {
    for hop in &route.hops {
        let pool = &graph.pools[hop.pool_index as usize];
        if pool.last_updated_slot == 0 {
            continue; // Never updated — skip check (warmup phase)
        }
        let staleness = current_slot.saturating_sub(pool.last_updated_slot);
        let max = max_stale_slots(pool);
        if staleness > max {
            return Some((hop.pool_index, staleness, max));
        }
    }
    None
}
```

**Step 3: 在 scan_routes_for_pool 的 Phase 1 probe 前加入 staleness 檢查**

在 `scan_routes_for_pool` 方法裡，Phase 1 parallel probe 的 `filter_map` 裡加入 staleness 檢查。需要把 `enable_staleness_check` 和 `slot` 傳進去：

```rust
let enable_staleness = self.config.enable_staleness_check;

let mut probed: Vec<(u32, Route, i64)> = routes.into_par_iter()
    .filter_map(|(idx, route)| {
        // Staleness check
        if enable_staleness {
            if let Some((pool_idx, staleness, max)) = find_stale_hop(&route, graph, slot) {
                let pool = &graph.pools[pool_idx as usize];
                log::debug!(
                    "[STALE] Skip route: hop pool {:.6}.. stale {} slots (max={})",
                    pool.address.to_string(), staleness, max
                );
                return None;
            }
        }

        // Quick reserve check
        for hop in &route.hops {
            if !graph.pool_has_min_reserve(hop.pool_index, min_reserve) {
                return None;
            }
        }
        let probe_profit = optimizer::simulate_route_profit(&route, graph, probe_amount);
        if probe_profit > 0 {
            Some((idx, route, probe_profit))
        } else {
            None
        }
    })
    .collect();
```

**Step 4: Build and test**

Run: `cargo build -p arb-engine && cargo test -p arb-engine`
Expected: 編譯成功，測試通過

**Step 5: Commit**

```bash
git add engine/src/scanner.rs engine/src/config.rs executor/src/config.rs
git commit -m "feat: dynamic slot staleness check based on pool reserves"
```

---

## Task 6: 整合測試與驗證

**Files:**
- All modified files

**Step 1: Full build**

Run: `cargo build --release --example full_pipeline`
Expected: 編譯成功

**Step 2: Run tests**

Run: `cargo test --workspace`
Expected: 所有測試通過

**Step 3: 更新 CLAUDE.md**

在 CLAUDE.md 的「已踩過的坑」section 加入：
- simulateTransaction 預驗證（SIMULATE PASS/FAIL log）
- 事件驅動訂閱（零延遲 Notify）
- 動態 staleness check

**Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: update CLAUDE.md with cache accuracy improvements"
```
