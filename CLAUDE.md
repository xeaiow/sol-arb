# Onchain Arb — Solana Atomic Arbitrage Bot

## 語言
一律使用繁體中文溝通。技術名詞和程式碼識別符保持原文。

## 專案架構

```
solana-streamer/   — Stage 1: 資料層（獨立 git repo: solana-streamer-sdk）
engine/            — Stage 2: 路由引擎
executor/          — Stage 3: 交易組裝與發送
program/           — Solana 鏈上程式（Anchor）
```

### Pipeline 完整流程
1. **Streamer** — gRPC 訂閱鏈上事件，即時監聽 DEX 池子狀態
2. **Pool Cache** — DashMap 快取池子狀態，vault reverse index
3. **Engine/Scanner** — TokenGraph + DFS 找套利路徑 + ternary search 優化輸入
4. **TX Builder** — 組裝交易（flashloan + swap 指令 + priority fee）
5. **Executor/Sender** — 透過 Astralane/Flashblock/Jito 發送交易

### 關鍵檔案
- `solana-streamer/src/pool/state.rs` — DexType, PoolMath, PoolState
- `solana-streamer/src/pool/cache.rs` — PoolStateCache（update_math 需保留 CP reserves 和 fees）
- `solana-streamer/src/pool/decoder/` — 7 DEX decoders
- `solana-streamer/src/pool/streamer.rs` — PoolStreamer integration
- `engine/src/optimizer.rs` — simulate_route_profit, find_optimal_amount, clmm_cap_input
- `engine/src/scanner.rs` — Scanner（warmup + incremental/full scan）
- `executor/src/tx_builder.rs` — 交易組裝（priority fee, swap 指令, ALT）
- `executor/src/config.rs` — ExecutorConfigFile 設定結構
- `executor/config.toml` — 執行設定（**含 API key，已 gitignore**）

## 已踩過的坑（Production Lessons）

### Priority Fee
- SwQoS 失敗交易仍上鏈並扣費（不像 Jito bundle）
- **必須用固定 priority fee**，不可隨 expected_profit 線性增長
- 設定：`priority_fee_lamports = 100000`（0.0001 SOL），在 config.toml 的 flashblock/astralane section
- `calculate_cu_price()` 基於固定 lamports 和 CU limit 計算 micro-lamports per CU

### Anchor Optional Accounts
- IDL `optional=true` 的帳戶，用 program_id 作 placeholder 表示缺席（不是派生 PDA）
- 如果派生不存在的 PDA 會導致 0xbc4 (AccountNotInitialized)
- Meteora DAMM V2 的 `referral_token_account` 和 DLMM 的 `bitmap_ext`、`host_fee_in` 都是 optional

### pinocchio CPI 規則
- `invoke_signed_with_slice` 的 `account_views` 和 `instruction.accounts` 必須 **1:1 位置對應**
- 每個位置的 `address` 必須匹配，否則報 `InvalidArgument`
- `views.len() < instruction.accounts.len()` → `NotEnoughAccountKeys`（即 "insufficient account keys"）
- **絕對不可以在 views 中跳過帳戶**，即使是 placeholder

### Meteora DAMM V2
- `pool_authority` 是固定全域地址 `HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC`（來自 IDL），不是 per-pool PDA
- `referral_token_account` 是 optional，用 DAMM V2 program_id 作 placeholder（readonly）

### Meteora DLMM
- Bin-based AMM，離線用 ConstantProduct + vault balance 近似報價
- `Swap2` 指令：16 固定帳戶 + remaining bin_arrays
- `bitmap_ext[1]`、`host_fee_in[9]`、`program[15]` 是 optional accounts，用 DLMM program_id 作 placeholder（表示缺席）
- **pinocchio CPI 要求 views 和 instruction_accounts 必須 1:1 位置對應**，不可 skip 任何帳戶
- `remaining_accounts_info`：Borsh-serialized empty Vec（前 4 bytes = 0）
- `bin_array_bitmap_extension` PDA: `["bitmap", lb_pair]`
- Fee: `base_factor * bin_step * 10` (units of 1e-9)
- Bin array PDA seeds: `["bin_array", lb_pair, index.to_le_bytes()]`（index 是 i64）
- 每個 bin array 放 70 個 bin，`bin_array_index = floor(active_id / 70)`
- Decoder 在解碼時就計算 3 個 bin array PDAs（current ± 1）塞進 `extra_accounts[1..4]`
- `active_id` 變動時 LbPair account update 會重新算 bin array PDAs

### Orca Whirlpool
- Concentrated liquidity（與 Raydium CLMM 共用 `PoolMath::Concentrated`）
- Tick array 有 88 個 tick（vs Raydium 60），tick size = 113 bytes
- Tick array PDA 用 `start_index.to_string().as_bytes()`（vs Raydium 用 big-endian i32）
- `SwapV2` 的 `sqrt_price_limit`：a_to_b 用 `MIN_SQRT_PRICE_X64 = 4295048016`，b_to_a 用 `MAX_SQRT_PRICE_X64`
- Oracle PDA: `["oracle", whirlpool]`

### CLMM Tick Array
- 輸入金額超過已載入 tick array 範圍會報 `NotEnoughTickArrayAccount (6027)`
- `clmm_cap_input()` 用 `limit_in_a` / `limit_in_b` 限制第一跳 CLMM 輸入量
- 載入 7 個 tick arrays（current ± 3），提升大額交易報價準確度

### CP 池 Reserve/Fee 保留
- `update_math()` CP 分支必須保留現有 reserves（decoder 回傳 0 時）和 fees
- Decoder 可能回傳 hardcoded 預設 fees，不可覆蓋 streamer 設定的正確值

### Vault Batch Fetch
- `getMultipleAccounts` RPC 限制：部分節點上限 50（非預設 100）
- `streamer.rs` 用 `chunks(50)`，失敗的 vault 會 re-queue 重試

### MarginFi Flashloan
- `destination_token_account`（payer 的 base mint ATA）必須在 borrow 指令前就已初始化
- `build_create_ata_ixs` 需包含 base mint 的 ATA（不只是中間 token 的 ATA）

### Token 帳戶
- SPL Token 和 Token-2022 balance 都在 byte offset 64（u64 LE）
- Token-2022 ATA 需用正確的 token program 派生

### 事件驅動訂閱
- gRPC 訂閱更新改為 `tokio::sync::Notify` 零延遲事件驅動（不再 2 秒 polling）
- 三個獨立 Notify：subscription_notify、vault_notify、tick_reload_notify
- 新 vault 發現後立刻加入 gRPC 訂閱，不用等 interval

### simulateTransaction 預驗證
- 發送前用 `simulateTransaction` 模擬，失敗的交易直接跳過不送
- `sig_verify: false` + `replace_recent_blockhash: true` 加快模擬速度
- Log 格式：`[SIMULATE] PASS/FAIL` 含 engine_profit、CU、hops、slot
- RPC 錯誤時不阻塞，照常發送（graceful degradation）

### 動態 Slot 新鮮度
- 路徑中任一池子 `last_updated_slot` 過舊則跳過
- 門檻依 reserves 動態調整：>1000 SOL 容忍 5 slots、100-1000 容忍 3、<100 容忍 1
- `enable_staleness_check` 可在 config 關閉（預設開啟）

### TEST MODE vs PROD MODE
- Discriminator 3/4/5 = TEST MODE：跳過鏈上利潤驗證，虧損交易不會 revert
- Discriminator 0/1/2 = PROD MODE：原子性驗證利潤，虧損交易自動 revert
- TEST MODE 成功但虧損是正常的——因為沒有 on-chain profit check

## 交易分析 SOP

當用戶貼 Solana transaction signature 時，自動執行以下分析：

1. **取得交易資料** — 用 Helius RPC 呼叫 `getTransaction`（maxSupportedTransactionVersion: 0, encoding: jsonParsed）
2. **判斷成功/失敗**
   - 失敗：解析 `err` 欄位，對照已知錯誤碼（如 6027=NotEnoughTickArrayAccount, 2012=ConstraintAddress）找出根因
   - 成功：繼續盈虧分析
3. **盈虧分析**（成功交易）
   - 從 `preBalances`/`postBalances` 算出 payer 的 SOL 變化
   - 從 `preTokenBalances`/`postTokenBalances` 算出 token 變化
   - 計算手續費（`fee` 欄位）
   - 淨利 = SOL 變化 + fee（因為 fee 已從 balance 扣除）
   - 判斷 TEST MODE / PROD MODE（看 instruction discriminator）
4. **輸出格式**
   - 模式（TEST/PROD）、路徑（幾跳、哪些 DEX）
   - 投入金額、產出金額
   - 手續費、priority fee
   - 淨損益（含/不含手續費）
   - 若失敗：錯誤碼、根因、建議修正方向

RPC endpoint: 用 config.toml 中的 `rpc_url`，或 fallback 到 `https://mainnet.helius-rpc.com/?api-key=89ed37ec-971c-48e0-99db-921d578354e6`

## 開發規範

### 修改原則
- **說什麼做什麼，不要自己腦補**——不過度解讀指令
- **沒用到的程式碼就刪掉**——不要留死碼或向後相容 shim
- **有需要改就改**——分析效能後選最佳方案，不分鏈上鏈下
- **部署鏈上程式前必須先確認**——每次 deploy 都花錢

### 設定管理
- `executor/config.toml` 含 API key，已加入 `.gitignore`
- config struct 的欄位與 TOML 必須同步——新增/移除欄位要兩邊一起改
- 設定值用 SOL 為單位（f64），程式內部用 lamports（u64），轉換在 `sol_to_lamports()`

### 測試
- 各 decoder 有 `decode_bytes` 單元測試
- Engine 有 optimizer 單元測試
- 修改後應確認 `cargo build` 和 `cargo test` 通過

## 目前支援的 DEX（9 個）
1. Raydium AMM V4 — ConstantProduct
2. Raydium CPMM — ConstantProduct
3. Raydium CLMM — Concentrated（有 tick boundary 限制）
4. PumpFun — BondingCurve
5. PumpSwap — ConstantProduct
6. BonkSwap — ConstantProduct
7. Meteora DAMM V2 — ConstantProduct
8. Meteora DLMM — ConstantProduct（bin-based，用 vault balance 近似報價）
9. Orca Whirlpool — Concentrated（tick-based CLMM，與 Raydium CLMM 共用數學模型）

## Backlog 重點
- Raydium CLMM tick_arrays 完整支援
- 啟動時 getProgramAccounts 批量拉池子
- 詳見 memory/backlog.md
