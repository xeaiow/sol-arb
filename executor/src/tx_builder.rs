use std::sync::Arc;

use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{self, VersionedMessage, AddressLookupTableAccount},
    pubkey::Pubkey,
    signer::{keypair::Keypair, Signer},
    transaction::VersionedTransaction,
};
use solana_system_interface::instruction as system_instruction;

use arb_engine::opportunity::{Opportunity, PoolSnapshot};
use solana_streamer_sdk::pool::state::DexType;

use crate::alt::Tier0Alt;
use crate::anti_fp;
use crate::config::ExecutorConfigFile;
use crate::marginfi::MarginFiState;

// ── DEX Program IDs ──
const RAYDIUM_AMM_V4_PROGRAM: Pubkey = solana_sdk::pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
const RAYDIUM_CPMM_PROGRAM: Pubkey = solana_sdk::pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
const RAYDIUM_CLMM_PROGRAM: Pubkey = solana_sdk::pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
const PUMPFUN_PROGRAM: Pubkey = solana_sdk::pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");
const PUMPSWAP_PROGRAM: Pubkey = solana_sdk::pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
const BONKSWAP_PROGRAM: Pubkey = solana_sdk::pubkey!("BSwp6bEBihVLdqJRKGgzjcGLHkcTuzmSo1TQkHepzH8p");

// ── DEX Global PDAs / Constants ──
const RAYDIUM_AMM_AUTHORITY: Pubkey = solana_sdk::pubkey!("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1");
const SYSVAR_RENT: Pubkey = solana_sdk::pubkey!("SysvarRent111111111111111111111111111111111");
/// WSOL mint
// ── PumpFun constants ──
/// PumpFun global account (singleton, well-known)
const PUMPFUN_GLOBAL: Pubkey = solana_sdk::pubkey!("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf");
/// PumpFun fee recipient (from global account)
const PUMPFUN_FEE_RECIPIENT: Pubkey = solana_sdk::pubkey!("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM");
/// PumpFun fee program
const PUMPFUN_FEE_PROGRAM: Pubkey = solana_sdk::pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");

// ── PumpSwap constants ──
/// PumpSwap global config (singleton, well-known)
const PUMPSWAP_GLOBAL_CONFIG: Pubkey = solana_sdk::pubkey!("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw");
/// PumpSwap protocol fee recipient
const PUMPSWAP_PROTOCOL_FEE_RECIPIENT: Pubkey = solana_sdk::pubkey!("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV");
/// PumpSwap fee program (same as PumpFun)
const PUMPSWAP_FEE_PROGRAM: Pubkey = solana_sdk::pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");

// ── Meteora DAMM V2 constants ──
const METEORA_DAMM_V2_PROGRAM: Pubkey = solana_sdk::pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");

// ── Meteora DLMM constants ──
const METEORA_DLMM_PROGRAM: Pubkey = solana_sdk::pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");

// ── Orca Whirlpool constants ──
const ORCA_WHIRLPOOL_PROGRAM: Pubkey = solana_sdk::pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");

/// SPL Memo program v2 (required by DLMM swap2 and Whirlpool swap_v2)
const SPL_MEMO_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

/// SPL Token program ID
const SPL_TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Token-2022 program ID
const TOKEN_2022_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
/// Associated Token Account program ID
const ATA_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// System program ID
const SYSTEM_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("11111111111111111111111111111111");

/// Astralane tip account
const ASTRALANE_TIP_ACCOUNT: Pubkey = solana_sdk::pubkey!("astra4uejePWneqNaJKuFFA8oonqCE1sqF6b45kDMZm");

/// Derive an Associated Token Account address (PDA)
fn derive_ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    derive_ata_with_program(owner, mint, &SPL_TOKEN_PROGRAM_ID)
}

/// Derive ATA with explicit token program (SPL Token or Token-2022)
fn derive_ata_with_program(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

/// Built transaction pair (VersionedTransaction v0 with ALT)
pub struct TxPair {
    pub jito_tx: Option<VersionedTransaction>,  // Variant A: with tip, no CU price
    pub swqos_tx: Option<VersionedTransaction>, // Variant B: with CU price, no tip
}

pub struct TxBuilder {
    program_id: Pubkey,
    payer_pubkey: Pubkey,
    fee_collectors: Vec<Pubkey>,
    cu_jitter_range: u32,
    jito_tip_percentage: u32,
    jito_min_tip: u64,
    jito_min_operator_profit: u64,
    swqos_priority_fee: u64,
    jito_enabled: bool,
    swqos_enabled: bool,
    alt: Option<Arc<Tier0Alt>>,
    marginfi_state: Option<Arc<MarginFiState>>,
    /// Test mode: skip profit verification on-chain (discriminator +3)
    pub test_mode: bool,
}

impl TxBuilder {
    pub fn from_config(config: &ExecutorConfigFile, payer_pubkey: Pubkey) -> Self {
        let fee_collectors: Vec<Pubkey> = config
            .executor
            .anti_fingerprint
            .fee_collectors_sol
            .iter()
            .filter_map(|s| s.parse().ok())
            .collect();

        let jito = config.jito.as_ref();
        let jito_enabled = jito.map_or(false, |j| j.enabled);

        let flashblock_enabled = config.flashblock.as_ref().map_or(false, |f| f.enabled);
        let astralane_enabled = config.astralane.as_ref().map_or(false, |a| a.enabled);

        // Fixed priority fee: take from flashblock or astralane config, default 100k lamports (0.0001 SOL)
        let swqos_fee = config
            .flashblock
            .as_ref()
            .and_then(|f| f.priority_fee_lamports)
            .or_else(|| config.astralane.as_ref().and_then(|a| a.priority_fee_lamports))
            .unwrap_or(100_000);

        Self {
            program_id: config.executor.program_id.parse()
                .expect("Invalid program_id in config"),
            payer_pubkey,
            fee_collectors,
            cu_jitter_range: config.executor.anti_fingerprint.cu_jitter_range,
            jito_tip_percentage: jito.map_or(60, |j| j.tip_percentage),
            jito_min_tip: jito.map_or(1000, |j| j.min_tip_lamports),
            jito_min_operator_profit: jito.map_or(5000, |j| j.min_operator_profit_lamports),
            swqos_priority_fee: swqos_fee,
            jito_enabled,
            swqos_enabled: flashblock_enabled || astralane_enabled,
            alt: None,
            marginfi_state: None,
            test_mode: false,
        }
    }

    /// Set the MarginFi state for flashloan wrapping
    pub fn set_marginfi(&mut self, state: Arc<MarginFiState>) {
        self.marginfi_state = Some(state);
    }

    /// Set the pre-loaded ALT for VersionedTransaction compression
    pub fn set_alt(&mut self, alt: Arc<Tier0Alt>) {
        self.alt = Some(alt);
    }

    /// Build two transaction variants from an Opportunity.
    /// Uses rayon::join for parallel ed25519 signing.
    pub fn build(
        &self,
        opp: &Opportunity,
        payer: &Keypair,
        recent_blockhash: Hash,
    ) -> TxPair {
        let hop_count = opp.route.hops.len();
        debug_assert!((2..=4).contains(&hop_count), "hop_count must be 2-4, got {}", hop_count);

        let dex_types: Vec<u8> = opp
            .pool_snapshots
            .iter()
            .map(|s| s.dex_type as u8)
            .collect();
        let mut base_cu = anti_fp::estimate_cu(&dex_types);
        if self.marginfi_state.is_some() {
            base_cu += anti_fp::FLASHLOAN_CU_OVERHEAD;
        }
        let cu_limit = anti_fp::jittered_cu(base_cu, self.cu_jitter_range);

        let arb_ix = self.build_arb_instruction(opp);

        // If accounts are empty (missing required data), return empty TxPair
        if arb_ix.accounts.is_empty() {
            log::warn!("Skipping opportunity: missing required accounts");
            return TxPair { jito_tx: None, swqos_tx: None };
        }

        // Create ATA instructions for base mint + all intermediate mints
        let create_ata_ixs = self.build_create_ata_ixs(opp);

        // Build flashloan instructions if enabled
        let flashloan_ixs = self.build_flashloan_ixs(opp, &create_ata_ixs);

        let lookup_tables: Vec<AddressLookupTableAccount> = self
            .alt
            .as_ref()
            .map(|alt| vec![alt.account.clone()])
            .unwrap_or_default();

        // Prepare instruction sets for both variants
        let jito_ixs = if self.jito_enabled {
            let tip = self.calculate_jito_tip(opp.expected_profit);
            tip.map(|tip| {
                let tip_account = anti_fp::random_tip_account();
                let mut ixs = vec![
                    ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                ];
                ixs.extend(create_ata_ixs.clone());
                if let Some(ref fl) = flashloan_ixs {
                    ixs.push(fl.start_jito.clone());
                    ixs.push(fl.borrow.clone());
                }
                ixs.push(arb_ix.clone());
                if let Some(ref fl) = flashloan_ixs {
                    ixs.push(fl.repay.clone());
                    ixs.push(fl.end.clone());
                }
                ixs.push(system_instruction::transfer(&payer.pubkey(), &tip_account, tip));
                ixs
            })
        } else {
            None
        };

        let swqos_ixs = if self.swqos_enabled {
            let cu_price = self.calculate_cu_price(opp.expected_profit, base_cu);
            let total_priority_fee = cu_price * cu_limit as u64 / 1_000_000;
            log::debug!(
                "SwQoS tx: expected_profit={:.6} SOL, priority_fee={:.6} SOL, cu_price={}, cu_limit={}",
                opp.expected_profit as f64 / 1e9,
                total_priority_fee as f64 / 1e9,
                cu_price, cu_limit,
            );
            // Astralane requires a SOL transfer tip (min 10000 lamports = 0.00001 SOL)
            let astralane_tip: u64 = 10_000;
            let mut ixs = vec![
                ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                ComputeBudgetInstruction::set_compute_unit_price(cu_price),
            ];
            ixs.extend(create_ata_ixs);
            if let Some(ref fl) = flashloan_ixs {
                ixs.push(fl.start.clone());
                ixs.push(fl.borrow.clone());
            }
            ixs.push(arb_ix);
            if let Some(ref fl) = flashloan_ixs {
                ixs.push(fl.repay.clone());
                ixs.push(fl.end.clone());
            }
            ixs.push(system_instruction::transfer(&payer.pubkey(), &ASTRALANE_TIP_ACCOUNT, astralane_tip));
            Some(ixs)
        } else {
            None
        };

        // Parallel signing with rayon::join
        let payer_pubkey = payer.pubkey();
        let (jito_tx, swqos_tx) = rayon::join(
            || {
                jito_ixs.as_ref().and_then(|ixs| {
                    match message::v0::Message::try_compile(
                        &payer_pubkey,
                        ixs,
                        &lookup_tables,
                        recent_blockhash,
                    ) {
                        Ok(msg) => VersionedTransaction::try_new(VersionedMessage::V0(msg), &[payer])
                            .map_err(|e| log::warn!("Jito tx sign failed: {}", e))
                            .ok(),
                        Err(e) => {
                            log::warn!("Jito tx compile failed: {}", e);
                            None
                        }
                    }
                })
            },
            || {
                swqos_ixs.as_ref().and_then(|ixs| {
                    match message::v0::Message::try_compile(
                        &payer_pubkey,
                        ixs,
                        &lookup_tables,
                        recent_blockhash,
                    ) {
                        Ok(msg) => VersionedTransaction::try_new(VersionedMessage::V0(msg), &[payer])
                            .map_err(|e| log::warn!("SwQoS tx sign failed: {}", e))
                            .ok(),
                        Err(e) => {
                            log::warn!("SwQoS tx compile failed: {}", e);
                            None
                        }
                    }
                })
            },
        );

        TxPair { jito_tx, swqos_tx }
    }

    fn build_arb_instruction(&self, opp: &Opportunity) -> Instruction {
        let hop_count = opp.route.hops.len() as u8;
        let data = self.encode_instruction_data(opp, hop_count);
        let accounts = self.build_account_metas(opp);

        Instruction {
            program_id: self.program_id,
            accounts,
            data,
        }
    }

    /// Calculate extra accounts for each hop.
    /// CLMM: unique tick arrays beyond the first (base includes 1 tick array in 10 accounts).
    /// DLMM: bin arrays as remaining accounts (all are extra, base is 16 fixed accounts).
    fn hop_extra_accounts(snap: &PoolSnapshot) -> u8 {
        let extra = if snap.accounts.len() > 3 { &snap.accounts[3..] } else { return 0 };

        match snap.dex_type {
            DexType::RaydiumClmm => {
                // extra[0]=amm_config, extra[1]=observation_key, extra[2..]=tick_arrays
                let tick_arrays: Vec<&Pubkey> = if extra.len() > 2 {
                    extra[2..].iter().collect()
                } else {
                    return 0;
                };
                if tick_arrays.is_empty() {
                    return 0;
                }
                let mut unique = vec![tick_arrays[0]];
                for ta in tick_arrays.iter().skip(1) {
                    if !unique.contains(ta) {
                        unique.push(ta);
                    }
                    if unique.len() >= 3 {
                        break;
                    }
                }
                // extra_accounts = unique count - 1 (first tick array is in base 10 accounts)
                (unique.len() - 1) as u8
            }
            DexType::MeteoraDlmm => {
                // extra[0]=oracle, extra[1..]=bin_arrays (all bin arrays are extra)
                if extra.len() > 1 {
                    (extra.len() - 1).min(3) as u8
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    fn encode_instruction_data(&self, opp: &Opportunity, hop_count: u8) -> Vec<u8> {
        let mut data = Vec::with_capacity(20);

        // Discriminator: 0=2hop, 1=3hop, 2=4hop; test mode: 3=2hop, 4=3hop, 5=4hop
        let disc = if self.test_mode { hop_count - 2 + 3 } else { hop_count - 2 };
        data.push(disc);

        // First (buy) and last (sell) DEX types
        data.push(opp.pool_snapshots[0].dex_type as u8);
        data.push(opp.pool_snapshots[hop_count as usize - 1].dex_type as u8);

        // Flags byte:
        //   bit0 = buy_a_to_b
        //   bit1 = sell_a_to_b
        //   bit2 = buy_2022
        //   bit3 = sell_2022
        //   bit4-5 = buy extra_accounts (0-3, CLMM extra tick arrays)
        //   bit6-7 = sell extra_accounts (0-3)
        let mut flags: u8 = 0;
        if opp.pool_snapshots[0].is_a_to_b {
            flags |= 1;
        }
        if opp.pool_snapshots[hop_count as usize - 1].is_a_to_b {
            flags |= 1 << 1;
        }
        let buy = &opp.pool_snapshots[0];
        if buy.mint_a_is_2022 || buy.mint_b_is_2022 {
            flags |= 1 << 2;
        }
        let sell = &opp.pool_snapshots[hop_count as usize - 1];
        if sell.mint_a_is_2022 || sell.mint_b_is_2022 {
            flags |= 1 << 3;
        }
        // Encode extra_accounts for buy (bit4-5) and sell (bit6-7)
        flags |= (Self::hop_extra_accounts(&opp.pool_snapshots[0]) & 3) << 4;
        flags |= (Self::hop_extra_accounts(&opp.pool_snapshots[hop_count as usize - 1]) & 3) << 6;
        data.push(flags);

        // Middle hops (3-hop and 4-hop)
        for i in 1..hop_count as usize - 1 {
            data.push(opp.pool_snapshots[i].dex_type as u8);
            let mut mid_flags: u8 = 0;
            if opp.pool_snapshots[i].is_a_to_b {
                mid_flags |= 1;
            }
            let mid = &opp.pool_snapshots[i];
            if mid.mint_a_is_2022 || mid.mint_b_is_2022 {
                mid_flags |= 1 << 1;
            }
            // Encode extra_accounts for mid hop (bit2-3)
            mid_flags |= (Self::hop_extra_accounts(&opp.pool_snapshots[i]) & 3) << 2;
            data.push(mid_flags);
        }

        // amount_in (u64 LE)
        data.extend_from_slice(&opp.amount_in.to_le_bytes());

        // min_profit (u32 LE) — use 80% of expected as safety margin
        let min_profit = (opp.expected_profit * 80 / 100) as u32;
        data.extend_from_slice(&min_profit.to_le_bytes());

        data
    }

    fn build_account_metas(&self, opp: &Opportunity) -> Vec<AccountMeta> {
        let hop_count = opp.route.hops.len();
        let mut metas = Vec::new();

        // Fixed header (8 accounts)
        metas.push(AccountMeta::new(self.payer_pubkey, true));                 // [0] Payer (signer)
        metas.push(AccountMeta::new_readonly(opp.route.base_mint, false));     // [1] Base mint
        let user_base_ata = derive_ata(&self.payer_pubkey, &opp.route.base_mint);
        metas.push(AccountMeta::new(user_base_ata, false));                    // [2] User base ATA
        let fee_collector = anti_fp::random_fee_collector(&self.fee_collectors);
        metas.push(AccountMeta::new(fee_collector, false));                    // [3] Fee collector
        metas.push(AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false));    // [4] SPL Token
        metas.push(AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false));   // [5] Token-2022
        metas.push(AccountMeta::new_readonly(ATA_PROGRAM_ID, false));          // [6] ATA Program
        metas.push(AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false));       // [7] System

        // Intermediate token accounts: 3 × (hop_count - 1)
        for i in 1..hop_count {
            let prev_snapshot = &opp.pool_snapshots[i - 1];
            let (intermediate_mint, is_2022) = if prev_snapshot.is_a_to_b {
                (prev_snapshot.mint_b, prev_snapshot.mint_b_is_2022)
            } else {
                (prev_snapshot.mint_a, prev_snapshot.mint_a_is_2022)
            };
            let token_program = if is_2022 {
                TOKEN_2022_PROGRAM_ID
            } else {
                SPL_TOKEN_PROGRAM_ID
            };
            metas.push(AccountMeta::new_readonly(intermediate_mint, false));   // mint
            metas.push(AccountMeta::new_readonly(token_program, false));       // token program
            let intermediate_ata = derive_ata_with_program(&self.payer_pubkey, &intermediate_mint, &token_program);
            metas.push(AccountMeta::new(intermediate_ata, false));             // user ATA
        }

        // Per-hop pool accounts — assembled per DEX type with correct ordering.
        // PoolSnapshot.accounts layout: [pool_address, vault_a?, vault_b?, extra...]
        for snapshot in &opp.pool_snapshots {
            let hop_metas = self.build_hop_accounts(snapshot);
            if hop_metas.is_empty() {
                // Missing required accounts (e.g. CLMM tick_array) — abort
                return vec![];
            }
            metas.extend(hop_metas);
        }

        // Append DEX program IDs needed for CPI (must be in transaction accounts).
        // These are outside the per-hop slice but required for invoke_signed_with_slice.
        // Use a set to avoid duplicates (e.g. two CLMM hops need only one program ID).
        let mut dex_programs: Vec<Pubkey> = Vec::new();
        for snapshot in &opp.pool_snapshots {
            let prog = match snapshot.dex_type {
                DexType::RaydiumAmmV4 => RAYDIUM_AMM_V4_PROGRAM,
                DexType::RaydiumCpmm => RAYDIUM_CPMM_PROGRAM,
                DexType::RaydiumClmm => RAYDIUM_CLMM_PROGRAM,
                DexType::PumpFun => PUMPFUN_PROGRAM,
                DexType::PumpSwap => PUMPSWAP_PROGRAM,
                DexType::Bonk => BONKSWAP_PROGRAM,
                DexType::MeteoraDammV2 => METEORA_DAMM_V2_PROGRAM,
                DexType::MeteoraDlmm => METEORA_DLMM_PROGRAM,
                DexType::OrcaWhirlpool => ORCA_WHIRLPOOL_PROGRAM,
            };
            if !dex_programs.contains(&prog) {
                dex_programs.push(prog);
            }
        }
        for prog in dex_programs {
            metas.push(AccountMeta::new_readonly(prog, false));
        }

        metas
    }

    /// Build the per-hop CPI account list for a single pool snapshot.
    /// Must match the exact account ordering in program/src/swap.rs for each DEX.
    /// PoolSnapshot.accounts: [pool_address, vault_a?, vault_b?, extra...]
    fn build_hop_accounts(&self, snap: &PoolSnapshot) -> Vec<AccountMeta> {
        let pool = snap.accounts.get(0).copied().unwrap_or_default();
        let vault_a = snap.accounts.get(1).copied();
        let vault_b = snap.accounts.get(2).copied();
        let extra = if snap.accounts.len() > 3 { &snap.accounts[3..] } else { &[] };

        // Determine user ATAs for input/output based on direction
        let (input_mint, output_mint, input_is_2022, output_is_2022) = if snap.is_a_to_b {
            (snap.mint_a, snap.mint_b, snap.mint_a_is_2022, snap.mint_b_is_2022)
        } else {
            (snap.mint_b, snap.mint_a, snap.mint_b_is_2022, snap.mint_a_is_2022)
        };
        let input_token_prog = if input_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
        let output_token_prog = if output_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
        let user_input_ata = derive_ata_with_program(&self.payer_pubkey, &input_mint, &input_token_prog);
        let user_output_ata = derive_ata_with_program(&self.payer_pubkey, &output_mint, &output_token_prog);
        let (input_vault, output_vault) = if snap.is_a_to_b {
            (vault_a.unwrap_or_default(), vault_b.unwrap_or_default())
        } else {
            (vault_b.unwrap_or_default(), vault_a.unwrap_or_default())
        };

        match snap.dex_type {
            // Raydium AMM V4: 9 accounts (8 for CPI + program ID)
            // swap.rs: [token_program, amm, amm_authority, coin_vault, pc_vault,
            //           user_source, user_dest, user_owner, program]
            DexType::RaydiumAmmV4 => vec![
                AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
                AccountMeta::new(pool, false),
                AccountMeta::new_readonly(RAYDIUM_AMM_AUTHORITY, false),
                AccountMeta::new(vault_a.unwrap_or_default(), false),
                AccountMeta::new(vault_b.unwrap_or_default(), false),
                AccountMeta::new(user_input_ata, false),
                AccountMeta::new(user_output_ata, false),
                AccountMeta::new_readonly(self.payer_pubkey, true),
                AccountMeta::new_readonly(RAYDIUM_AMM_V4_PROGRAM, false),
            ],

            // Raydium CPMM: 13 accounts
            // extra[0]=amm_config, extra[1]=observation_key
            // swap.rs: [payer, authority, amm_config, pool_state, input_ata, output_ata,
            //  input_vault, output_vault, input_token_program, output_token_program,
            //  input_mint, output_mint, observation_state]
            DexType::RaydiumCpmm => {
                let amm_config = extra.first().copied().unwrap_or_default();
                let observation = extra.get(1).copied().unwrap_or_default();
                let (authority, _) = Pubkey::find_program_address(
                    &[b"vault_and_lp_mint_auth_seed", pool.as_ref()],
                    &RAYDIUM_CPMM_PROGRAM,
                );
                let input_token_prog = if snap.is_a_to_b && snap.mint_a_is_2022
                    || !snap.is_a_to_b && snap.mint_b_is_2022 {
                    TOKEN_2022_PROGRAM_ID
                } else {
                    SPL_TOKEN_PROGRAM_ID
                };
                let output_token_prog = if snap.is_a_to_b && snap.mint_b_is_2022
                    || !snap.is_a_to_b && snap.mint_a_is_2022 {
                    TOKEN_2022_PROGRAM_ID
                } else {
                    SPL_TOKEN_PROGRAM_ID
                };
                vec![
                    AccountMeta::new(self.payer_pubkey, true),
                    AccountMeta::new_readonly(authority, false),
                    AccountMeta::new_readonly(amm_config, false),
                    AccountMeta::new(pool, false),
                    AccountMeta::new(user_input_ata, false),
                    AccountMeta::new(user_output_ata, false),
                    AccountMeta::new(input_vault, false),
                    AccountMeta::new(output_vault, false),
                    AccountMeta::new_readonly(input_token_prog, false),
                    AccountMeta::new_readonly(output_token_prog, false),
                    AccountMeta::new_readonly(input_mint, false),
                    AccountMeta::new_readonly(output_mint, false),
                    AccountMeta::new(observation, false),
                    AccountMeta::new_readonly(RAYDIUM_CPMM_PROGRAM, false),
                ]
            }

            // Raydium CLMM: 10 base + 0-2 unique extra tick arrays + program ID
            // extra[0]=amm_config, extra[1]=observation_key, extra[2..]=tick_arrays
            // On-chain layout: [payer, amm_config, pool_state, input_ata, output_ata,
            //  input_vault, output_vault, observation, token_program, tick_array0, ...]
            // base_pool_account_count = 10, extra_accounts = unique extra tick arrays (0-2)
            // CLMM program ID appended at end (needed for CPI but not counted in offset calc)
            DexType::RaydiumClmm => {
                let amm_config = extra.first().copied().unwrap_or_default();
                let observation = extra.get(1).copied().unwrap_or_default();
                let tick_arrays: Vec<Pubkey> = if extra.len() > 2 {
                    extra[2..].to_vec()
                } else {
                    vec![]
                };
                if tick_arrays.is_empty() || tick_arrays[0] == Pubkey::default() {
                    log::warn!("CLMM tick_array missing for pool {}, skipping hop", pool);
                    return vec![];
                }
                // Deduplicate tick arrays — only pass unique ones to avoid AccountBorrowFailed
                let mut unique_tas = vec![tick_arrays[0]];
                for ta in tick_arrays.iter().skip(1) {
                    if !unique_tas.contains(ta) {
                        unique_tas.push(*ta);
                    }
                    if unique_tas.len() >= 3 {
                        break;
                    }
                }
                let mut metas = vec![
                    AccountMeta::new(self.payer_pubkey, true),
                    AccountMeta::new_readonly(amm_config, false),
                    AccountMeta::new(pool, false),
                    AccountMeta::new(user_input_ata, false),
                    AccountMeta::new(user_output_ata, false),
                    AccountMeta::new(input_vault, false),
                    AccountMeta::new(output_vault, false),
                    AccountMeta::new(observation, false),
                    AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
                    AccountMeta::new(unique_tas[0], false),
                ];
                for ta in unique_tas.iter().skip(1) {
                    metas.push(AccountMeta::new(*ta, false));
                }
                metas
            }

            // PumpFun: 16 accounts (buy layout, sell uses 14 but on-chain slices)
            // extra[0]=creator
            // swap.rs: [global, fee_recipient, mint, bonding_curve, associated_bonding_curve,
            //  associated_user, user, system, token_program, creator_vault, event_authority,
            //  program, global_volume_accumulator, user_volume_accumulator, fee_config, fee_program]
            DexType::PumpFun => {
                let creator = extra.first().copied().unwrap_or_default();
                let token_mint = snap.mint_a;
                let bonding_curve = pool; // pool address IS the bonding curve
                let associated_bonding_curve = derive_ata(&bonding_curve, &token_mint);
                let associated_user = derive_ata(&self.payer_pubkey, &token_mint);
                // creator_vault = creator (receives SOL directly)
                let creator_vault = creator;
                // event_authority PDA: ["__event_authority"]
                let (event_authority, _) = Pubkey::find_program_address(
                    &[b"__event_authority"],
                    &PUMPFUN_PROGRAM,
                );
                // global_volume_accumulator PDA: ["global_volume_accumulator"]
                let (global_volume_acc, _) = Pubkey::find_program_address(
                    &[b"global_volume_accumulator"],
                    &PUMPFUN_PROGRAM,
                );
                // user_volume_accumulator PDA: ["user_volume_accumulator", user]
                let (user_volume_acc, _) = Pubkey::find_program_address(
                    &[b"user_volume_accumulator", self.payer_pubkey.as_ref()],
                    &PUMPFUN_PROGRAM,
                );
                // fee_config PDA of fee program: ["fee_config", program_id]
                let (fee_config, _) = Pubkey::find_program_address(
                    &[b"fee_config", PUMPFUN_PROGRAM.as_ref()],
                    &PUMPFUN_FEE_PROGRAM,
                );
                vec![
                    AccountMeta::new_readonly(PUMPFUN_GLOBAL, false),       // [0] global
                    AccountMeta::new(PUMPFUN_FEE_RECIPIENT, false),         // [1] fee_recipient
                    AccountMeta::new_readonly(token_mint, false),            // [2] mint
                    AccountMeta::new(bonding_curve, false),                  // [3] bonding_curve
                    AccountMeta::new(associated_bonding_curve, false),       // [4] associated_bonding_curve
                    AccountMeta::new(associated_user, false),               // [5] associated_user
                    AccountMeta::new(self.payer_pubkey, true),              // [6] user
                    AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),    // [7] system_program
                    AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false), // [8] token_program
                    AccountMeta::new(creator_vault, false),                 // [9] creator_vault
                    AccountMeta::new_readonly(event_authority, false),      // [10] event_authority
                    AccountMeta::new_readonly(PUMPFUN_PROGRAM, false),      // [11] program
                    AccountMeta::new(global_volume_acc, false),             // [12] global_volume_accumulator
                    AccountMeta::new(user_volume_acc, false),               // [13] user_volume_accumulator
                    AccountMeta::new_readonly(fee_config, false),           // [14] fee_config
                    AccountMeta::new_readonly(PUMPFUN_FEE_PROGRAM, false),  // [15] fee_program
                ]
            }

            // PumpSwap: 23 accounts (buy layout, sell uses 21 but on-chain slices)
            // extra[0]=coin_creator
            // swap.rs: [pool, user, global_config, base_mint, quote_mint,
            //  user_base_ata, user_quote_ata, pool_base_vault, pool_quote_vault,
            //  protocol_fee_recipient, protocol_fee_recipient_ata,
            //  base_token_program, quote_token_program, system, ata_program,
            //  event_authority, program, coin_creator_vault_ata, coin_creator_vault_authority,
            //  global_volume_accumulator, user_volume_accumulator, fee_config, fee_program]
            DexType::PumpSwap => {
                let _coin_creator = extra.first().copied().unwrap_or_default();
                let base_mint = snap.mint_a;
                let quote_mint = snap.mint_b;
                let base_token_prog = if snap.mint_a_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
                let quote_token_prog = if snap.mint_b_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
                let user_base_ata = derive_ata_with_program(&self.payer_pubkey, &base_mint, &base_token_prog);
                let user_quote_ata = derive_ata_with_program(&self.payer_pubkey, &quote_mint, &quote_token_prog);
                let pool_base_vault = vault_a.unwrap_or_default();
                let pool_quote_vault = vault_b.unwrap_or_default();
                let protocol_fee_recipient_ata = derive_ata_with_program(&PUMPSWAP_PROTOCOL_FEE_RECIPIENT, &quote_mint, &quote_token_prog);
                // event_authority PDA: ["__event_authority"]
                let (event_authority, _) = Pubkey::find_program_address(
                    &[b"__event_authority"],
                    &PUMPSWAP_PROGRAM,
                );
                // coin_creator_vault_authority PDA: ["coin_creator_vault_authority", pool]
                let (coin_creator_vault_authority, _) = Pubkey::find_program_address(
                    &[b"coin_creator_vault_authority", pool.as_ref()],
                    &PUMPSWAP_PROGRAM,
                );
                // coin_creator_vault_ata = ATA(coin_creator_vault_authority, quote_mint)
                let coin_creator_vault_ata = derive_ata_with_program(&coin_creator_vault_authority, &quote_mint, &quote_token_prog);
                // global_volume_accumulator PDA
                let (global_volume_acc, _) = Pubkey::find_program_address(
                    &[b"global_volume_accumulator"],
                    &PUMPSWAP_PROGRAM,
                );
                // user_volume_accumulator PDA
                let (user_volume_acc, _) = Pubkey::find_program_address(
                    &[b"user_volume_accumulator", self.payer_pubkey.as_ref()],
                    &PUMPSWAP_PROGRAM,
                );
                // fee_config PDA: ["fee_config", program_id]
                let (fee_config, _) = Pubkey::find_program_address(
                    &[b"fee_config", PUMPSWAP_PROGRAM.as_ref()],
                    &PUMPSWAP_FEE_PROGRAM,
                );
                vec![
                    AccountMeta::new(pool, false),                                  // [0] pool
                    AccountMeta::new(self.payer_pubkey, true),                      // [1] user
                    AccountMeta::new_readonly(PUMPSWAP_GLOBAL_CONFIG, false),       // [2] global_config
                    AccountMeta::new_readonly(base_mint, false),                    // [3] base_mint
                    AccountMeta::new_readonly(quote_mint, false),                   // [4] quote_mint
                    AccountMeta::new(user_base_ata, false),                         // [5] user_base_token_account
                    AccountMeta::new(user_quote_ata, false),                        // [6] user_quote_token_account
                    AccountMeta::new(pool_base_vault, false),                       // [7] pool_base_token_account
                    AccountMeta::new(pool_quote_vault, false),                      // [8] pool_quote_token_account
                    AccountMeta::new(PUMPSWAP_PROTOCOL_FEE_RECIPIENT, false),       // [9] protocol_fee_recipient
                    AccountMeta::new(protocol_fee_recipient_ata, false),            // [10] protocol_fee_recipient_ata
                    AccountMeta::new_readonly(base_token_prog, false),              // [11] base_token_program
                    AccountMeta::new_readonly(quote_token_prog, false),             // [12] quote_token_program
                    AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),            // [13] system_program
                    AccountMeta::new_readonly(ATA_PROGRAM_ID, false),               // [14] associated_token_program
                    AccountMeta::new_readonly(event_authority, false),              // [15] event_authority
                    AccountMeta::new_readonly(PUMPSWAP_PROGRAM, false),             // [16] program
                    AccountMeta::new(coin_creator_vault_ata, false),                // [17] coin_creator_vault_ata
                    AccountMeta::new_readonly(coin_creator_vault_authority, false), // [18] coin_creator_vault_authority
                    AccountMeta::new(global_volume_acc, false),                     // [19] global_volume_accumulator
                    AccountMeta::new(user_volume_acc, false),                       // [20] user_volume_accumulator
                    AccountMeta::new_readonly(fee_config, false),                   // [21] fee_config
                    AccountMeta::new_readonly(PUMPSWAP_FEE_PROGRAM, false),         // [22] fee_program
                ]
            }

            // Bonkswap: 17 accounts
            // extra[0]=global_config (state), extra[1]=platform_config
            // swap.rs: [state, pool, token_x, token_y, pool_x_account, pool_y_account,
            //  swapper_x_account, swapper_y_account, swapper, referrer_x_account,
            //  referrer_y_account, referrer, program_authority, system, token_program,
            //  associated_token_program, rent]
            DexType::Bonk => {
                let global_config = extra.first().copied().unwrap_or_default();
                let _platform_config = extra.get(1).copied().unwrap_or_default();
                let token_x = snap.mint_a;
                let token_y = snap.mint_b;
                let pool_x_account = vault_a.unwrap_or_default();
                let pool_y_account = vault_b.unwrap_or_default();
                let swapper_x = derive_ata(&self.payer_pubkey, &token_x);
                let swapper_y = derive_ata(&self.payer_pubkey, &token_y);
                // program_authority PDA: ["authority"]
                let (program_authority, _) = Pubkey::find_program_address(
                    &[b"authority"],
                    &BONKSWAP_PROGRAM,
                );
                // referrer = payer (self-referral, no-op fees)
                let referrer = self.payer_pubkey;
                let referrer_x = derive_ata(&referrer, &token_x);
                let referrer_y = derive_ata(&referrer, &token_y);
                vec![
                    AccountMeta::new_readonly(global_config, false),        // [0] state
                    AccountMeta::new(pool, false),                          // [1] pool
                    AccountMeta::new_readonly(token_x, false),              // [2] token_x
                    AccountMeta::new_readonly(token_y, false),              // [3] token_y
                    AccountMeta::new(pool_x_account, false),                // [4] pool_x_account
                    AccountMeta::new(pool_y_account, false),                // [5] pool_y_account
                    AccountMeta::new(swapper_x, false),                     // [6] swapper_x_account
                    AccountMeta::new(swapper_y, false),                     // [7] swapper_y_account
                    AccountMeta::new(self.payer_pubkey, true),              // [8] swapper
                    AccountMeta::new(referrer_x, false),                    // [9] referrer_x_account
                    AccountMeta::new(referrer_y, false),                    // [10] referrer_y_account
                    AccountMeta::new_readonly(referrer, false),             // [11] referrer
                    AccountMeta::new_readonly(program_authority, false),    // [12] program_authority
                    AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),    // [13] system_program
                    AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false), // [14] token_program
                    AccountMeta::new_readonly(ATA_PROGRAM_ID, false),       // [15] associated_token_program
                    AccountMeta::new_readonly(SYSVAR_RENT, false),          // [16] rent
                ]
            }

            // Meteora DAMM V2: 14 accounts
            // swap.rs: [pool_authority, pool, input_token_account, output_token_account,
            //  token_a_vault, token_b_vault, token_a_mint, token_b_mint, payer,
            //  token_a_program, token_b_program, referral_token_account,
            //  event_authority, program]
            DexType::MeteoraDammV2 => {
                // pool_authority is a fixed global address (from IDL: address constraint)
                let pool_authority: Pubkey = solana_sdk::pubkey!("HLnpSz9h2S4hiLQ43rnSD9XkcUThA7B8hQMKmDaiTLcC");
                let token_a_prog = if snap.mint_a_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
                let token_b_prog = if snap.mint_b_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
                // event_authority PDA
                let (event_authority, _) = Pubkey::find_program_address(
                    &[b"__event_authority"],
                    &METEORA_DAMM_V2_PROGRAM,
                );
                // referral_token_account: must match the output mint (fee is taken from output side)
                let referral_token_account = derive_ata_with_program(&self.payer_pubkey, &output_mint, &output_token_prog);
                vec![
                    AccountMeta::new_readonly(pool_authority, false),       // [0] pool_authority
                    AccountMeta::new(pool, false),                          // [1] pool
                    AccountMeta::new(user_input_ata, false),                // [2] input_token_account
                    AccountMeta::new(user_output_ata, false),               // [3] output_token_account
                    AccountMeta::new(vault_a.unwrap_or_default(), false),   // [4] token_a_vault
                    AccountMeta::new(vault_b.unwrap_or_default(), false),   // [5] token_b_vault
                    AccountMeta::new_readonly(snap.mint_a, false),          // [6] token_a_mint
                    AccountMeta::new_readonly(snap.mint_b, false),          // [7] token_b_mint
                    AccountMeta::new(self.payer_pubkey, true),              // [8] payer
                    AccountMeta::new_readonly(token_a_prog, false),         // [9] token_a_program
                    AccountMeta::new_readonly(token_b_prog, false),         // [10] token_b_program
                    AccountMeta::new(referral_token_account, false),        // [11] referral_token_account
                    AccountMeta::new_readonly(event_authority, false),      // [12] event_authority
                    AccountMeta::new_readonly(METEORA_DAMM_V2_PROGRAM, false), // [13] program
                ]
            }

            // Meteora DLMM: 16 fixed accounts + remaining bin_arrays
            // swap.rs: [lb_pair, bin_array_bitmap_extension, reserve_x, reserve_y,
            //  user_token_in, user_token_out, token_x_mint, token_y_mint, oracle,
            //  host_fee_in, user, token_x_program, token_y_program, memo_program,
            //  event_authority, program, ...bin_arrays]
            // extra[0]=oracle, extra[1..]=bin_array PDAs (if available)
            DexType::MeteoraDlmm => {
                let oracle = extra.first().copied().unwrap_or_default();
                let token_x_prog = if snap.mint_a_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
                let token_y_prog = if snap.mint_b_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
                // bin_array_bitmap_extension and host_fee_in are optional accounts.
                // DLMM program checks if they're initialized — passing an uninitialized PDA
                // causes 0xbc4 (AccountNotInitialized). Use DLMM program_id as placeholder
                // in AccountMeta (the account_infos skip them, matching reference impl).
                let (event_authority, _) = Pubkey::find_program_address(
                    &[b"__event_authority"],
                    &METEORA_DLMM_PROGRAM,
                );
                let mut metas = vec![
                    AccountMeta::new(pool, false),                              // [0] lb_pair
                    AccountMeta::new_readonly(METEORA_DLMM_PROGRAM, false),     // [1] bin_array_bitmap_extension (placeholder)
                    AccountMeta::new(vault_a.unwrap_or_default(), false),        // [2] reserve_x
                    AccountMeta::new(vault_b.unwrap_or_default(), false),        // [3] reserve_y
                    AccountMeta::new(user_input_ata, false),                     // [4] user_token_in
                    AccountMeta::new(user_output_ata, false),                    // [5] user_token_out
                    AccountMeta::new_readonly(snap.mint_a, false),               // [6] token_x_mint
                    AccountMeta::new_readonly(snap.mint_b, false),               // [7] token_y_mint
                    AccountMeta::new(oracle, false),                             // [8] oracle
                    AccountMeta::new_readonly(METEORA_DLMM_PROGRAM, false),     // [9] host_fee_in (placeholder)
                    AccountMeta::new_readonly(self.payer_pubkey, true),           // [10] user
                    AccountMeta::new_readonly(token_x_prog, false),              // [11] token_x_program
                    AccountMeta::new_readonly(token_y_prog, false),              // [12] token_y_program
                    AccountMeta::new_readonly(SPL_MEMO_PROGRAM_ID, false),        // [13] memo_program
                    AccountMeta::new_readonly(event_authority, false),            // [14] event_authority
                    AccountMeta::new_readonly(METEORA_DLMM_PROGRAM, false),      // [15] program
                ];
                // Append bin_array remaining accounts (extra[1..])
                let bin_arrays = if extra.len() > 1 { &extra[1..] } else { &[] };
                if bin_arrays.is_empty() {
                    log::warn!("[DLMM_TX] bin_arrays missing for pool {}, skipping hop", pool);
                    return vec![];
                }
                log::debug!(
                    "[DLMM_TX] pool={} oracle={} bin_arrays={} vault_a={} vault_b={} 2022=({},{})",
                    &pool.to_string()[..8], &oracle.to_string()[..8],
                    bin_arrays.len(),
                    vault_a.map(|v| v.to_string()[..8].to_string()).unwrap_or("none".into()),
                    vault_b.map(|v| v.to_string()[..8].to_string()).unwrap_or("none".into()),
                    snap.mint_a_is_2022, snap.mint_b_is_2022,
                );
                for ba in bin_arrays {
                    metas.push(AccountMeta::new(*ba, false));
                }
                metas
            }

            // Orca Whirlpool: 15 accounts
            // swap.rs: [token_program_a, token_program_b, memo_program,
            //  token_authority, whirlpool, token_mint_a, token_mint_b,
            //  token_owner_account_a, token_vault_a, token_owner_account_b, token_vault_b,
            //  tick_array_0, tick_array_1, tick_array_2, oracle]
            // extra[0]=oracle, extra[1..]=tick_array PDAs
            DexType::OrcaWhirlpool => {
                let oracle = extra.first().copied().unwrap_or_default();
                let tick_arrays: Vec<Pubkey> = if extra.len() > 1 {
                    extra[1..].to_vec()
                } else {
                    vec![]
                };
                if tick_arrays.is_empty() || tick_arrays[0] == Pubkey::default() {
                    log::warn!("Whirlpool tick_array missing for pool {}, skipping hop", pool);
                    return vec![];
                }
                let token_a_prog = if snap.mint_a_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
                let token_b_prog = if snap.mint_b_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
                let user_ata_a = derive_ata_with_program(&self.payer_pubkey, &snap.mint_a, &token_a_prog);
                let user_ata_b = derive_ata_with_program(&self.payer_pubkey, &snap.mint_b, &token_b_prog);
                // Deduplicate tick arrays
                let mut unique_tas = vec![tick_arrays[0]];
                for ta in tick_arrays.iter().skip(1) {
                    if !unique_tas.contains(ta) {
                        unique_tas.push(*ta);
                    }
                    if unique_tas.len() >= 3 {
                        break;
                    }
                }
                // Pad to exactly 3 tick arrays (Whirlpool requires exactly 3)
                while unique_tas.len() < 3 {
                    unique_tas.push(*unique_tas.last().unwrap());
                }
                vec![
                    AccountMeta::new_readonly(token_a_prog, false),             // [0] token_program_a
                    AccountMeta::new_readonly(token_b_prog, false),             // [1] token_program_b
                    AccountMeta::new_readonly(SPL_MEMO_PROGRAM_ID, false),       // [2] memo_program
                    AccountMeta::new_readonly(self.payer_pubkey, true),           // [3] token_authority
                    AccountMeta::new(pool, false),                              // [4] whirlpool
                    AccountMeta::new_readonly(snap.mint_a, false),              // [5] token_mint_a
                    AccountMeta::new_readonly(snap.mint_b, false),              // [6] token_mint_b
                    AccountMeta::new(user_ata_a, false),                        // [7] token_owner_account_a
                    AccountMeta::new(vault_a.unwrap_or_default(), false),        // [8] token_vault_a
                    AccountMeta::new(user_ata_b, false),                        // [9] token_owner_account_b
                    AccountMeta::new(vault_b.unwrap_or_default(), false),        // [10] token_vault_b
                    AccountMeta::new(unique_tas[0], false),                     // [11] tick_array_0
                    AccountMeta::new(unique_tas[1], false),                     // [12] tick_array_1
                    AccountMeta::new(unique_tas[2], false),                     // [13] tick_array_2
                    AccountMeta::new(oracle, false),                            // [14] oracle
                ]
            }
        }
    }

    fn calculate_jito_tip(&self, expected_profit: u64) -> Option<u64> {
        let tip = expected_profit * self.jito_tip_percentage as u64 / 100;
        let tip = tip.max(self.jito_min_tip);
        if tip + self.jito_min_operator_profit > expected_profit {
            return None; // Not profitable enough
        }
        Some(tip)
    }


    /// Build create_associated_token_account_idempotent instructions
    /// for base mint and all intermediate mints. Idempotent = no-op if exists.
    fn build_create_ata_ixs(&self, opp: &Opportunity) -> Vec<Instruction> {
        let mut mints: Vec<(Pubkey, bool)> = Vec::new(); // (mint, is_token_2022)
        let hop_count = opp.route.hops.len();

        // Intermediate mints (between hops)
        for i in 1..hop_count {
            let prev = &opp.pool_snapshots[i - 1];
            let (mint, is_2022) = if prev.is_a_to_b {
                (prev.mint_b, prev.mint_b_is_2022)
            } else {
                (prev.mint_a, prev.mint_a_is_2022)
            };
            if !mints.iter().any(|(m, _)| *m == mint) {
                mints.push((mint, is_2022));
            }
        }

        mints.iter().map(|(mint, is_2022)| {
            let token_program = if *is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
            Instruction {
                program_id: ATA_PROGRAM_ID,
                accounts: vec![
                    AccountMeta::new(self.payer_pubkey, true),       // funding
                    AccountMeta::new(derive_ata_with_program(&self.payer_pubkey, mint, &token_program), false), // ATA
                    AccountMeta::new_readonly(self.payer_pubkey, false), // wallet
                    AccountMeta::new_readonly(*mint, false),         // mint
                    AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
                    AccountMeta::new_readonly(token_program, false),
                ],
                data: vec![1], // 1 = CreateIdempotent
            }
        }).collect()
    }

    /// Build flashloan instructions (start, borrow, repay, end) if MarginFi is configured.
    /// Returns None if flashloan is not enabled or base_mint has no MarginFi bank.
    fn build_flashloan_ixs(
        &self,
        opp: &Opportunity,
        create_ata_ixs: &[Instruction],
    ) -> Option<FlashloanIxs> {
        let mfi = self.marginfi_state.as_ref()?;

        let base_mint = opp.route.base_mint;
        let bank = mfi.banks.get(&base_mint)?;
        let user_base_ata = derive_ata(&self.payer_pubkey, &base_mint);

        // Calculate end_flashloan index in the final instruction list.
        // Layout (swqos): [set_cu_limit, set_cu_price, ...create_atas, start, borrow, arb, repay, end, tip]
        // Layout (jito):  [set_cu_limit, ...create_atas, start, borrow, arb, repay, end, tip]
        // We use a placeholder; the actual index depends on the variant.
        // For swqos: 2 + create_ata_count + 4 (start, borrow, arb, repay) = end index
        // For jito:  1 + create_ata_count + 4 = end index
        // Since both variants build independently, we compute for the larger (swqos) case.
        // MarginFi checks: tx.instructions[end_index].program_id == MarginFi program
        // end_index is 0-based absolute position in the transaction.
        let ata_count = create_ata_ixs.len();
        // We'll use swqos layout as default (2 compute budget ixs).
        // For jito (1 compute budget ix), end_index would be off by 1,
        // but we build separate FlashloanIxs for each variant below.
        // Actually, start_flashloan's end_index is checked at runtime by MarginFi,
        // so we need the correct value. Let's compute per-variant in build().
        // For now, return the instructions with a placeholder end_index=0,
        // and fix it up in build().

        // Actually, let's just compute for both and pick later. Since this method
        // is called once, let's compute the swqos end_index (the common path).
        let end_index_swqos = (2 + ata_count + 4) as u64; // set_cu_limit + set_cu_price + atas + start + borrow + arb + repay
        let end_index_jito = (1 + ata_count + 4) as u64;  // set_cu_limit + atas + start + borrow + arb + repay

        let start = mfi.build_start_flashloan_ix(&self.payer_pubkey, end_index_swqos);
        let start_jito = mfi.build_start_flashloan_ix(&self.payer_pubkey, end_index_jito);
        let borrow = mfi.build_borrow_ix(&self.payer_pubkey, &base_mint, &user_base_ata, opp.amount_in)
            .ok()?;
        let repay = mfi.build_repay_ix(&self.payer_pubkey, &base_mint, &user_base_ata, opp.amount_in)
            .ok()?;
        let end = mfi.build_end_flashloan_ix(
            &self.payer_pubkey,
            &[(bank.address, bank.oracle)],
        );

        Some(FlashloanIxs { start, start_jito, borrow, repay, end })
    }

    fn calculate_cu_price(&self, _expected_profit: u64, base_cu: u32) -> u64 {
        // Fixed priority fee — do not scale with expected_profit.
        // SwQoS failed txs still land on-chain and pay fees, so keep it fixed and low.
        (self.swqos_priority_fee * 1_000_000) / base_cu as u64
    }
}

/// Pre-built flashloan instructions for wrapping arb transactions
struct FlashloanIxs {
    start: Instruction,      // start_flashloan (swqos end_index)
    start_jito: Instruction, // start_flashloan (jito end_index)
    borrow: Instruction,
    repay: Instruction,
    end: Instruction,
}
