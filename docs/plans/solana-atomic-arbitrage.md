# Solana 原子套利技術筆記

> 來源：Telegram 套利討論群對話分析（108,025 則訊息，2024/08 - 2026/03）

---

## 一、Bot 服務器是否需要 RPC？

### 自建 Jupiter 方案：需要連接 RPC + gRPC（但不自己跑）

群裡有明確結論：

- 「jup需要rpc和grpc」
- 「jup是用來找交易路徑的，至少需要rpc才能跑，如果額外加上grpc可以讓jup讀取速度更快」
- 「Jup要rpc grpc」

Bot 服務器上跑的自建 Jupiter 需要連接到 RPC 和 gRPC 端點，但這些端點由另一台節點服務器（agave-validator + yellowstone geyser）提供。Bot 服務器本身不跑 RPC 節點，它是作為客戶端連接自建節點的 RPC/gRPC 接口。

架構：`節點服務器暴露 RPC（HTTP）+ gRPC → Bot 服務器上的自建 Jupiter 連過去拿數據和發交易`

### 手搓路由方案：完全不用 RPC

群裡頭部玩家確實有完全不用 RPC 的做法：

- 「我方案不用RPC」「我手頭是 9354p 夠用，關閉了各種參數，只用來做ARB」
- 「我是不用rpc，只用Grpc訂閱數據，所以可以關閉」
- 「我幾乎不用rpc，除了啟動的時候用一下」
- 「頭部幾個都是手搓的，說不定硬件成本還低呢，不用這麼多ip，也不用跑jup」

這些人完全繞過 Jupiter，自己寫路由引擎，只靠 gRPC 訂閱帳戶數據，本地計算價格和最優路徑，直接構造交易指令送 Jito Bundle。

### 總結

| 方案 | 是否需要 RPC | 是否需要 gRPC | 難度 |
|---|---|---|---|
| 自建 Jupiter | 需要（連節點的） | 需要 | 中等 |
| 手搓路由 | 不需要（或僅啟動時用） | 需要 | 很高 |

---

## 二、什麼是手搓路由？

### 定義

自己寫程式取代 Jupiter 的路由引擎。Jupiter 做的事是：拿到各 DEX 池子的即時數據 → 計算各路徑的報價 → 找出最優兌換路徑。手搓路由就是自己實現這整套邏輯，不再依賴 Jupiter API。

群裡總結：「jup 負責詢價和構建交易，不用 jup 就得手搓路由，還得自己算價格，尋找最優路徑，這可不是一時半會兒能搞定的」

### 為什麼要手搓？

1. Jupiter 路由引擎閉源，自建 Jupiter 最快也要 5ms 輪詢一次報價，且沒有回調機制只能盲目輪詢
2. 手搓可根據 gRPC 推送的數據變化即時觸發計算，有人做到「5ms 構造完交易」
3. 手搓可完全不用 RPC，只靠 gRPC 訂閱數據，減少一整層網路延遲

### 難度與共識

- 「手搓路由已經不是新手村的東西，這種還有教程那就是手摸手教賺錢」
- 「Jup 精髓在於它的路由引擎，但閉源。現在我自己手搓的路由引擎太垃圾」
- 「手搓路由快得很，就是消耗精力太多了」
- 「合約就 100 來行代碼，路由才是核心科技」

---

## 三、拿數據：RPC（pull）vs gRPC（push）

### RPC 方式（pull）

主動發 HTTP 請求問節點：「這個帳戶現在的數據是什麼？」每次想知道池子狀態就要發一次 `getAccountInfo` 請求。池子越多、頻率越高，請求量爆炸，且每次都有網路往返延遲。

### gRPC 方式（push）

跟節點建一條持久連線，告訴它：「我要訂閱這些池子帳戶，有變動就推給我。」之後只要鏈上有人在這個池子交易，節點主動把最新的 account data 推送過來，不需要發任何請求。

做法：

1. 啟動時用 gRPC 訂閱所有關注的池子帳戶
2. gRPC 持續推送這些帳戶的最新 data（raw bytes）
3. 自己在本地解析這些 bytes，算出池子儲備量和價格
4. 完全不需要呼叫任何 RPC 的 `getAccountInfo`

發交易也不一定需要 RPC——做套利的人把交易送進 Jito Bundle，走 Jito Block Engine 的獨立 API，不經過 RPC 的 `sendTransaction`。

整條鏈路：`gRPC 收數據 → 本地算路徑 → Jito 送交易`，RPC 完全被繞過。

---

## 四、本地算路徑怎麼做到？

### 步驟一：本地維護池子狀態

透過 gRPC 訂閱關注的池子帳戶，每次有人交易 gRPC 推送最新 account data（raw bytes），在本地記憶體維護所有池子的即時狀態。

- 「訂閱帳戶數據，然後當交易推送過來後，我再去本地數據去進行一次模擬 swap 再去計算是否有利潤」
- 進階做法：「本地維護數據，然後每次交易過來後自己手動去更新帳戶數據」

### 步驟二：本地算價格（模擬 swap）

兩種做法：

#### 做法 A：數學計算（快但難）

自己用程式碼實現各 DEX 的數學公式：

| DEX 類型 | 定價方式 | 難度 | 群裡評價 |
|---|---|---|---|
| Raydium AMM V4 | `K = x * y` 恆定乘積 | 簡單 | 「AMM 好算」「價格就是 token 數量除以 SOL 數量」 |
| Raydium CLMM / Orca Whirlpool | 集中流動性，處理 tick 區間，讀取 sqrtPrice | 複雜 | 「CLMM 計算邏輯看得頭大」 |
| Meteora DLMM | bin 計算 | 很難 | 「meteora 真 tm 攔路虎」 |
| PumpFun | bonding curve | 中等 | 公式已知 |

性能數據：「算價 + 算最大利潤，如果是兩個 DEX 都是 AMM，大概只要幾 us（微秒），有 DLMM 或 CLMM 大概要 50us」

技巧：把鏈上 u256 整數運算改成 f64 浮點數加速。

#### 做法 B：本地模擬器（慢但通用）

用 Mollusk 等 Solana 交易模擬器，在本地跑一遍 DEX 的合約邏輯。

- 「都是仿真器，本地數學計算不準」「都是在本地模擬交易」
- 「用 mollusk 模擬，只要你能接受一次 0.3ms 的耗時」
- 好處是不用拆解每個 DEX 的數學公式，壞處是比純數學算慢幾個數量級（0.3ms vs 幾 us）

### 步驟三：尋找最優路徑

#### Bellman-Ford 算法

把各池子匯率取對數作為邊的權重，建有向圖，找負環（negative cycle）代表套利空間。

- 「貝爾曼就是專門算最短圖路徑的，Jupiter 的路由引擎就是貝爾曼算法」
- 「jup 的 Metis 使用的是貝爾曼算法」

但不是唯一選擇：

- 「貝爾曼是最慢的，3hop 5hop 用的算法都不一樣」
- 「貝爾曼算法不是聖杯」
- 「最短路徑算法很多的，又不止一種」
- 「1w 個池子，4hop 就很難算了，5hop 不是一般機器能跑的」

#### 暴力搜尋

多數人做 2-3 hop，只盯少量 DEX 的配對，窮舉路徑也夠用。

### 步驟四：算最優輸入金額

找到有套利空間的路徑後，算投入多少金額利潤最大：

| 方法 | 說明 | 群裡評價 |
|---|---|---|
| 三分法迭代 | 利潤對輸入是單峰函數，三分搜快速逼近 | 「我用的三分法去迭代的」「迭代 10 次，相當於模擬 10 次」 |
| 折半查找（二分） | 二分搜尋最佳輸入 | 「折半查找啊」 |
| 暴力枚舉 | 直接試不同金額取最大 | 「這是暴力算法」 |
| 合約內檢查 | 固定幾個金額丟上去讓合約判斷 | 「合約裡面檢查下就行」 |

### 步驟五：構造交易指令

兩種做法：

- **調用 Jupiter 合約**（較簡單）：「自己手搓路由，調用 jup route 就可以了，一樣的效果，不用自己寫合約」
- **直接調用各 DEX 合約**（更進階）：「前前後後部署了 4、5 次合約，可算跑通了幾個 DEX」

### 完整鏈路

```
gRPC 推送帳戶數據
  → 本地更新池子狀態
    → 本地算各池子報價（數學公式或模擬器）
      → 圖算法找套利路徑
        → 三分法/二分法算最優輸入
          → 構造交易指令
            → 送 Jito Bundle
```

全部在本地記憶體完成，不需要外部網路請求。

---

## 五、sol-parser-sdk 評估

**專案地址**：https://github.com/0xfnzero/sol-parser-sdk

### 它做到的事（數據層）

1. 連接 gRPC，訂閱鏈上交易
2. 解析各 DEX 的交易事件，從 raw bytes 提取結構化數據
3. 支援 DEX：PumpFun、PumpSwap、Raydium AMM V4、Raydium CLMM、Raydium CPMM、Orca Whirlpool、Meteora AMM/DLMM/DAMM、Bonk Launchpad
4. 解析延遲 10-20μs，SIMD 加速、零拷貝、無鎖隊列
5. 事件帶有關鍵數據（reserves、amounts 等）

### 還需要自己寫的部分（策略層）

1. **本地維護池子狀態** — 用事件或帳戶訂閱更新本地 reserves
2. **各 DEX 報價計算** — `get_amount_out(amount_in, reserve_in, reserve_out)` 等
3. **路由搜尋算法** — Bellman-Ford 或暴力搜尋
4. **最優輸入金額計算** — 三分法/二分法
5. **交易構造 + Jito 發送**

### 結論

作為數據層的起點很合適，省掉了 gRPC 連接管理和各 DEX 指令格式解碼的繁瑣工作。但核心競爭力在上面的策略層——報價計算精度和路由搜尋速度。

架構：

```
sol-parser-sdk（已有）
  → 池子狀態維護（自己寫）
    → 報價計算引擎（自己寫）
      → 路由搜尋（自己寫）
        → 交易構造 + Jito 發送（自己寫）
```

群裡建議：先用自建 Jupiter 跑通，理解整個流程，再逐步手搓替換。「先從 jup 開始吧，大佬們講的是進階的手搓路由了」

---

## 六、gRPC 服務商推薦（套利用）

群裡針對原子套利使用的 gRPC 服務分為三個層級：

### 第三方服務商（入門）

| 服務商 | 月費 | 群裡評價 |
|---|---|---|
| Shyft | $199 | 最便宜入門選項，「shyft 兩百刀」，有人用它做到 +0 slot |
| Helius | $499+ | 品質好但貴，「helius 大家認為會好一些」，有 staked connection |
| Triton | 更貴 | 有 QUIC 私有連接，「triton 那個 quic 私有專用連接到 swqos 的貴的要死」 |

### 社區 / 專門節點租用（進階）

群主自建節點在 Jito 同機房，社區成員租用：「群主的 grpc」「和群主搞好關係，他出租的比自己建的快」。另有專門節點商如 P9、Urban、Orbit、Onyx 等。

### 自建節點（頂級）

需要高配服務器（503GB RAM + 4TB NVMe），成本 $1,800+/月，但延遲最低。「頂級 bot 應該都是在自己的質押節點上干的，shreds 優先、swqos 優先」

---

## 七、Jito IP 限速與多 IP 策略

### 問題

Jito Block Engine 對每個 IP 限速 5 TPS（每秒 5 筆交易）。群裡常見報錯：「jito rate limit TPS=1」。

### 解決方案：多 IP 並發

用 HAProxy 等負載均衡器把請求分散到多個 IP：

- 「走 jito 就得 ddos 模式，多 ip 是標配，這種也是咱們玩得起的普惠方法」
- 「多搞點機器，程序可以無限開的」
- 「所以還是得多 ip，ip 越多實力越強」

IP 來源：多台 VPS、向 ISP 買額外 IP、HAProxy 綁定多網卡。

---

## 八、Jito 以外的交易提交服務

群裡討論了大量 Jito 以外的上鏈/MEV 相關服務商。

### 1. 0slot（討論最多）

大量群友使用，很多人靠它做到同區塊狙擊。

- 速度：「用了一下 0slot 好像比 jito 快多了」、「基本人手節點 + 0slot」、「要快必須得手搓，手搓以後終於能干到 0slot 了」
- 小費：舊帳號最低 0.0001 SOL，新帳號 0.001 SOL 起，群裡覺得「0slot 太貴了」
- 防夾：沒有防夾選項，有人 10% 滑點被夾，懷疑「就是 0slot 自己弄的」
- 限制：後來開始提高准入門檻，「0slot 搞個試用那么難」、「每天 1 個 SOL tip 才夠資格」
- 附加工具：開源捆綁檢測 https://github.com/0slot-trade/checktx_v2
- 需要設 ComputeUnitPrice：「0slot 需要，jito bundle 不用」

群裡有人總結：「確實快，占比百分之 90」，但也有人指出「0slot 或者 node nextblock 自己人玩的，給他們掏小費還給他們抬價格，賺麻了」。

### 2. bloXroute（114 次提及）

群裡有 bloXroute 員工直接參與討論，利益相關已聲明。

- 核心賣點：super bundle 功能——「大家的 tip 匯到一起給的更高，會更快一點，而且 rate 限制更少（bloxroute 有 jito 的最高權限帳戶）」
- 價格：最便宜 $1,250/月，有人看到 $1,500-$1,800 方案，「bloxroute 確實貴一點」
- 功能：同時提供 gRPC 數據 + SWQoS 質押權重上鏈
- 爭議：後來從免費轉收費，「bloxroute 現在收費了」

### 3. NextBlock（78 次提及）

定位抗 MEV 上鏈服務，群裡評價兩極。

- 正面：「今天狙 void 成功的 3 大哥 2 個用的都是 nextblock」、「綜合來說 nextblock 用的人是最多的」
- 負面：多人反映「用 nextblock 上鏈還會被夾」、「今晚好幾個也是使用 nextblock 被夾的」
- 防夾：有 antimev 選項，「之前用 nextblock 沒走 antimev，被夾到天上去了」
- 價格：起碼 999 套餐，每個 tier 速度一樣只是 TPS limit 不同
- 速度：「nextblock 比 jito 快嗎？」——「不一定」、「變量挺多的，建議都測測」
- 功能：也提供 gRPC 訂閱和交易提交

### 4. Temporal / Nozomi（16 次提及）

較新服務商，小費地址 `TEMPaMeCRFAS9EKF53Jd6KpHxgL47uWLcpFArU1Fanq`。

- 速度：「temporal 和 bloxroute 比誰快？」——「很難說」
- 小費：最低 0.001 SOL，群裡覺得「這可太高了」
- Bundle：「temporal 能不能像 jito 那樣 bundle？」——有 bundle 服務但文檔不完整
- IP 限制：有人問「temporal 有 ip 限制嗎」，未得到明確答案
- 缺點：「好像沒有 revert protection」

### 5. Flashblock（7 次提及，但評價具體）

性價比路線。

- 免費方案：「免費用 10 TPS，小費最低 0.0001 起」
- 速度：「好像打不過 0slot 但是跟 nextblock 比起來又好用」
- 評價：「經典的買不進、賣不出，勝在便宜」
- 套利關鍵：提供 SWQoS 上鏈服務，「套利 swqos 好像只有 flashblock、Astralane 這兩家可以吧」
- 官方進群推廣過免費 Stream 試用

### 6. Astralane（7 次提及）

專注 SWQoS 的服務商。

- 小費：最低 0.00001 SOL（所有服務中最低）
- 定位：SWQoS 通道出租，比較小眾
- 活動：在深圳和倫敦辦過 MEV Snackdown 線下活動
- 有人提到「astralane 還補貼了 2 SOL」

### 7. SWQoS（Stake-Weighted Quality of Service，41 次提及）

非具體服務商，而是 Solana 原生機制。質押越多的驗證節點，發交易的優先權越高。

- 門檻：「不是至少 20000 SOL 才有權重嗎？低於 20000 個只是有質押收益，享受不到 SWQoS 權重」
- 頂級玩家共識：「頂級 bot 應該都是在自己的質押節點上干的，shreds 優先、swqos 優先」
- 普通人入口：透過 Flashblock、Astralane 等第三方租用 SWQoS 通道
- 群裡判斷：「速度要上 swqos」、「我看有套利大佬用 swqos 的，他們那個一天 10 個 sol 叫賺的少」

### 實戰用法：多通道並發

群裡玩家不只依賴一家，而是多通道同時發：

- 「jito、temp、0slot 一起發」
- 「之前是 jito、blox 和 temp 這三個用的多一些」
- 「JUP + FAST + Nextblock + JITO，但是我這個跑法成本很高」
- 「現在發交易用 jito 還是 blox 還是 nextblock 還是 temp 啊？」——「選哪個更快，同等 gas 下」

邏輯：不同 slot 的 leader 節點跟各服務商的連接速度不同，多通道同時發可以提高命中率。

### 服務商對比總表

| 服務商 | 最低小費 | 防夾 | 價格 | 速度評價 | 適合場景 |
|---|---|---|---|---|---|
| Jito | 10000 lamports | Bundle 原子性 | 免費（按 tip） | 基準線 | 通用 |
| 0slot | 0.001 SOL | 無 | 按 tip | 比 Jito 快 | 狙擊 / 套利 |
| bloXroute | — | super bundle | $1,250+/月 | 跟 Jito 互有勝負 | 大資金 / 高頻 |
| NextBlock | — | antimev 選項 | $999+/月 | 不穩定 | 狙擊（需開 antimev） |
| Temporal | 0.001 SOL | 無 revert protection | 按 tip | 「很難說」 | 多通道備選 |
| Flashblock | 0.0001 SOL | — | 免費 10 TPS | 弱於 0slot | 低成本套利 / SWQoS |
| Astralane | 0.00001 SOL | — | — | — | SWQoS 套利 |
| SWQoS 自建 | — | — | 質押 20000+ SOL | 最快 | 頂級玩家 |
