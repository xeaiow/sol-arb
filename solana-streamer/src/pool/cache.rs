use std::sync::Arc;
use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::mpsc;
use super::state::{DlmmBinArray, PoolMath, PoolState, PoolUpdate, TickArray};

/// Request to reload tick arrays for a CLMM pool whose tick moved out of range.
pub struct TickArrayReloadRequest {
    pub pool_address: Pubkey,
    pub tick_current: i32,
    pub tick_spacing: u16,
}

/// Request to reload bin arrays for a DLMM pool whose active_id changed.
pub struct BinArrayReloadRequest {
    pub pool_address: Pubkey,
    pub active_id: i32,
}

/// Thread-safe pool state cache with vault reverse index.
pub struct PoolStateCache {
    /// Main pool storage: pool address → PoolState
    pools: DashMap<Pubkey, PoolState>,
    /// Reverse index: vault address → pool address
    vault_to_pool: DashMap<Pubkey, Pubkey>,
    /// Reverse index: tick array address → pool address
    tick_array_to_pool: DashMap<Pubkey, Pubkey>,
    /// Reverse index: token mint → Vec<pool address> (for cross-DEX arb)
    mint_to_pools: DashMap<Pubkey, Vec<Pubkey>>,
    /// Channel to emit updates to downstream consumers
    update_tx: mpsc::Sender<PoolUpdate>,
    /// Queue of CLMM pools that need tick array reload
    tick_reload_queue: tokio::sync::Mutex<Vec<TickArrayReloadRequest>>,
    /// Queue of DLMM pools that need bin array reload
    bin_array_reload_queue: tokio::sync::Mutex<Vec<BinArrayReloadRequest>>,
    /// Notify: CLMM pools need tick array reload
    tick_reload_notify: Arc<tokio::sync::Notify>,
}

impl PoolStateCache {
    /// Create a new cache that emits updates on the given channel.
    pub fn new(update_tx: mpsc::Sender<PoolUpdate>, tick_reload_notify: Arc<tokio::sync::Notify>) -> Self {
        Self {
            pools: DashMap::new(),
            vault_to_pool: DashMap::new(),
            tick_array_to_pool: DashMap::new(),
            mint_to_pools: DashMap::new(),
            update_tx,
            tick_reload_queue: tokio::sync::Mutex::new(Vec::new()),
            bin_array_reload_queue: tokio::sync::Mutex::new(Vec::new()),
            tick_reload_notify,
        }
    }

    /// Insert a pool, register its vault mappings, and emit initial PoolUpdate.
    pub fn insert(&self, mut pool: PoolState) {
        let address = pool.address;
        if let Some(vault) = pool.vault_a {
            self.vault_to_pool.insert(vault, address);
        }
        if let Some(vault) = pool.vault_b {
            self.vault_to_pool.insert(vault, address);
        }

        // Compute initial CLMM limits (streamer sets tick_arrays + fee_rate before insert,
        // but doesn't compute limits — they default to 0)
        if let PoolMath::Concentrated {
            sqrt_price_x64, liquidity, tick_current, fee_rate, ref tick_arrays,
            ref mut limit_in_a, ref mut limit_in_b, ..
        } = pool.math {
            if !tick_arrays.is_empty() {
                let (la, lb) = super::state::compute_clmm_limits_with_fee(
                    sqrt_price_x64, liquidity, tick_current, tick_arrays, fee_rate,
                );
                *limit_in_a = la;
                *limit_in_b = lb;
            }
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

        // Update mint→pools index
        for mint in [pool.mint_a, pool.mint_b] {
            let mut entry = self.mint_to_pools.entry(mint).or_default();
            if !entry.contains(&address) {
                entry.push(address);
            }
        }

        self.pools.insert(address, pool);
    }

    /// Get all pool addresses for a given token mint.
    pub fn pools_for_mint(&self, mint: &Pubkey) -> Vec<Pubkey> {
        self.mint_to_pools.get(mint).map(|v| v.clone()).unwrap_or_default()
    }

    /// Check if a pool exists by address.
    pub fn contains(&self, address: &Pubkey) -> bool {
        self.pools.contains_key(address)
    }

    /// Get a reference to a pool by address.
    pub fn get(&self, address: &Pubkey) -> Option<dashmap::mapref::one::Ref<'_, Pubkey, PoolState>> {
        self.pools.get(address)
    }

    pub fn get_mut(&self, address: &Pubkey) -> Option<dashmap::mapref::one::RefMut<'_, Pubkey, PoolState>> {
        self.pools.get_mut(address)
    }

    /// Number of vault addresses tracked in the reverse index.
    pub fn vault_count(&self) -> usize {
        self.vault_to_pool.len()
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
                let (limit_a, limit_b) = super::state::compute_clmm_limits_with_fee(
                    *sqrt_price_x64, *liquidity, *tick_current, &tick_arrays, fee_rate,
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
            } else if let (
                PoolMath::DammV2Concentrated { .. },
                PoolMath::DammV2Concentrated { .. },
            ) = (&pool.math, &math) {
                // DAMM V2 CL: the full pool account is re-decoded on each gRPC update,
                // so all fields (sqrt_price, liquidity, etc.) come fresh. Just use new math.
                math
            } else if let (
                PoolMath::MeteoraDlmm {
                    bin_arrays: ref existing_ba,
                    active_id: old_active_id,
                    ..
                },
                PoolMath::MeteoraDlmm {
                    active_id: new_active_id,
                    bin_step,
                    base_factor,
                    variable_fee_control,
                    max_volatility_accumulator,
                    volatility_accumulator,
                    volatility_reference,
                    index_reference,
                    bin_arrays: ref new_ba,
                },
            ) = (&pool.math, &math) {
                // Preserve existing bin_arrays (they come from separate accounts, not LbPair)
                let bin_arrays = if new_ba.is_empty() { existing_ba.clone() } else { new_ba.clone() };

                // Check if active_id moved outside the loaded bin array range
                if !bin_arrays.is_empty() && *new_active_id != *old_active_id {
                    let new_ba_index = super::decoder::meteora_dlmm::bin_id_to_bin_array_index(*new_active_id);
                    let covered = bin_arrays.iter().any(|ba| ba.index == new_ba_index);
                    if !covered {
                        needs_tick_reload = true; // reuse flag for bin array reload
                    }
                }

                PoolMath::MeteoraDlmm {
                    active_id: *new_active_id,
                    bin_step: *bin_step,
                    base_factor: *base_factor,
                    variable_fee_control: *variable_fee_control,
                    max_volatility_accumulator: *max_volatility_accumulator,
                    volatility_accumulator: *volatility_accumulator,
                    volatility_reference: *volatility_reference,
                    index_reference: *index_reference,
                    bin_arrays,
                }
            } else {
                math
            };

            // Queue tick/bin array reload if needed
            if needs_tick_reload {
                if let PoolMath::Concentrated { tick_current, tick_spacing, .. } = &math {
                    log::debug!("CLMM {} tick moved out of range (tick={}), queuing tick array reload",
                        address, tick_current);
                    if let Ok(mut queue) = self.tick_reload_queue.try_lock() {
                        if !queue.iter().any(|r| r.pool_address == *address) {
                            queue.push(TickArrayReloadRequest {
                                pool_address: *address,
                                tick_current: *tick_current,
                                tick_spacing: *tick_spacing,
                            });
                            self.tick_reload_notify.notify_one();
                        }
                    }
                }
                if let PoolMath::MeteoraDlmm { active_id, .. } = &math {
                    log::debug!("DLMM {} active_id moved out of range (id={}), queuing bin array reload",
                        address, active_id);
                    if let Ok(mut queue) = self.bin_array_reload_queue.try_lock() {
                        if !queue.iter().any(|r| r.pool_address == *address) {
                            queue.push(BinArrayReloadRequest {
                                pool_address: *address,
                                active_id: *active_id,
                            });
                            self.tick_reload_notify.notify_one();
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
            // Update reserves for CP pools (vault balance = reserve)
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
            }

            // Always update slot and emit PoolUpdate (for all DEX types)
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

    /// Re-emit a pool's current state to the engine channel.
    pub fn re_emit_pool(&self, pool: &PoolState) {
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
                sqrt_price_x64, liquidity, tick_current, fee_rate, ref tick_arrays,
                ref mut limit_in_a, ref mut limit_in_b, ..
            } = pool.math {
                let (la, lb) = super::state::compute_clmm_limits_with_fee(
                    sqrt_price_x64, liquidity, tick_current, tick_arrays, fee_rate,
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

    /// Drain pending bin array reload requests.
    pub async fn drain_bin_array_reload_requests(&self) -> Vec<BinArrayReloadRequest> {
        let mut queue = self.bin_array_reload_queue.lock().await;
        std::mem::take(&mut *queue)
    }

    /// Update a single bin array in a DLMM pool and emit a PoolUpdate.
    pub fn update_bin_array(
        &self,
        bin_array_address: &Pubkey,
        new_bin_array: DlmmBinArray,
        slot: u64,
    ) {
        let pool_address = match self.pool_by_tick_array(bin_array_address) {
            Some(addr) => addr,
            None => return,
        };

        if let Some(mut pool) = self.pools.get_mut(&pool_address) {
            if let PoolMath::MeteoraDlmm {
                ref mut bin_arrays,
                ..
            } = pool.math
            {
                // Replace bin array with matching index, or append
                let idx = new_bin_array.index;
                if let Some(existing) = bin_arrays.iter_mut().find(|ba| ba.index == idx) {
                    *existing = new_bin_array;
                } else {
                    bin_arrays.push(new_bin_array);
                }
            }

            if matches!(pool.math, PoolMath::MeteoraDlmm { .. }) {
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

    /// Replace all bin arrays for a DLMM pool and emit a PoolUpdate.
    /// `new_extra_pdas` = [oracle, existing_bin_array_pda_1, ...] — only includes PDAs that exist on-chain.
    pub fn replace_bin_arrays(&self, address: &Pubkey, new_bin_arrays: Vec<DlmmBinArray>, new_extra_pdas: Vec<Pubkey>, slot: u64) {
        if let Some(mut pool) = self.pools.get_mut(address) {
            if let PoolMath::MeteoraDlmm { ref mut bin_arrays, .. } = pool.math {
                *bin_arrays = new_bin_arrays;
            }

            if matches!(pool.math, PoolMath::MeteoraDlmm { .. }) {
                // Update extra_accounts with only existing bin array PDAs
                pool.extra_accounts = new_extra_pdas;
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

    /// Replace tick arrays for a CLMM pool and emit a PoolUpdate.
    pub fn replace_tick_arrays(&self, address: &Pubkey, new_tick_arrays: Vec<TickArray>, slot: u64) {
        if let Some(mut pool) = self.pools.get_mut(address) {
            if let PoolMath::Concentrated { ref mut tick_arrays, .. } = pool.math {
                *tick_arrays = new_tick_arrays;
            }
            // Recompute limits after replacing tick arrays
            if let PoolMath::Concentrated {
                sqrt_price_x64, liquidity, tick_current, fee_rate, ref tick_arrays,
                ref mut limit_in_a, ref mut limit_in_b, ..
            } = pool.math {
                let (la, lb) = super::state::compute_clmm_limits_with_fee(
                    sqrt_price_x64, liquidity, tick_current, tick_arrays, fee_rate,
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
