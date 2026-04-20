# Onchain Arb — Solana Atomic Arbitrage Bot

Solana 鏈上原子套利機器人，透過 gRPC 即時監聽多個 DEX 池子狀態，以圖論搜尋套利路徑，並以 flashloan 原子交易執行。虧損自動 revert，零本金風險。

## 專案架構

```
solana-streamer/   Stage 1: 資料層 — gRPC 訂閱 + PoolStateCache（獨立 repo: solana-streamer-sdk）
engine/            Stage 2: 路由引擎 — TokenGraph + DFS + ternary search
executor/          Stage 3: 交易組裝與發送 — Astralane / Flashblock / Jito
program/           Stage 4: 鏈上程式 — pinocchio 原生（非 Anchor）
dex-pinocchio-cpi/ DEX CPI 封裝（獨立 repo，需另外 git pull）
```

### Pipeline 流程

1. **Streamer** — gRPC 訂閱 9 個 DEX program，即時解碼池子狀態
2. **Pool Cache** — DashMap 快取，vault reverse index 支援 token account 更新
3. **Engine / Scanner** — warmup + incremental/full scan，DFS 找迴路 + 三分搜最優輸入
4. **TX Builder** — 組裝 MarginFi flashloan + swap ix + 固定 priority fee + ALT
5. **Sender** — `simulateTransaction` 預驗證通過後，透過 SwQoS / Jito bundle 發送

## 支援的 DEX（9 個）

| DEX | 數學模型 | 備註 |
|-----|----------|------|
| Raydium AMM V4 | ConstantProduct | |
| Raydium CPMM | ConstantProduct | authority 為全域常數 |
| Raydium CLMM | Concentrated | tick array 方向排列 |
| Meteora DAMM V2 | DammV2Concentrated | Uniswap V3 風格、sqrt-price |
| Meteora DLMM | ConstantProduct 近似 | bin-based，active_id 變動時重算 bin array PDA |
| Orca Whirlpool | Concentrated | 共用 CLMM 數學；b_to_a 需 shift |
| PumpSwap | ConstantProduct | buy=23 帳戶，sell=21 帳戶 |
| PumpFun | BondingCurve | 幾乎無套利價值 |
| BonkSwap | ConstantProduct | |

所有主要 DEX cross-DEX CPI 已在 mainnet 驗證（buy + sell 雙方向）。

## 關鍵設計

### 事件驅動、零延遲
- `tokio::sync::Notify` 三條獨立通道（subscription / vault / tick reload），取代輪詢
- 新 vault 發現後立即加入 gRPC 訂閱

### 動態 Slot 新鮮度
- reserves > 1000 SOL 容忍 5 slots、100–1000 容忍 3、< 100 容忍 1
- 路徑中任一池子過舊則跳過

### simulateTransaction 預驗證
- `sig_verify=false` + `replace_recent_blockhash=true` 加速模擬
- 失敗不上鏈、節省 SwQoS 失敗交易的 priority fee
- RPC 錯誤時 graceful degradation，照常送出

### 固定 Priority Fee
SwQoS 失敗交易仍扣費，不可隨 expected_profit 線性增長。固定 0.0001 SOL，依 CU limit 換算 micro-lamports per CU。

### TEST / PROD Mode
- Discriminator 0/1/2 = PROD：鏈上原子驗證利潤，虧損自動 revert
- Discriminator 3/4/5 = TEST：跳過 on-chain profit check

### getProgramAccounts 覆蓋面優化
gRPC 只推送有交易的池子；啟動時用 gPA 批量拉靜默池：DLMM 全量 + Orca 全量 + CLMM/CPMM SOL-B，合計 ~138K，約 20 秒。

## 部署

### 鏈上程式

```bash
cd program && cargo build-sbf

solana program deploy target/deploy/arb_program.so \
  --program-id 8NwGVcMu96JTJwfUKQNXjYMd87JyGWcPkaENe9NPzLCV \
  --url https://api.mainnet-beta.solana.com \
  --keypair ~/.config/solana/id.json \
  --with-compute-unit-price 100000
```

不能用 Helius RPC deploy（不支援 preflight）。需約 0.42 SOL buffer rent，成功後退回。pinocchio 0.10+ 要求 rustc 1.84+，必要時 `agave-install update`。

### Executor

設定檔位於 `executor/config.toml`（含 API key，已 gitignore）。設定值以 SOL 為單位（f64），內部轉 lamports（u64）。

## 測試工具

- `executor/src/bin/test_cross_dex.rs` — Cross-DEX CPI 驗證（全部 DEX）
- `executor/src/bin/test_dex_cpi.rs` — 單一 DEX CPI 驗證
- `executor/src/bin/test_gpa.rs` — getProgramAccounts 批量拉池測試
- 各 decoder 有 `decode_bytes` 單元測試
