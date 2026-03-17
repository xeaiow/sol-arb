use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use dashmap::DashMap;
use log::{info, debug};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::{mpsc, Notify};

use crate::streaming::event_parser::DexEvent;
use super::cache::PoolStateCache;
use super::decoder;
use super::discovery::{self, DiscoveredPool};
use super::state::{DexType, PoolMath, PoolState, PoolUpdate};

/// Configuration for the pool streamer
pub struct PoolStreamerConfig {
    pub rpc_url: String,
    pub update_channel_size: usize,
}

impl Default for PoolStreamerConfig {
    fn default() -> Self {
        Self {
            rpc_url: "https://api.mainnet-beta.solana.com".to_string(),
            update_channel_size: 2_000_000,
        }
    }
}

/// A vault address waiting for balance fetch, with metadata to update the right pool field.
struct PendingVault {
    vault: Pubkey,
    is_vault_a: bool,
}

/// Cached PumpSwap fee rates (fetched once from GlobalConfig)
struct PumpSwapFees {
    /// Total fee in basis points (lp_fee + protocol_fee)
    total_fee_bps: u64,
}

/// Main pool streamer — integrates discovery, decoding, and caching
pub struct PoolStreamer {
    cache: Arc<PoolStateCache>,
    rpc: Arc<RpcClient>,
    pending_subscriptions: Arc<tokio::sync::Mutex<Vec<String>>>,
    /// Vaults waiting for batch RPC fetch
    pending_vaults: Arc<tokio::sync::Mutex<Vec<PendingVault>>>,
    /// Pools discovered from tx events, waiting for gRPC account data to arrive
    pending_discoveries: DashMap<Pubkey, DiscoveredPool>,
    /// Latest blockhash from gRPC BlockMeta events
    latest_blockhash: Arc<tokio::sync::RwLock<Option<Hash>>>,
    /// Slot of the latest blockhash
    latest_blockhash_slot: Arc<AtomicU64>,
    /// Cached PumpSwap GlobalConfig fee rates
    pumpswap_fees: tokio::sync::OnceCell<PumpSwapFees>,
    /// Notify: new accounts pending gRPC subscription
    subscription_notify: Arc<Notify>,
    /// Dirty flag: set when new accounts are pushed, cleared after drain
    subscription_dirty: Arc<AtomicBool>,
    /// Notify: new vaults pending initial balance fetch
    vault_notify: Arc<Notify>,
    /// Notify: CLMM pools need tick array reload
    tick_reload_notify: Arc<Notify>,
    /// Counter for vault balance updates (diagnostic)
    vault_update_count: AtomicU64,
    /// Counter for ALL token account events received (diagnostic)
    token_event_count: AtomicU64,
}

impl PoolStreamer {
    pub fn new(config: PoolStreamerConfig) -> (Self, mpsc::Receiver<PoolUpdate>) {
        let (update_tx, update_rx) = mpsc::channel(config.update_channel_size);
        let tick_reload_notify = Arc::new(Notify::new());
        let cache = Arc::new(PoolStateCache::new(update_tx, tick_reload_notify.clone()));
        let rpc = Arc::new(RpcClient::new(config.rpc_url));

        let streamer = Self {
            cache,
            rpc,
            pending_subscriptions: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            pending_vaults: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            pending_discoveries: DashMap::new(),
            latest_blockhash: Arc::new(tokio::sync::RwLock::new(None)),
            latest_blockhash_slot: Arc::new(AtomicU64::new(0)),
            pumpswap_fees: tokio::sync::OnceCell::new(),
            subscription_notify: Arc::new(Notify::new()),
            subscription_dirty: Arc::new(AtomicBool::new(false)),
            vault_notify: Arc::new(Notify::new()),
            tick_reload_notify,
            vault_update_count: AtomicU64::new(0),
            token_event_count: AtomicU64::new(0),
        };

        (streamer, update_rx)
    }

    pub fn cache(&self) -> Arc<PoolStateCache> {
        self.cache.clone()
    }

    pub fn rpc(&self) -> &RpcClient {
        &self.rpc
    }

    pub fn subscription_notify(&self) -> Arc<Notify> {
        self.subscription_notify.clone()
    }

    pub fn subscription_dirty(&self) -> Arc<AtomicBool> {
        self.subscription_dirty.clone()
    }

    pub fn vault_notify(&self) -> Arc<Notify> {
        self.vault_notify.clone()
    }

    pub fn tick_reload_notify(&self) -> Arc<Notify> {
        self.tick_reload_notify.clone()
    }

    /// Process a DexEvent — handles discovery, state updates, and vault balance updates
    pub async fn on_event(&self, event: DexEvent) {
        // Latency measurement: use event metadata
        {
            use std::sync::atomic::AtomicI64;
            static LAST_LOG: AtomicI64 = AtomicI64::new(0);
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
            let last = LAST_LOG.load(Ordering::Relaxed);
            // Log once per second
            if now_ms - last > 1000 {
                if LAST_LOG.compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed).is_ok() {
                    let meta = match &event {
                        DexEvent::PumpSwapBuyEvent(e) => Some(("PumpSwap", &e.metadata)),
                        DexEvent::PumpSwapSellEvent(e) => Some(("PumpSwap", &e.metadata)),
                        DexEvent::RaydiumCpmmSwapEvent(e) => Some(("CPMM", &e.metadata)),
                        DexEvent::MeteoraDlmmSwap2Event(e) => Some(("DLMM", &e.metadata)),
                        DexEvent::RaydiumClmmSwapEvent(e) => Some(("CLMM", &e.metadata)),
                        _ => None,
                    };
                    if let Some((dex, m)) = meta {
                        let lag_ms = if m.block_time_ms > 0 {
                            now_ms - m.block_time_ms
                        } else if m.block_time > 0 {
                            (now_ms / 1000 - m.block_time) * 1000
                        } else {
                            -1
                        };
                        info!("[GRPC_LAG] slot={} lag={}ms dex={} sig={}",
                            m.slot, lag_ms, dex, &m.signature.to_string()[..16]);
                    }
                }
            }
        }

        // 1. Try pool discovery from transaction events
        if let Some(discovered) = discovery::discover_pool(&event) {
            self.handle_discovery(discovered).await;
        }

        // 2. Try direct pool state update from account events
        if let Some(pool_state) = decoder::pool_state_from_event(&event) {
            self.handle_account_update(pool_state).await;
        }

        // 3. Handle vault token account balance updates
        if let DexEvent::TokenAccountEvent(ref token_event) = event {
            if let Some(balance) = token_event.amount {
                // Log all token account events to diagnose gRPC vault update delivery
                let known = self.cache.is_vault_a(&token_event.pubkey).is_some();
                if known {
                    self.handle_token_account_update(
                        &token_event.pubkey,
                        balance,
                        token_event.metadata.slot,
                    );
                }
                // Count ALL token account events (whether known vault or not)
                let total = self.token_event_count.fetch_add(1, Ordering::Relaxed) + 1;
                if total <= 5 || total % 5000 == 0 {
                    info!("TokenAccountEvent #{}: pubkey={} balance={} slot={} known_vault={}",
                        total, token_event.pubkey, balance, token_event.metadata.slot, known);
                }
            }
        }

        // 4a. Handle DLMM bin array updates
        if let DexEvent::MeteoraDlmmBinArrayAccountEvent(ref ba_event) = event {
            let known = self.cache.pool_by_tick_array(&ba_event.pubkey).is_some();
            if let Some(ba) = decoder::meteora_dlmm::decode_bin_array(&ba_event.data) {
                debug!("[DLMM_BA] gRPC update: ba={} index={} bins_with_liq={} slot={} known={}",
                    ba_event.pubkey, ba.index, ba.bins.len(), ba_event.metadata.slot, known);
                self.cache.update_bin_array(&ba_event.pubkey, ba, ba_event.metadata.slot);
            } else {
                debug!("[DLMM_BA] gRPC decode failed: ba={} data_len={}", ba_event.pubkey, ba_event.data.len());
            }
        }

        // 4. Handle CLMM tick array updates (Raydium CLMM)
        if let DexEvent::RaydiumClmmTickArrayStateAccountEvent(ref ta_event) = event {
            let ta = decoder::raydium_clmm::tick_array_from_state(&ta_event.tick_array_state);
            self.cache.update_tick_array(&ta_event.pubkey, ta, ta_event.metadata.slot);
        }

        // 4b. Handle Orca Whirlpool tick array updates
        if let DexEvent::OrcaWhirlpoolTickArrayAccountEvent(ref ta_event) = event {
            // Need tick_spacing from the pool to correctly compute absolute tick indices
            let tick_spacing = self.cache.get(&ta_event.whirlpool)
                .and_then(|p| match p.math {
                    PoolMath::Concentrated { tick_spacing, .. } => Some(tick_spacing),
                    _ => None,
                })
                .unwrap_or(1); // fallback: spacing=1 preserves old behavior if pool not cached yet
            let ta = decoder::orca_whirlpool::tick_array_from_event(ta_event, tick_spacing);
            self.cache.update_tick_array(&ta_event.pubkey, ta, ta_event.metadata.slot);
        }

        // 5. Handle BlockMeta events (blockhash for executor)
        if let DexEvent::BlockMetaEvent(ref block_event) = event {
            if let Ok(hash) = block_event.block_hash.parse::<Hash>() {
                let prev_slot = self.latest_blockhash_slot.load(Ordering::Relaxed);
                if block_event.slot > prev_slot {
                    *self.latest_blockhash.write().await = Some(hash);
                    self.latest_blockhash_slot.store(block_event.slot, Ordering::Relaxed);
                }
            }
        }
    }

    /// Handle a newly discovered pool — no RPC calls, just queue for gRPC account data
    async fn handle_discovery(&self, discovered: DiscoveredPool) {
        // Already registered
        if self.cache.contains(&discovered.address) {
            return;
        }
        // Already pending
        if self.pending_discoveries.contains_key(&discovered.address) {
            return;
        }

        debug!("Discovered new pool: {} ({:?}), subscribing via gRPC",
            discovered.address, discovered.dex_type);

        // Pool accounts are owned by DEX programs → already covered by owner filter.
        // No need to add to explicit subscription list.

        self.pending_discoveries.insert(discovered.address, discovered);
    }

    /// Handle a pool account update from gRPC.
    /// If this pool is in pending_discoveries, finalize registration.
    /// If already registered, just update math.
    pub async fn handle_account_update(&self, mut pool_state: PoolState) {
        if self.cache.contains(&pool_state.address) {
            // Existing pool — update math
            self.cache.update_math(
                &pool_state.address,
                pool_state.math,
                pool_state.last_updated_slot,
            );

            // Bootstrap pools have reserve=0 because their vaults were never
            // subscribed via gRPC. When we see a pool account update (someone
            // traded), subscribe to its vaults now so we get balance updates.
            let needs_vaults = if let Some(pool) = self.cache.get(&pool_state.address) {
                match &pool.math {
                    PoolMath::ConstantProduct { reserve_a, reserve_b, .. } => {
                        *reserve_a == 0 && *reserve_b == 0
                    }
                    _ => false,
                }
            } else {
                false
            };

            if needs_vaults {
                if let Some(pool) = self.cache.get(&pool_state.address) {
                    let va = pool.vault_a;
                    let vb = pool.vault_b;
                    drop(pool);
                    let mut pending_sub = self.pending_subscriptions.lock().await;
                    let mut pending_vault = self.pending_vaults.lock().await;
                    if let Some(va) = va {
                        pending_sub.push(va.to_string());
                        pending_vault.push(PendingVault { vault: va, is_vault_a: true });
                    }
                    if let Some(vb) = vb {
                        pending_sub.push(vb.to_string());
                        pending_vault.push(PendingVault { vault: vb, is_vault_a: false });
                    }
                    drop(pending_sub);
                    drop(pending_vault);
                    self.subscription_dirty.store(true, Ordering::Release);
                    self.subscription_notify.notify_waiters();
                    self.vault_notify.notify_one();
                }
            }

            return;
        }

        // Check if this was a pending discovery
        let discovered = self.pending_discoveries.remove(&pool_state.address)
            .map(|(_, d)| d);

        // Patch mints from discovery event if decoder couldn't extract them
        if let Some(ref disc) = discovered {
            if pool_state.mint_a == Pubkey::default() {
                if let Some(mint) = disc.mint_a {
                    pool_state.mint_a = mint;
                }
            }
            if pool_state.mint_b == Pubkey::default() {
                if let Some(mint) = disc.mint_b {
                    pool_state.mint_b = mint;
                }
            }
        }

        // Skip if mints are still unknown
        if pool_state.mint_a == Pubkey::default() || pool_state.mint_b == Pubkey::default() {
            debug!("Pool {} has unknown mints, skipping", pool_state.address);
            return;
        }

        // Queue vault token accounts for gRPC subscription (to get balance updates).
        // Pool accounts are already covered by the owner filter (DEX program IDs),
        // but vaults are SPL Token accounts and need explicit subscription.
        {
            let mut pending = self.pending_subscriptions.lock().await;
            if let Some(vault_a) = &pool_state.vault_a {
                pending.push(vault_a.to_string());
            }
            if let Some(vault_b) = &pool_state.vault_b {
                pending.push(vault_b.to_string());
            }
        }
        self.subscription_dirty.store(true, Ordering::Release);
        self.subscription_notify.notify_waiters();

        info!(
            "Pool registered: {} {:?} ({} / {}) [vaults_tracked: {}]",
            pool_state.address, pool_state.dex_type, pool_state.mint_a, pool_state.mint_b,
            self.cache.vault_count(),
        );

        // For CLMM pools: fetch tick arrays and AmmConfig fee_rate (blocking, needed before insert)
        // TODO: eliminate this RPC call by subscribing to AmmConfig/TickArray accounts via gRPC
        if pool_state.dex_type == DexType::RaydiumClmm {
            let addr = pool_state.address;
            self.fetch_clmm_extras(&addr, &mut pool_state).await;
        }

        // For DLMM pools: fetch bin arrays (3 around active_id)
        if pool_state.dex_type == DexType::MeteoraDlmm {
            let addr = pool_state.address;
            self.fetch_dlmm_bin_arrays(&addr, &mut pool_state).await;
        }

        // For Orca Whirlpool pools: fetch tick arrays (fee_rate already in pool state)
        if pool_state.dex_type == DexType::OrcaWhirlpool {
            let addr = pool_state.address;
            self.fetch_whirlpool_tick_arrays(&addr, &mut pool_state).await;
        }

        // For CPMM pools: fetch AmmConfig trade_fee_rate (extra_accounts[0] = amm_config)
        if pool_state.dex_type == DexType::RaydiumCpmm {
            self.fetch_cpmm_fee(&mut pool_state).await;
        }

        // For PumpSwap pools: apply cached GlobalConfig fee rates
        if pool_state.dex_type == DexType::PumpSwap {
            self.apply_pumpswap_fee(&mut pool_state).await;
        }

        // Detect Token-2022 mints for DEXes that don't embed token program in pool state.
        // CPMM and DAMM V2 already detect this from pool account data.
        if !matches!(pool_state.dex_type, DexType::RaydiumCpmm | DexType::MeteoraDammV2) {
            self.detect_token_2022(&mut pool_state).await;
        }

        // Insert pool immediately (reserve may be 0).
        // Engine's reserve transition detection will build routes once balances arrive.
        self.cache.insert(pool_state.clone());

        // Queue vault addresses for batch RPC fetch
        {
            let mut pending = self.pending_vaults.lock().await;
            if let Some(va) = pool_state.vault_a {
                pending.push(PendingVault { vault: va, is_vault_a: true });
            }
            if let Some(vb) = pool_state.vault_b {
                pending.push(PendingVault { vault: vb, is_vault_a: false });
            }
        }
        self.vault_notify.notify_one();
    }

    /// Lightweight pool registration for bootstrap (gPA bulk load).
    /// Skips all RPC-intensive operations (tick arrays, bin arrays, AmmConfig, Token-2022 detect).
    /// Does NOT add vaults to gRPC subscription — 534K pools would produce 1M+ vault addresses
    /// and crash the gRPC connection. Vault balances are fetched via batch RPC instead.
    /// After batch fetch, pools with sufficient reserves become scannable.
    pub async fn register_pool_lightweight(&self, pool_state: PoolState) {
        if self.cache.contains(&pool_state.address) {
            return;
        }
        if pool_state.mint_a == Pubkey::default() || pool_state.mint_b == Pubkey::default() {
            return;
        }

        // Insert pool (no tick array, no bin array, no fee fetch)
        self.cache.insert(pool_state);

        // Do NOT queue vaults for gRPC subscription — too many addresses would crash gRPC.
        // Do NOT queue vaults for batch RPC fetch — 1M+ vaults would overwhelm RPC.
        // Vault balances will arrive when gRPC pushes pool account updates (owner filter),
        // triggering handle_account_update which queues vaults for the specific pools that
        // are actively traded.
    }

    fn handle_token_account_update(&self, token_account: &Pubkey, balance: u64, slot: u64) {
        if let Some(is_a) = self.cache.is_vault_a(token_account) {
            let count = self.vault_update_count.fetch_add(1, Ordering::Relaxed) + 1;
            if count % 1000 == 0 {
                info!("Vault balance updates received: {} total (latest: {} balance={} slot={})",
                    count, token_account, balance, slot);
            }
            self.cache.update_vault_balance(token_account, balance, is_a, slot);
        }
    }

    /// Fetch AmmConfig (for fee_rate) and tick arrays for a CLMM pool.
    /// TODO: eliminate RPC by subscribing to these accounts via gRPC
    async fn fetch_clmm_extras(&self, pool_address: &Pubkey, pool_state: &mut PoolState) {
        let PoolMath::Concentrated {
            ref mut fee_rate,
            ref mut tick_arrays,
            tick_current,
            tick_spacing,
            ..
        } = pool_state.math
        else {
            return;
        };

        // Fetch AmmConfig for fee_rate
        if let Ok(pool_data) = self.rpc.get_account_data(pool_address).await {
            use crate::streaming::event_parser::protocols::raydium_clmm::types::{
                pool_state_decode, POOL_STATE_SIZE,
            };
            if pool_data.len() >= POOL_STATE_SIZE + 8 {
                if let Some(ps) = pool_state_decode(&pool_data[8..POOL_STATE_SIZE + 8]) {
                    if let Ok(config_data) = self.rpc.get_account_data(&ps.amm_config).await {
                        if let Some(config) = decoder::raydium_clmm::decode_amm_config(&config_data) {
                            *fee_rate = config.trade_fee_rate;
                            debug!(
                                "CLMM {} fee_rate={} tick_spacing={}",
                                pool_address, config.trade_fee_rate, config.tick_spacing
                            );
                        }
                    }
                }
            }
        }

        // Fetch 3 tick arrays (left, current, right)
        // Tick arrays are owned by Raydium CLMM program → already covered by owner filter.
        // Only need to register in cache for reverse lookup, no explicit gRPC subscription.
        let start_indices =
            decoder::raydium_clmm::tick_array_start_indices(tick_current, tick_spacing);

        let mut loaded = 0u32;
        let mut failed = 0u32;
        for start_index in start_indices {
            if let Some(pda) = decoder::raydium_clmm::tick_array_pda(pool_address, start_index) {
                match self.rpc.get_account_data(&pda).await {
                    Ok(ta_data) => {
                        if let Some(ta) = decoder::raydium_clmm::decode_tick_array(&ta_data) {
                            tick_arrays.push(ta);
                            self.cache.register_tick_array(pda, *pool_address);
                            loaded += 1;
                        }
                    }
                    Err(e) => {
                        if failed == 0 {
                            info!("CLMM tick_array fetch err for {}: {}", pda, e);
                        }
                        failed += 1;
                    }
                }
            }
        }
        info!(
            "CLMM {} tick_arrays: {} loaded, {} failed (tick={}, spacing={})",
            pool_address, loaded, failed, tick_current, tick_spacing
        );
    }

    /// Fetch tick arrays for an Orca Whirlpool pool.
    /// fee_rate is already embedded in the pool state from the decoder.
    async fn fetch_whirlpool_tick_arrays(&self, pool_address: &Pubkey, pool_state: &mut PoolState) {
        let PoolMath::Concentrated {
            ref mut tick_arrays,
            tick_current,
            tick_spacing,
            ..
        } = pool_state.math
        else {
            return;
        };

        let start_indices =
            decoder::orca_whirlpool::tick_array_start_indices(tick_current, tick_spacing);

        let mut loaded = 0u32;
        let mut failed = 0u32;
        for start_index in start_indices {
            if let Some(pda) = decoder::orca_whirlpool::tick_array_pda(pool_address, start_index) {
                match self.rpc.get_account_data(&pda).await {
                    Ok(ta_data) => {
                        if let Some(ta) = decoder::orca_whirlpool::decode_tick_array(&ta_data, tick_spacing) {
                            tick_arrays.push(ta);
                            self.cache.register_tick_array(pda, *pool_address);
                            loaded += 1;
                        }
                    }
                    Err(e) => {
                        if failed == 0 {
                            info!("Whirlpool tick_array fetch err for {}: {}", pda, e);
                        }
                        failed += 1;
                    }
                }
            }
        }
        info!(
            "Whirlpool {} tick_arrays: {} loaded, {} failed (tick={}, spacing={})",
            pool_address, loaded, failed, tick_current, tick_spacing
        );
    }

    /// Fetch bin arrays for a DLMM pool (3 arrays around active_id).
    /// Only keeps arrays that actually exist on-chain.
    async fn fetch_dlmm_bin_arrays(&self, pool_address: &Pubkey, pool_state: &mut PoolState) {
        let active_id = match pool_state.math {
            PoolMath::MeteoraDlmm { active_id, .. } => active_id,
            _ => return,
        };

        let bin_pdas = decoder::meteora_dlmm::bin_array_pdas_for_swap(pool_address, active_id);

        let mut fetched_arrays = Vec::with_capacity(3);
        let mut existing_pdas = Vec::with_capacity(3);
        let mut loaded = 0u32;
        let mut failed = 0u32;

        for pda in &bin_pdas {
            match self.rpc.get_account_data(pda).await {
                Ok(data) => {
                    if let Some(ba) = decoder::meteora_dlmm::decode_bin_array(&data) {
                        fetched_arrays.push(ba);
                        existing_pdas.push(*pda);
                        self.cache.register_tick_array(*pda, *pool_address);
                        loaded += 1;
                    }
                }
                Err(_) => {
                    failed += 1;
                }
            }
        }

        // Update bin_arrays in math
        if let PoolMath::MeteoraDlmm { ref mut bin_arrays, .. } = pool_state.math {
            *bin_arrays = fetched_arrays;
        }

        // Update extra_accounts to only include existing bin array PDAs
        // Layout: [oracle, existing_bin_array_pdas...]
        if !pool_state.extra_accounts.is_empty() {
            let oracle = pool_state.extra_accounts[0];
            let mut new_extra = vec![oracle];
            new_extra.extend(existing_pdas);
            pool_state.extra_accounts = new_extra;
        }

        info!(
            "DLMM {} bin_arrays: {} loaded, {} not found (active_id={})",
            pool_address, loaded, failed, active_id
        );
    }

    /// Process pending bin array reload requests.
    /// Fetches new bin arrays for DLMM pools where active_id moved out of range.
    pub async fn flush_bin_array_reloads(&self) {
        let requests = self.cache.drain_bin_array_reload_requests().await;
        if requests.is_empty() {
            return;
        }

        for req in requests {
            let bin_pdas = decoder::meteora_dlmm::bin_array_pdas_for_swap(
                &req.pool_address,
                req.active_id,
            );

            let mut new_bin_arrays = Vec::with_capacity(3);
            let mut new_extra = Vec::with_capacity(4);

            // Preserve oracle from existing extra_accounts
            if let Some(pool) = self.cache.get(&req.pool_address) {
                if let Some(oracle) = pool.extra_accounts.first() {
                    new_extra.push(*oracle);
                }
            }

            for pda in &bin_pdas {
                self.cache.register_tick_array(*pda, req.pool_address);
                match self.rpc.get_account_data(pda).await {
                    Ok(data) => {
                        if let Some(ba) = decoder::meteora_dlmm::decode_bin_array(&data) {
                            new_bin_arrays.push(ba);
                            new_extra.push(*pda);
                        }
                    }
                    Err(_) => {
                        // Bin array doesn't exist — skip
                    }
                }
            }

            if !new_bin_arrays.is_empty() {
                info!(
                    "DLMM {} bin array reload: {} loaded (active_id={})",
                    req.pool_address, new_bin_arrays.len(), req.active_id
                );
                let reload_slot = self.cache.get(&req.pool_address)
                    .map(|p| p.last_updated_slot)
                    .unwrap_or(1);
                self.cache.replace_bin_arrays(&req.pool_address, new_bin_arrays, new_extra, reload_slot);
            }
        }
    }

    /// Apply PumpSwap fee rates from cached GlobalConfig.
    /// Fetches GlobalConfig once on first PumpSwap pool registration.
    async fn apply_pumpswap_fee(&self, pool_state: &mut PoolState) {
        const PUMPSWAP_GLOBAL_CONFIG: Pubkey =
            solana_sdk::pubkey!("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");

        let fees = self.pumpswap_fees.get_or_init(|| async {
            match self.rpc.get_account_data(&PUMPSWAP_GLOBAL_CONFIG).await {
                Ok(data) => {
                    use crate::streaming::event_parser::protocols::pumpswap::types::{
                        global_config_decode, GLOBAL_CONFIG_SIZE,
                    };
                    if data.len() >= GLOBAL_CONFIG_SIZE + 8 {
                        if let Some(config) = global_config_decode(&data[8..GLOBAL_CONFIG_SIZE + 8]) {
                            let total = config.lp_fee_basis_points + config.protocol_fee_basis_points;
                            info!("PumpSwap GlobalConfig: lp_fee={}bps, protocol_fee={}bps, total={}bps",
                                config.lp_fee_basis_points, config.protocol_fee_basis_points, total);
                            return PumpSwapFees { total_fee_bps: total };
                        }
                    }
                    info!("PumpSwap GlobalConfig decode failed, using default 200bps");
                    PumpSwapFees { total_fee_bps: 200 }
                }
                Err(e) => {
                    info!("PumpSwap GlobalConfig fetch failed: {}, using default 200bps", e);
                    PumpSwapFees { total_fee_bps: 200 }
                }
            }
        }).await;

        if let PoolMath::ConstantProduct {
            ref mut fee_numerator,
            ref mut fee_denominator,
            ..
        } = pool_state.math {
            *fee_numerator = fees.total_fee_bps;
            *fee_denominator = 10000;
        }
    }

    /// Detect Token-2022 mints by checking mint account owner.
    /// Mint accounts owned by TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb are Token-2022.
    async fn detect_token_2022(&self, pool_state: &mut PoolState) {
        const TOKEN_2022_PROGRAM_ID: Pubkey =
            solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

        let mints = vec![pool_state.mint_a, pool_state.mint_b];
        match self.rpc.get_multiple_accounts(&mints).await {
            Ok(accounts) => {
                if let Some(Some(acct)) = accounts.first() {
                    pool_state.mint_a_is_2022 = acct.owner == TOKEN_2022_PROGRAM_ID;
                }
                if let Some(Some(acct)) = accounts.get(1) {
                    pool_state.mint_b_is_2022 = acct.owner == TOKEN_2022_PROGRAM_ID;
                }
                if pool_state.mint_a_is_2022 || pool_state.mint_b_is_2022 {
                    info!(
                        "Token-2022 detected: {} {:?} mint_a_2022={} mint_b_2022={}",
                        pool_state.address, pool_state.dex_type,
                        pool_state.mint_a_is_2022, pool_state.mint_b_is_2022,
                    );
                }
            }
            Err(e) => {
                debug!("Token-2022 detection failed for {}: {}", pool_state.address, e);
            }
        }
    }

    /// Fetch AmmConfig trade_fee_rate for a CPMM pool.
    /// The amm_config address is stored in extra_accounts[0].
    async fn fetch_cpmm_fee(&self, pool_state: &mut PoolState) {
        let amm_config_addr = match pool_state.extra_accounts.first() {
            Some(addr) => *addr,
            None => return,
        };

        match self.rpc.get_account_data(&amm_config_addr).await {
            Ok(data) => {
                use crate::streaming::event_parser::protocols::raydium_cpmm::types::{
                    amm_config_decode, AMM_CONFIG_SIZE,
                };
                if data.len() >= AMM_CONFIG_SIZE + 8 {
                    if let Some(config) = amm_config_decode(&data[8..AMM_CONFIG_SIZE + 8]) {
                        // trade_fee_rate is in 1e-6 units (e.g. 2500 = 0.25%)
                        // Convert to fee_numerator/fee_denominator
                        if let PoolMath::ConstantProduct {
                            ref mut fee_numerator,
                            ref mut fee_denominator,
                            ..
                        } = pool_state.math {
                            *fee_numerator = config.trade_fee_rate;
                            *fee_denominator = 1_000_000;
                        }
                        debug!(
                            "CPMM {} fee_rate={} ({}bps)",
                            pool_state.address, config.trade_fee_rate,
                            config.trade_fee_rate / 100,
                        );
                    }
                }
            }
            Err(e) => {
                debug!("CPMM AmmConfig fetch failed for {}: {}", amm_config_addr, e);
            }
        }
    }

    /// Process pending tick array reload requests (call periodically from main loop).
    /// Fetches new tick arrays for CLMM pools where tick_current moved out of range.
    pub async fn flush_tick_reloads(&self) {
        let requests = self.cache.drain_tick_reload_requests().await;
        if requests.is_empty() {
            return;
        }

        for req in requests {
            // Determine if this is a Raydium CLMM or Orca Whirlpool pool
            let is_whirlpool = self.cache.get(&req.pool_address)
                .map(|p| p.dex_type == DexType::OrcaWhirlpool)
                .unwrap_or(false);

            let mut new_tick_arrays = Vec::with_capacity(7);

            if is_whirlpool {
                let start_indices =
                    decoder::orca_whirlpool::tick_array_start_indices(req.tick_current, req.tick_spacing);
                for start_index in start_indices {
                    if let Some(pda) = decoder::orca_whirlpool::tick_array_pda(&req.pool_address, start_index) {
                        self.cache.register_tick_array(pda, req.pool_address);
                        match self.rpc.get_account_data(&pda).await {
                            Ok(ta_data) => {
                                if let Some(ta) = decoder::orca_whirlpool::decode_tick_array(&ta_data, req.tick_spacing) {
                                    new_tick_arrays.push(ta);
                                }
                            }
                            Err(e) => {
                                debug!("Whirlpool tick array reload fetch err for {}: {}", pda, e);
                            }
                        }
                    }
                }
            } else {
                let start_indices =
                    decoder::raydium_clmm::tick_array_start_indices(req.tick_current, req.tick_spacing);
                for start_index in start_indices {
                    if let Some(pda) = decoder::raydium_clmm::tick_array_pda(&req.pool_address, start_index) {
                        self.cache.register_tick_array(pda, req.pool_address);
                        match self.rpc.get_account_data(&pda).await {
                            Ok(ta_data) => {
                                if let Some(ta) = decoder::raydium_clmm::decode_tick_array(&ta_data) {
                                    new_tick_arrays.push(ta);
                                }
                            }
                            Err(e) => {
                                debug!("Tick array reload fetch err for {}: {}", pda, e);
                            }
                        }
                    }
                }
            }

            if !new_tick_arrays.is_empty() {
                let dex_name = if is_whirlpool { "Whirlpool" } else { "CLMM" };
                info!(
                    "{} {} tick array reload: {} loaded (tick={})",
                    dex_name, req.pool_address, new_tick_arrays.len(), req.tick_current
                );
                // Use the pool's existing slot (not 0) to avoid dedup issues in scanner
                let reload_slot = self.cache.get(&req.pool_address)
                    .map(|p| p.last_updated_slot)
                    .unwrap_or(1);
                self.cache.replace_tick_arrays(&req.pool_address, new_tick_arrays, reload_slot);
            }
        }
    }

    /// Queue an account address for gRPC subscription.
    pub async fn queue_subscription(&self, address: String) {
        self.pending_subscriptions.lock().await.push(address);
    }

    /// Queue many subscription addresses at once (single lock).
    pub async fn queue_subscriptions_batch(&self, addresses: Vec<String>) {
        let mut pending = self.pending_subscriptions.lock().await;
        pending.extend(addresses);
    }

    /// Drain pending subscription addresses (call before update_subscription)
    pub async fn drain_pending_subscriptions(&self) -> Vec<String> {
        let mut pending = self.pending_subscriptions.lock().await;
        std::mem::take(&mut *pending)
    }

    pub fn pool_count(&self) -> usize {
        self.cache.len()
    }

    pub fn pending_count(&self) -> usize {
        self.pending_discoveries.len()
    }

    /// Get the latest blockhash from gRPC BlockMeta events.
    pub async fn latest_blockhash(&self) -> Option<(Hash, u64)> {
        let hash = self.latest_blockhash.read().await;
        hash.map(|h| (h, self.latest_blockhash_slot.load(Ordering::Relaxed)))
    }

    /// Get a shared handle to the blockhash (for Executor to read directly).
    pub fn latest_blockhash_handle(&self) -> Arc<tokio::sync::RwLock<Option<Hash>>> {
        self.latest_blockhash.clone()
    }

    /// Batch-fetch pending vault balances via getMultipleAccounts.
    /// Call this periodically (e.g. every 2 seconds) from the main loop.
    pub async fn flush_pending_vaults(&self) {
        let vaults: Vec<PendingVault> = {
            let mut pending = self.pending_vaults.lock().await;
            std::mem::take(&mut *pending)
        };

        if vaults.is_empty() {
            return;
        }

        // Process in chunks of 50 (getMultipleAccounts RPC limit)
        for chunk in vaults.chunks(50) {
            let pubkeys: Vec<Pubkey> = chunk.iter().map(|v| v.vault).collect();
            match self.rpc.get_multiple_accounts(&pubkeys).await {
                Ok(accounts) => {
                    let current_slot = self.latest_blockhash_slot.load(Ordering::Relaxed);
                    for (i, maybe_account) in accounts.iter().enumerate() {
                        if let Some(account) = maybe_account {
                            if account.data.len() >= 72 {
                                let balance = u64::from_le_bytes(
                                    account.data[64..72].try_into().unwrap_or([0; 8]),
                                );
                                self.cache.update_vault_balance(
                                    &chunk[i].vault,
                                    balance,
                                    chunk[i].is_vault_a,
                                    current_slot,
                                );
                            }
                        }
                    }
                    info!("Batch vault fetch: {}/{} succeeded (slot={})",
                        accounts.iter().filter(|a| a.is_some()).count(), chunk.len(), current_slot);
                }
                Err(e) => {
                    debug!("Batch vault fetch failed ({}), re-queuing {} vaults", e, chunk.len());
                    // Re-queue failed vaults for retry
                    let mut pending = self.pending_vaults.lock().await;
                    for v in chunk {
                        pending.push(PendingVault {
                            vault: v.vault,
                            is_vault_a: v.is_vault_a,
                        });
                    }
                }
            }
        }
    }
}
