# Onchain Arb — Solana Atomic Arbitrage Bot

## 語言
一律使用繁體中文溝通。技術名詞和程式碼識別符保持原文。

## 專案架構

```
solana-streamer/   — Stage 1: 資料層（獨立 git repo: solana-streamer-sdk）
engine/            — Stage 2: 路由引擎
executor/          — Stage 3: 交易組裝與發送
program/           — Solana 鏈上程式（pinocchio 原生，非 Anchor）
dex-pinocchio-cpi/ — DEX CPI 封裝（自開發，需 git pull 才有）
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
- **`replace_bin_arrays()` 必須同步更新 `extra_accounts`**——否則 tx_builder 用的 bin array PDAs 是舊的

### Orca Whirlpool
- Concentrated liquidity（與 Raydium CLMM 共用 `PoolMath::Concentrated`）
- Tick array 有 88 個 tick（vs Raydium 60），tick size = 113 bytes
- Tick array PDA 用 `start_index.to_string().as_bytes()`（vs Raydium 用 big-endian i32）
- `SwapV2` 的 `sqrt_price_limit`：a_to_b 用 `MIN_SQRT_PRICE_X64 = 4295048016`，b_to_a 用 `MAX_SQRT_PRICE_X64`
- Oracle PDA: `["oracle", whirlpool]`

### PumpSwap Buy/Sell 方向與帳戶佈局
- PumpSwap `mint_a = base`（token），`mint_b = quote`（SOL）
- **方向語意**：engine 的 `is_a_to_b=true`（base→quote）= **sell**，`is_a_to_b=false`（quote→base）= **buy**
- swap.rs 的 `if !is_a_to_b` 分支是 buy，`else` 分支是 sell（**不要搞反**）
- `input_ata_index`：sell(a_to_b) 讀 base[5]，buy(!a_to_b) 讀 quote[6]
- Buy = 23 帳戶，Sell = 21 帳戶（Sell 沒有 `global_volume_accumulator` 和 `user_volume_accumulator`）
- **off-chain 統一傳 23 帳戶（Buy 佈局）**，on-chain Sell 跳過 [19][20]，用 [21]=`fee_config`、[22]=`fee_program`
- 如果 Sell 用 [19] 作 `fee_config` 會導致 0xbbf (AccountOwnedByWrongProgram)——因為 [19] 是 volume accumulator PDA
- `fee_config` PDA seeds: `["fee_config", PUMPSWAP_PROGRAM]` → `PUMPSWAP_FEE_PROGRAM`
- `base_pool_account_count` = 23（固定，不分 buy/sell）

### Raydium CPMM Authority
- 全域常數 `GpMZbSM2GgvTKHJirzeGfMFoaZ8UR2X7F4v8vHTvxFbL`，**不是** per-pool PDA
- 用 `find_program_address` 派生會得到錯誤地址，導致 0x7d6 (ConstraintSeeds)

### CLMM Tick Array
- 輸入金額超過已載入 tick array 範圍會報 `NotEnoughTickArrayAccount (6027)`
- `clmm_cap_input()` 用 `limit_in_a` / `limit_in_b` 限制第一跳 CLMM 輸入量
- 載入 7 個 tick arrays（current ± 3），提升大額交易報價準確度
- **`clmm_get_amount_out()` 不可在 tick 用盡後繼續報價**——假設無限流動性會產生虛假巨額利潤（10-84 SOL），on-chain 實際上輸出遠低於報價，導致 Custom(1) loss error
- `InvalidFirstTickArrayAccount (6028)`：tick_current 已移動但 tick array PDA 還是舊的

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

### 鏈上程式部署流程
1. **確認 `dex-pinocchio-cpi/` 存在**——不在 sol-arb repo 裡，需另外 `git pull`
2. **Build SBF**：`cd program && cargo build-sbf`（產出 `target/deploy/arb_program.so`）
3. **Solana tools 版本**：pinocchio 0.10+ 需要 rustc 1.84+，用 `agave-install update` 更新
4. **Deploy**：
   ```bash
   solana program deploy target/deploy/arb_program.so \
     --program-id 8NwGVcMu96JTJwfUKQNXjYMd87JyGWcPkaENe9NPzLCV \
     --url https://api.mainnet-beta.solana.com \
     --keypair ~/.config/solana/id.json \
     --with-compute-unit-price 100000
   ```
   - **不能用 Helius RPC deploy**——不支援 preflight check，會報 `Invalid Request`
   - 用公共 mainnet RPC（`https://api.mainnet-beta.solana.com`）
   - Upgrade authority: `A6m1zY2dM2ue4Aem6q2WSZyS3CX4ap39ApyJSCXhB5Fq`（同 payer）
   - Deploy 約需 0.42 SOL（buffer rent），成功後會退回
5. **Deploy 失敗處理**：
   - 餘額不足：先充值到 payer 地址
   - Buffer 殘留：`solana program close <BUFFER_ADDRESS>` 回收 lamports
   - 如果 buffer 找不到（已被清理），lamports 已退回，直接重試即可

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
7. Meteora DAMM V2 — DammV2Concentrated（Uniswap V3 風格 single-range CL，sqrt-price 數學）
8. Meteora DLMM — ConstantProduct（bin-based，用 vault balance 近似報價）
9. Orca Whirlpool — Concentrated（tick-based CLMM，與 Raydium CLMM 共用數學模型）

## Backlog 重點
- Raydium CLMM tick_arrays 完整支援
- 啟動時 getProgramAccounts 批量拉池子
- 詳見 memory/backlog.md
