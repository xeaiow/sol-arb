use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use dashmap::DashMap;
use log::{info, debug};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;

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
            update_channel_size: 4096,
        }
    }
}

/// A vault address waiting for balance fetch, with metadata to update the right pool field.
struct PendingVault {
    vault: Pubkey,
    is_vault_a: bool,
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
}

impl PoolStreamer {
    pub fn new(config: PoolStreamerConfig) -> (Self, mpsc::Receiver<PoolUpdate>) {
        let (update_tx, update_rx) = mpsc::channel(config.update_channel_size);
        let cache = Arc::new(PoolStateCache::new(update_tx));
        let rpc = Arc::new(RpcClient::new(config.rpc_url));

        let streamer = Self {
            cache,
            rpc,
            pending_subscriptions: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            pending_vaults: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            pending_discoveries: DashMap::new(),
            latest_blockhash: Arc::new(tokio::sync::RwLock::new(None)),
            latest_blockhash_slot: Arc::new(AtomicU64::new(0)),
        };

        (streamer, update_rx)
    }

    pub fn cache(&self) -> Arc<PoolStateCache> {
        self.cache.clone()
    }

    /// Process a DexEvent — handles discovery, state updates, and vault balance updates
    pub async fn on_event(&self, event: DexEvent) {
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
                self.handle_token_account_update(
                    &token_event.pubkey,
                    balance,
                    token_event.metadata.slot,
                );
            }
        }

        // 4. Handle CLMM tick array updates
        if let DexEvent::RaydiumClmmTickArrayStateAccountEvent(ref ta_event) = event {
            let ta = decoder::raydium_clmm::tick_array_from_state(&ta_event.tick_array_state);
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

        // Subscribe to pool account so gRPC pushes its snapshot
        {
            let mut pending = self.pending_subscriptions.lock().await;
            pending.push(discovered.address.to_string());
        }

        self.pending_discoveries.insert(discovered.address, discovered);
    }

    /// Handle a pool account update from gRPC.
    /// If this pool is in pending_discoveries, finalize registration.
    /// If already registered, just update math.
    async fn handle_account_update(&self, mut pool_state: PoolState) {
        if self.cache.contains(&pool_state.address) {
            // Existing pool — just update math
            self.cache.update_math(
                &pool_state.address,
                pool_state.math,
                pool_state.last_updated_slot,
            );
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

        info!(
            "Pool registered: {} {:?} ({} / {})",
            pool_state.address, pool_state.dex_type, pool_state.mint_a, pool_state.mint_b
        );

        // For CLMM pools: fetch tick arrays and AmmConfig fee_rate (blocking, needed before insert)
        // TODO: eliminate this RPC call by subscribing to AmmConfig/TickArray accounts via gRPC
        if pool_state.dex_type == DexType::RaydiumClmm {
            let addr = pool_state.address;
            self.fetch_clmm_extras(&addr, &mut pool_state).await;
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
    }

    fn handle_token_account_update(&self, token_account: &Pubkey, balance: u64, slot: u64) {
        if let Some(is_a) = self.cache.is_vault_a(token_account) {
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
        let start_indices =
            decoder::raydium_clmm::tick_array_start_indices(tick_current, tick_spacing);
        let mut pending = self.pending_subscriptions.lock().await;

        let mut loaded = 0u32;
        let mut failed = 0u32;
        for start_index in start_indices {
            if let Some(pda) = decoder::raydium_clmm::tick_array_pda(pool_address, start_index) {
                match self.rpc.get_account_data(&pda).await {
                    Ok(ta_data) => {
                        if let Some(ta) = decoder::raydium_clmm::decode_tick_array(&ta_data) {
                            tick_arrays.push(ta);
                            self.cache.register_tick_array(pda, *pool_address);
                            pending.push(pda.to_string());
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

    /// Process pending tick array reload requests (call periodically from main loop).
    /// Fetches new tick arrays for CLMM pools where tick_current moved out of range.
    pub async fn flush_tick_reloads(&self) {
        let requests = self.cache.drain_tick_reload_requests().await;
        if requests.is_empty() {
            return;
        }

        for req in requests {
            let start_indices =
                decoder::raydium_clmm::tick_array_start_indices(req.tick_current, req.tick_spacing);
            let mut new_tick_arrays = Vec::with_capacity(3);
            let mut pending = self.pending_subscriptions.lock().await;

            for start_index in start_indices {
                if let Some(pda) = decoder::raydium_clmm::tick_array_pda(&req.pool_address, start_index) {
                    // Register mapping regardless of fetch success (for future gRPC updates)
                    self.cache.register_tick_array(pda, req.pool_address);
                    pending.push(pda.to_string());

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

            if !new_tick_arrays.is_empty() {
                info!(
                    "CLMM {} tick array reload: {} loaded (tick={})",
                    req.pool_address, new_tick_arrays.len(), req.tick_current
                );
                self.cache.replace_tick_arrays(&req.pool_address, new_tick_arrays, 0);
            }
        }
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

        // Process in chunks of 100 (RPC limit for getMultipleAccounts)
        for chunk in vaults.chunks(100) {
            let pubkeys: Vec<Pubkey> = chunk.iter().map(|v| v.vault).collect();
            match self.rpc.get_multiple_accounts(&pubkeys).await {
                Ok(accounts) => {
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
                                    1,
                                );
                            }
                        }
                    }
                    info!("Batch vault fetch: {}/{} succeeded",
                        accounts.iter().filter(|a| a.is_some()).count(), chunk.len());
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
