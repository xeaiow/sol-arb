use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;
use super::state::{PoolMath, PoolState, PoolUpdate, TickArray};

/// Request to reload tick arrays for a CLMM pool whose tick moved out of range.
pub struct TickArrayReloadRequest {
    pub pool_address: Pubkey,
    pub tick_current: i32,
    pub tick_spacing: u16,
}

/// Thread-safe pool state cache with vault reverse index.
pub struct PoolStateCache {
    /// Main pool storage: pool address → PoolState
    pools: DashMap<Pubkey, PoolState>,
    /// Reverse index: vault address → pool address
    vault_to_pool: DashMap<Pubkey, Pubkey>,
    /// Reverse index: tick array address → pool address
    tick_array_to_pool: DashMap<Pubkey, Pubkey>,
    /// Channel to emit updates to downstream consumers
    update_tx: mpsc::Sender<PoolUpdate>,
    /// Queue of CLMM pools that need tick array reload
    tick_reload_queue: tokio::sync::Mutex<Vec<TickArrayReloadRequest>>,
}

impl PoolStateCache {
    /// Create a new cache that emits updates on the given channel.
    pub fn new(update_tx: mpsc::Sender<PoolUpdate>) -> Self {
        Self {
            pools: DashMap::new(),
            vault_to_pool: DashMap::new(),
            tick_array_to_pool: DashMap::new(),
            update_tx,
            tick_reload_queue: tokio::sync::Mutex::new(Vec::new()),
        }
    }

    /// Insert a pool, register its vault mappings, and emit initial PoolUpdate.
    pub fn insert(&self, pool: PoolState) {
        let address = pool.address;
        if let Some(vault) = pool.vault_a {
            self.vault_to_pool.insert(vault, address);
        }
        if let Some(vault) = pool.vault_b {
            self.vault_to_pool.insert(vault, address);
        }

        // Emit initial PoolUpdate so Engine knows about this pool
        let update = PoolUpdate {
            pool_address: pool.address,
            dex_type: pool.dex_type,
            mint_a: pool.mint_a,
            mint_b: pool.mint_b,
            vault_a: pool.vault_a,
            vault_b: pool.vault_b,
            mint_a_is_2022: pool.mint_a_is_2022,
            mint_b_is_2022: pool.mint_b_is_2022,
            extra_accounts: pool.extra_accounts.clone(),
            math: pool.math.clone(),
            slot: pool.last_updated_slot,
        };
        let _ = self.update_tx.try_send(update);

        self.pools.insert(address, pool);
    }

    /// Check if a pool exists by address.
    pub fn contains(&self, address: &Pubkey) -> bool {
        self.pools.contains_key(address)
    }

    /// Get a reference to a pool by address.
    pub fn get(&self, address: &Pubkey) -> Option<dashmap::mapref::one::Ref<'_, Pubkey, PoolState>> {
        self.pools.get(address)
    }

    /// Look up a pool address by vault address.
    pub fn pool_by_vault(&self, vault: &Pubkey) -> Option<Pubkey> {
        self.vault_to_pool.get(vault).map(|r| *r.value())
    }

    /// Update pool math and emit a PoolUpdate downstream.
    /// For CLMM pools, preserves existing tick_arrays (they come from separate accounts).
    /// If tick_current moved outside the loaded tick array range, queues a reload request.
    pub fn update_math(&self, address: &Pubkey, math: PoolMath, slot: u64) {
        if let Some(mut pool) = self.pools.get_mut(address) {
            // For CLMM: preserve tick_arrays and fee_rate from existing state,
            // because the new math from decode has tick_arrays=[] and fee_rate=0.
            let mut needs_tick_reload = false;
            let math = if let (
                PoolMath::Concentrated { tick_arrays: ref existing_ta, fee_rate: existing_fee, tick_current: old_tick, tick_spacing: _old_spacing, .. },
                PoolMath::Concentrated { sqrt_price_x64, liquidity, tick_current, tick_spacing, tick_arrays: ref new_ta, fee_rate: new_fee, .. },
            ) = (&pool.math, &math) {
                let tick_arrays = if new_ta.is_empty() { existing_ta.clone() } else { new_ta.clone() };

                // Check if tick_current moved outside the loaded tick array range
                if !tick_arrays.is_empty() && *tick_current != *old_tick {
                    let covered = tick_arrays.iter().any(|ta| {
                        let ta_end = ta.start_tick_index + *tick_spacing as i32 * 60;
                        *tick_current >= ta.start_tick_index && *tick_current < ta_end
                    });
                    if !covered {
                        needs_tick_reload = true;
                    }
                }

                let fee_rate = if *new_fee != 0 { *new_fee } else { *existing_fee };
                let (limit_a, limit_b) = super::state::compute_clmm_limits(
                    *sqrt_price_x64, *liquidity, *tick_current, &tick_arrays,
                );
                PoolMath::Concentrated {
                    sqrt_price_x64: *sqrt_price_x64,
                    liquidity: *liquidity,
                    tick_current: *tick_current,
                    tick_spacing: *tick_spacing,
                    fee_rate,
                    tick_arrays,
                    limit_in_a: limit_a,
                    limit_in_b: limit_b,
                }
            } else if let (
                PoolMath::ConstantProduct { reserve_a: existing_a, reserve_b: existing_b, fee_numerator: existing_fn, fee_denominator: existing_fd },
                PoolMath::ConstantProduct { reserve_a: new_a, reserve_b: new_b, .. },
            ) = (&pool.math, &math) {
                // CP pools (AMM V4, CPMM, PumpSwap, Meteora): decoders emit reserves=0.
                // Preserve existing vault-balance-derived reserves unless the new math
                // has non-zero reserves (e.g. Bonk which reads reserves from pool account).
                let ra = if *new_a != 0 { *new_a } else { *existing_a };
                let rb = if *new_b != 0 { *new_b } else { *existing_b };
                // Always preserve existing fees — they were set correctly at pool
                // registration by streamer (fetch_cpmm_fee / apply_pumpswap_fee /
                // decoder for AMM V4/Bonk/Meteora). Decoder re-decodes return
                // hardcoded defaults that would overwrite the correct values.
                PoolMath::ConstantProduct {
                    reserve_a: ra,
                    reserve_b: rb,
                    fee_numerator: *existing_fn,
                    fee_denominator: *existing_fd,
                }
            } else {
                math
            };

            // Queue tick reload if needed (before updating pool, to capture new tick_current)
            if needs_tick_reload {
                if let PoolMath::Concentrated { tick_current, tick_spacing, .. } = &math {
                    log::debug!("CLMM {} tick moved out of range (tick={}), queuing tick array reload",
                        address, tick_current);
                    // Use try_lock to avoid blocking — if locked, skip (next update will catch it)
                    if let Ok(mut queue) = self.tick_reload_queue.try_lock() {
                        // Don't queue duplicates
                        if !queue.iter().any(|r| r.pool_address == *address) {
                            queue.push(TickArrayReloadRequest {
                                pool_address: *address,
                                tick_current: *tick_current,
                                tick_spacing: *tick_spacing,
                            });
                        }
                    }
                }
            }

            pool.math = math.clone();
            pool.last_updated_slot = slot;

            let update = PoolUpdate {
                pool_address: pool.address,
                dex_type: pool.dex_type,
                mint_a: pool.mint_a,
                mint_b: pool.mint_b,
                vault_a: pool.vault_a,
                vault_b: pool.vault_b,
                mint_a_is_2022: pool.mint_a_is_2022,
                mint_b_is_2022: pool.mint_b_is_2022,
                extra_accounts: pool.extra_accounts.clone(),
                math,
                slot,
            };
            let _ = self.update_tx.try_send(update);
        }
    }

    /// Update a vault balance for a ConstantProduct pool from an on-chain
    /// token account balance change.
    pub fn update_vault_balance(
        &self,
        vault_address: &Pubkey,
        balance: u64,
        is_vault_a: bool,
        slot: u64,
    ) {
        let pool_address = match self.pool_by_vault(vault_address) {
            Some(addr) => addr,
            None => return,
        };

        if let Some(mut pool) = self.pools.get_mut(&pool_address) {
            if let PoolMath::ConstantProduct {
                ref mut reserve_a,
                ref mut reserve_b,
                ..
            } = pool.math
            {
                if is_vault_a {
                    *reserve_a = balance;
                } else {
                    *reserve_b = balance;
                }
                pool.last_updated_slot = slot;

                let update = PoolUpdate {
                    pool_address: pool.address,
                    dex_type: pool.dex_type,
                    mint_a: pool.mint_a,
                    mint_b: pool.mint_b,
                    vault_a: pool.vault_a,
                    vault_b: pool.vault_b,
                    mint_a_is_2022: pool.mint_a_is_2022,
                    mint_b_is_2022: pool.mint_b_is_2022,
                    extra_accounts: pool.extra_accounts.clone(),
                    math: pool.math.clone(),
                    slot,
                };
                let _ = self.update_tx.try_send(update);
            }
        }
    }

    /// Determine whether a vault address corresponds to vault_a (true) or vault_b (false).
    pub fn is_vault_a(&self, vault_address: &Pubkey) -> Option<bool> {
        let pool_address = self.pool_by_vault(vault_address)?;
        let pool = self.pools.get(&pool_address)?;
        if pool.vault_a.as_ref() == Some(vault_address) {
            Some(true)
        } else if pool.vault_b.as_ref() == Some(vault_address) {
            Some(false)
        } else {
            None
        }
    }

    /// Return all registered vault addresses.
    pub fn all_vault_addresses(&self) -> Vec<Pubkey> {
        self.vault_to_pool.iter().map(|r| *r.key()).collect()
    }

    /// Return all pool addresses.
    pub fn all_pool_addresses(&self) -> Vec<Pubkey> {
        self.pools.iter().map(|r| *r.key()).collect()
    }

    /// Register a tick array → pool mapping.
    pub fn register_tick_array(&self, tick_array_address: Pubkey, pool_address: Pubkey) {
        self.tick_array_to_pool.insert(tick_array_address, pool_address);
    }

    /// Look up a pool address by tick array address.
    pub fn pool_by_tick_array(&self, tick_array_address: &Pubkey) -> Option<Pubkey> {
        self.tick_array_to_pool.get(tick_array_address).map(|r| *r.value())
    }

    /// Update a tick array in a CLMM pool and emit a PoolUpdate.
    pub fn update_tick_array(
        &self,
        tick_array_address: &Pubkey,
        new_tick_array: TickArray,
        slot: u64,
    ) {
        let pool_address = match self.pool_by_tick_array(tick_array_address) {
            Some(addr) => addr,
            None => return,
        };

        if let Some(mut pool) = self.pools.get_mut(&pool_address) {
            if let PoolMath::Concentrated {
                ref mut tick_arrays,
                ..
            } = pool.math
            {
                // Replace tick array with matching start_tick_index, or append
                let start = new_tick_array.start_tick_index;
                if let Some(existing) = tick_arrays.iter_mut().find(|ta| ta.start_tick_index == start) {
                    *existing = new_tick_array;
                } else {
                    tick_arrays.push(new_tick_array);
                }
            }
            // Recompute limits after modifying tick arrays
            if let PoolMath::Concentrated {
                sqrt_price_x64, liquidity, tick_current, ref tick_arrays,
                ref mut limit_in_a, ref mut limit_in_b, ..
            } = pool.math {
                let (la, lb) = super::state::compute_clmm_limits(
                    sqrt_price_x64, liquidity, tick_current, tick_arrays,
                );
                *limit_in_a = la;
                *limit_in_b = lb;
            }

            if matches!(pool.math, PoolMath::Concentrated { .. }) {
                pool.last_updated_slot = slot;

                let update = PoolUpdate {
                    pool_address: pool.address,
                    dex_type: pool.dex_type,
                    mint_a: pool.mint_a,
                    mint_b: pool.mint_b,
                    vault_a: pool.vault_a,
                    vault_b: pool.vault_b,
                    mint_a_is_2022: pool.mint_a_is_2022,
                    mint_b_is_2022: pool.mint_b_is_2022,
                    extra_accounts: pool.extra_accounts.clone(),
                    math: pool.math.clone(),
                    slot,
                };
                let _ = self.update_tx.try_send(update);
            }
        }
    }

    /// Drain pending tick array reload requests.
    pub async fn drain_tick_reload_requests(&self) -> Vec<TickArrayReloadRequest> {
        let mut queue = self.tick_reload_queue.lock().await;
        std::mem::take(&mut *queue)
    }

    /// Replace tick arrays for a CLMM pool and emit a PoolUpdate.
    pub fn replace_tick_arrays(&self, address: &Pubkey, new_tick_arrays: Vec<TickArray>, slot: u64) {
        if let Some(mut pool) = self.pools.get_mut(address) {
            if let PoolMath::Concentrated { ref mut tick_arrays, .. } = pool.math {
                *tick_arrays = new_tick_arrays;
            }
            // Recompute limits after replacing tick arrays
            if let PoolMath::Concentrated {
                sqrt_price_x64, liquidity, tick_current, ref tick_arrays,
                ref mut limit_in_a, ref mut limit_in_b, ..
            } = pool.math {
                let (la, lb) = super::state::compute_clmm_limits(
                    sqrt_price_x64, liquidity, tick_current, tick_arrays,
                );
                *limit_in_a = la;
                *limit_in_b = lb;
            }

            if matches!(pool.math, PoolMath::Concentrated { .. }) {
                pool.last_updated_slot = slot;

                let update = PoolUpdate {
                    pool_address: pool.address,
                    dex_type: pool.dex_type,
                    mint_a: pool.mint_a,
                    mint_b: pool.mint_b,
                    vault_a: pool.vault_a,
                    vault_b: pool.vault_b,
                    mint_a_is_2022: pool.mint_a_is_2022,
                    mint_b_is_2022: pool.mint_b_is_2022,
                    extra_accounts: pool.extra_accounts.clone(),
                    math: pool.math.clone(),
                    slot,
                };
                let _ = self.update_tx.try_send(update);
            }
        }
    }

    /// Return the number of pools in the cache.
    pub fn len(&self) -> usize {
        self.pools.len()
    }
}
