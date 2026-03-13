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

### 3. Orca Whirlpool ✅
- **問題**: tick array 中 tick index 計算錯誤 — `decode_tick_array()` 和 `tick_array_from_event()` 用 `start_tick_index + i`，應為 `start_tick_index + i * tick_spacing`
- **根因**: Orca 在 tick array 中每個 tick 間隔 `tick_spacing`，但 decoder 把它們當連續的（間隔 1）
- **影響**: tick_spacing=64 時，tick indices 壓縮了 64 倍，導致 sqrt_price 計算偏差巨大
  - 大額交易（500 SOL）報價偏差 **-9.9%**（BUGGY: 380,993B vs FIXED: 422,866B）
  - BUGGY nearest tick price: 868（真實: 1433，偏差 -39%）
  - 小額交易不受影響（不需要 cross tick）
- **修正**:
  - `decode_tick_array(data)` → `decode_tick_array(data, tick_spacing)`，index 改為 `start + i * tick_spacing`
  - `tick_array_from_event(event)` → `tick_array_from_event(event, tick_spacing)`，同上
  - tick array minimum size: 10000 → 9988（8 disc + 4 start + 88×113 ticks + 32 whirlpool = 9988）
  - 更新 streamer.rs 3 個呼叫點傳入 tick_spacing
- **Raydium CLMM 不受影響**: 它直接從 on-chain TickState.tick 讀 absolute index，不需要乘 tick_spacing
- **驗證**: `verify_whirlpool.rs`（Phase 1: buggy vs fixed 比對 + Phase 2: production decoder pipeline 驗證）
  - ✓ Production pipeline 報價與 fixed local 報價 100% 一致
  - ✓ All tick indices are multiples of tick_spacing
  - ✓ Nearest ticks correctly bracket real price
- **影響檔案**: decoder/orca_whirlpool.rs, streamer.rs, events.rs (min size constant)

### 4. Raydium CLMM ✅ 驗證完成（無需修改）
- 共用 `PoolMath::Concentrated` 和 `clmm_get_amount_out()` tick traversal
- **不受 Orca bug 影響**: Raydium 的 TickState struct 直接含 absolute tick index（`t.tick`），decoder 只是複製
- 載入 7 個 tick arrays（±3），有 `clmm_cap_input()` 保護大額交易
- 0.3% haircut 合理

### 5. Raydium AMM V4 ✅ 驗證完成（無需修改）
- 標準 ConstantProduct，fee 從 pool account 的 Fees struct 讀取
- Reserves 從 vault balance gRPC 更新，cache.rs update_math() 正確保留
- Fee 是 static（baked into pool account），不需要外部 fetch

### 6. Raydium CPMM ✅ 驗證完成（無需修改）
- 標準 ConstantProduct，fee 從 AmmConfig 外部 fetch（fetch_cpmm_fee）
- Decoder hardcodes 25/10000 預設值，streamer 覆蓋為正確值
- cache.rs update_math() 保留 fetched fee，不被 decoder 預設值覆蓋

### 7. PumpSwap ✅ 驗證完成（無需修改）
- 標準 ConstantProduct，fee 從 GlobalConfig 外部 fetch（apply_pumpswap_fee）
- buy/sell 帳戶佈局已修正
- Fee = lp_fee_basis_points + protocol_fee_basis_points

### 8. PumpFun ✅ 驗證完成（無需修改）
- BondingCurve with virtual reserves + 1% hardcoded fee + real_cap
- Fee 不可設定（protocol 固定 1%）

### 9. BonkSwap ✅ 驗證完成（無需修改）
- 標準 ConstantProduct，virtual reserves 直接從 pool account 讀取
- 1% hardcoded fee

## 其他待修項目

- **config.toml `max_profit_ratio = 50`**（5000%）太寬鬆，應改回接近 `0.5`（50%），過濾明顯假機會但保留合理空間
- **DLMM 0.5% haircut** — CLMM 和 DAMM V2 都用 0.3%，DLMM 用 0.5% 可能偏高。但 DLMM 是 bin-based 近似報價，精度損失可能比 tick-based CLMM 大。建議用實際交易數據評估後再決定
- **DLMM `max_stale_slots` 回傳 0** — scanner.rs 中 DLMM 的 sol_reserve 估算回傳 0，導致 staleness 門檻過嚴（只容忍 1 slot）。需從 vault balance 或 bin liquidity 估算 sol_reserve
