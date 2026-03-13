# DEX 報價修正進度

## 已完成

### 1. Meteora DAMM V2 ✅ (commit 740dfb6)
- **問題**: 用了錯誤的 `ConstantProduct` 模型，vault balance 包含 range-outside 資產，隱含價格偏差 72-263%
- **修正**: 新增 `PoolMath::DammV2Concentrated` variant，使用 Uniswap V3 風格 sqrt-price CL 數學
- **驗證**: `verify_damm_v2.rs` 確認 f64 報價與鏈上結果一致
- **公式**: A→B: `next_sp = L*sp/(L + amt*sp)`, B→A: `next_sp = sp + amt/L`
- **影響檔案**: state.rs, decoder, events.rs, parser.rs, cache.rs, optimizer.rs, graph.rs, scanner.rs

### 2. Meteora DLMM ✅ (commit f822ce5)
- **問題**: `decode_bytes()` 所有 offset 偏移 +8 bytes（active_id 讀 84 應為 76，bin_step 讀 88 應為 80）
- **根因**: StaticParameters(32 bytes) + VariableParameters(32 bytes) 之後的欄位 offset 計算錯誤
- **修正**: 修正 active_id@76, bin_step@80, mints@88/120, vaults@152/184, oracle@552
- **附加修正**: `pool_fee_ratio()` 原本對 DLMM 回傳 0.0，現在正確計算 base_fee + var_fee
- **驗證**: `verify_dlmm.rs` 確認修正後 bin_step/active_id 讀取正確
- **注意**: gRPC parser 用 sequential offset 讀取，一直是正確的。只有 `decode_bytes`（RPC 批量載入）有問題

## 進行中

### 3. Orca Whirlpool ⚠️ 發現潛在 bug，尚未修正
- **疑似問題**: tick array 中 tick index 計算可能錯誤
  - `decode_tick_array()` 用 `start_tick_index + i` 計算 tick index
  - 但 Orca Whirlpool 每個 tick 間隔 `tick_spacing`，應該是 `start_tick_index + i * tick_spacing`
  - 如果 `tick_spacing=64`，第 5 個 tick 的 index 應為 `start + 320`，不是 `start + 5`
  - 需要確認 Raydium CLMM 是否也有同樣問題（兩者共用 `PoolMath::Concentrated`）
- **待做**:
  1. 確認 Raydium CLMM 的 tick array 是否用不同 layout（Raydium 每 array 60 ticks，每 tick 也是按 tick_spacing 間隔嗎？）
  2. 寫 `verify_whirlpool.rs` 對比鏈上報價
  3. 如果 tick index 計算確實錯誤，修正 `decode_tick_array` 和 `tick_array_from_event`

## 未開始

### 4. Raydium CLMM — 待驗證
- 共用 `PoolMath::Concentrated` 和 `clmm_get_amount_out()` tick traversal
- 需確認 tick index 計算是否正確（同 Orca 問題）
- 載入 7 個 tick arrays（±3），大額交易可能超出範圍
- 有 0.3% haircut

### 5. Raydium AMM V4 — 可能正確
- 標準 ConstantProduct，vault balance = reserves
- 需快速驗證 fee 是否正確讀取

### 6. Raydium CPMM — 可能正確
- 標準 ConstantProduct，fee 從 AmmConfig 讀取
- 需確認 fee 讀取流程

### 7. PumpSwap — 已修過，應正確
- 標準 ConstantProduct，fee 從 GlobalConfig 讀取
- 之前已修過 buy/sell 帳戶佈局問題

### 8. PumpFun — 可能正確
- BondingCurve with virtual reserves + 1% fee + real_cap

### 9. BonkSwap — 可能正確
- 標準 ConstantProduct，reserves 直接從 pool account 讀取

## 其他待修項目

- **config.toml `max_profit_ratio = 50`**（5000%）太寬鬆，應改回接近 `0.1`（10%），這允許大量不合理的「假機會」通過
- **DLMM 0.5% haircut** 可能過大，CLMM 用 0.3%，需評估是否下調
- **DLMM `max_stale_slots` 回傳 0** — scanner.rs 中 DLMM 的 sol_reserve 估算回傳 0，導致 staleness 門檻過嚴（只容忍 1 slot）
