# Cache 準確度改善與假機會過濾設計

日期：2026-03-13

## 背景

Engine 持續產出利潤率不合理的假機會（如 43.7% profit ratio），導致 PROD MODE 下所有交易被鏈上 revert，跑一整晚零成交。根因是 cache 的池子狀態跟鏈上實際有落差，engine 用過時的 reserves 算出不存在的利潤。

本設計包含四項改善，從資料源到發送端全面提升準確度。

## 改善 1：事件驅動訂閱（零延遲 Notify）

### 問題

`subscription_updater` 每 2 秒 poll 一次 `drain_pending_subscriptions()`，新發現的 vault 最多等 2 秒才加入 gRPC 訂閱。2 秒 = 5 個 slot，這段時間 reserves 完全是盲區。

### 方案

用 `tokio::sync::Notify` 取代 interval polling，實現零延遲訂閱更新。

**架構：**

```
handle_discovery() / handle_account_update()
    ↓ push to pending_subscriptions
    ↓ self.subscription_notify.notify_one()

subscription_updater loop:
    subscription_notify.notified().await   ← 零延遲喚醒
    drain_pending_subscriptions()
    update_subscription()
```

**三個獨立 Notify：**

| Notify | 觸發時機 | 消費者 |
|--------|---------|--------|
| `subscription_notify` | push 到 pending_subscriptions 時 | subscription_updater：drain + update_subscription |
| `vault_notify` | push 到 pending_vaults 時 | vault_fetcher：flush_pending_vaults |
| `tick_reload_notify` | push 到 tick_reload_queue 時 | tick_reloader：flush_tick_reloads |

分開的原因：tick reload 涉及多個 RPC call，不能阻塞訂閱更新。

**修改檔案：**
- `solana-streamer/src/pool/streamer.rs` — 加 Notify 欄位，push 時觸發
- `solana-streamer/src/pool/cache.rs` — tick_reload_queue push 時觸發 notify
- `executor/examples/full_pipeline.rs` — 三個 loop 改成 notified().await

**Debounce 分析：**

不需要 debounce。`Notify::notify_one()` 天然合併：如果多個 push 在消費者處理前發生，只會喚醒一次。同一個 pool 的 vault_a + vault_b 在 `handle_account_update` 裡同步加入，消費者一次 drain 全部拿走。

## 改善 2：simulateTransaction 預驗證

### 問題

Engine 算出的假機會直接送上鏈。Astralane SwQoS 失敗交易仍扣手續費（不像 Jito bundle）。

### 方案

在 Executor 組裝完交易、發送前，用 `simulateTransaction` 模擬。只看成功/失敗，不解析利潤（利潤交給鏈上 PROD MODE 驗證）。

**流程：**

```
Engine Opportunity → TX Builder 組裝 → simulateTransaction
    ↓ 失敗 → log 錯誤原因，跳過
    ↓ 成功 → 發送交易
```

**模擬設定：**
- `sig_verify: false` — 不驗簽名，加快速度
- `replace_recent_blockhash: true` — 避免 blockhash 過期導致模擬失敗

**Log 格式：**

```
[SIMULATE] PASS: engine_profit=0.05 SOL | 2 hops RaydiumClmm→PumpSwap → sending
[SIMULATE] FAIL: InstructionError(2, Custom(6027)) | engine_profit=7.18 SOL | 3 hops CLMM→CLMM→Meteora → skipped
```

**修改檔案：**
- `executor/src/executor.rs` — 在發送前插入 simulate 步驟

## 改善 3：CLMM 擴展 tick arrays（3 → 7）

### 問題

只載入 3 個 tick arrays（left/current/right），大額交易跨越更多 tick 時 `limit_in_a`/`limit_in_b` 不準，產生假機會或 NotEnoughTickArrayAccount 錯誤。

### 方案

`tick_array_start_indices()` 從回傳 3 個擴展為 7 個（current ± 3）。

**影響：**
- 每個 CLMM 池子初始載入多 4 個 RPC call + 4 個 gRPC 訂閱帳戶
- `compute_clmm_limits` 不用改，已遍歷所有 tick_arrays
- tick reload 時也載入 7 個

**修改檔案：**
- `solana-streamer/src/pool/decoder/raydium_clmm.rs` — `tick_array_start_indices()` 擴展

## 改善 4：Slot 新鮮度檢查（動態 staleness）

### 問題

高頻池子每 slot 多筆交易，cache 落後幾個 slot 就過時。Engine 不管資料多舊都照算。

### 方案

在 Scanner 掃描路徑時，對每個 hop 的 pool 檢查 `last_updated_slot` vs 當前 slot。依據池子 reserves 大小動態決定容忍度：

| Reserves 區間 | max_stale_slots | 理由 |
|--------------|-----------------|------|
| > 1000 SOL | 5（2 秒） | 大池子每 slot 價格變化小 |
| 100–1000 SOL | 3（1.2 秒） | 中等敏感度 |
| < 100 SOL | 1（400ms） | 小池子價格波動大，幾筆交易就差很多 |

**Reserves 計算：**
- CP 池：`reserve_a` 或 `reserve_b`（取 SOL 那邊，用 mint 判斷）
- CLMM 池：用 `liquidity` 作為代理指標（或統一用固定 max_stale_slots=3）

**Stale 路徑處理：**
- 直接跳過，不發送
- Log：`[STALE] Skip: hop 1 pool 3nMF.. stale 8 slots (reserves=50 SOL, max=1)`

**修改檔案：**
- `engine/src/scanner.rs` — 掃描時加入 staleness 檢查
- `engine/src/config.rs` — 可選：加 `enable_staleness_check: bool` 開關（預設 true）
