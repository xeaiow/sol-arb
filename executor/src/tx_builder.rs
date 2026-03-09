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

// ── DEX Program IDs ──
const RAYDIUM_CPMM_PROGRAM: Pubkey = solana_sdk::pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
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
const PUMPSWAP_GLOBAL_CONFIG: Pubkey = solana_sdk::pubkey!("ADyA8hdefbFUWVfrRxCDdvo7EhBYic9nCR4jBdMZxW8R");
/// PumpSwap protocol fee recipient
const PUMPSWAP_PROTOCOL_FEE_RECIPIENT: Pubkey = solana_sdk::pubkey!("62qc2CNXwrYqQScmEdiZFFAnJR262PxxUZLtQ3iEQFhg");
/// PumpSwap fee program (same as PumpFun)
const PUMPSWAP_FEE_PROGRAM: Pubkey = solana_sdk::pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");

// ── Meteora DAMM V2 constants ──
const METEORA_DAMM_V2_PROGRAM: Pubkey = solana_sdk::pubkey!("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");

/// SPL Token program ID
const SPL_TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Token-2022 program ID
const TOKEN_2022_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
/// Associated Token Account program ID
const ATA_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
/// System program ID
const SYSTEM_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("11111111111111111111111111111111");

/// Derive an Associated Token Account address (PDA)
fn derive_ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), SPL_TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
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
    swqos_cu_price_percentage: u32,
    jito_enabled: bool,
    swqos_enabled: bool,
    alt: Option<Arc<Tier0Alt>>,
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

        let swqos_pct = config
            .flashblock
            .as_ref()
            .map_or(30, |f| f.cu_price_percentage)
            .max(
                config
                    .astralane
                    .as_ref()
                    .map_or(30, |a| a.cu_price_percentage),
            );

        Self {
            program_id: config.executor.program_id.parse()
                .expect("Invalid program_id in config"),
            payer_pubkey,
            fee_collectors,
            cu_jitter_range: config.executor.anti_fingerprint.cu_jitter_range,
            jito_tip_percentage: jito.map_or(60, |j| j.tip_percentage),
            jito_min_tip: jito.map_or(1000, |j| j.min_tip_lamports),
            jito_min_operator_profit: jito.map_or(5000, |j| j.min_operator_profit_lamports),
            swqos_cu_price_percentage: swqos_pct,
            jito_enabled,
            swqos_enabled: flashblock_enabled || astralane_enabled,
            alt: None,
        }
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
        let base_cu = anti_fp::estimate_cu(&dex_types);
        let cu_limit = anti_fp::jittered_cu(base_cu, self.cu_jitter_range);

        let arb_ix = self.build_arb_instruction(opp);

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
                vec![
                    ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                    arb_ix.clone(),
                    system_instruction::transfer(&payer.pubkey(), &tip_account, tip),
                ]
            })
        } else {
            None
        };

        let swqos_ixs = if self.swqos_enabled {
            let cu_price = self.calculate_cu_price(opp.expected_profit, base_cu);
            Some(vec![
                ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                ComputeBudgetInstruction::set_compute_unit_price(cu_price),
                arb_ix,
            ])
        } else {
            None
        };

        // Parallel signing with rayon::join
        let payer_pubkey = payer.pubkey();
        let (jito_tx, swqos_tx) = rayon::join(
            || {
                jito_ixs.as_ref().and_then(|ixs| {
                    let msg = message::v0::Message::try_compile(
                        &payer_pubkey,
                        ixs,
                        &lookup_tables,
                        recent_blockhash,
                    )
                    .ok()?;
                    VersionedTransaction::try_new(VersionedMessage::V0(msg), &[payer]).ok()
                })
            },
            || {
                swqos_ixs.as_ref().and_then(|ixs| {
                    let msg = message::v0::Message::try_compile(
                        &payer_pubkey,
                        ixs,
                        &lookup_tables,
                        recent_blockhash,
                    )
                    .ok()?;
                    VersionedTransaction::try_new(VersionedMessage::V0(msg), &[payer]).ok()
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

    fn encode_instruction_data(&self, opp: &Opportunity, hop_count: u8) -> Vec<u8> {
        let mut data = Vec::with_capacity(20);

        // Discriminator: 0=2hop, 1=3hop, 2=4hop
        data.push(hop_count - 2);

        // First (buy) and last (sell) DEX types
        data.push(opp.pool_snapshots[0].dex_type as u8);
        data.push(opp.pool_snapshots[hop_count as usize - 1].dex_type as u8);

        // Flags byte: bit0=buy_a_to_b, bit1=sell_a_to_b, bit2=buy_2022, bit3=sell_2022
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
            let intermediate_ata = derive_ata(&self.payer_pubkey, &intermediate_mint);
            metas.push(AccountMeta::new(intermediate_ata, false));             // user ATA
        }

        // Per-hop pool accounts — assembled per DEX type with correct ordering.
        // PoolSnapshot.accounts layout: [pool_address, vault_a?, vault_b?, extra...]
        for snapshot in &opp.pool_snapshots {
            let hop_metas = self.build_hop_accounts(snapshot);
            metas.extend(hop_metas);
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
        let (input_mint, output_mint) = if snap.is_a_to_b {
            (snap.mint_a, snap.mint_b)
        } else {
            (snap.mint_b, snap.mint_a)
        };
        let user_input_ata = derive_ata(&self.payer_pubkey, &input_mint);
        let user_output_ata = derive_ata(&self.payer_pubkey, &output_mint);
        let (input_vault, output_vault) = if snap.is_a_to_b {
            (vault_a.unwrap_or_default(), vault_b.unwrap_or_default())
        } else {
            (vault_b.unwrap_or_default(), vault_a.unwrap_or_default())
        };

        match snap.dex_type {
            // Raydium AMM V4: 8 accounts
            // swap.rs: [token_program, amm, amm_authority, coin_vault, pc_vault,
            //           user_source, user_dest, user_owner]
            DexType::RaydiumAmmV4 => vec![
                AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
                AccountMeta::new(pool, false),
                AccountMeta::new_readonly(RAYDIUM_AMM_AUTHORITY, false),
                AccountMeta::new(vault_a.unwrap_or_default(), false),
                AccountMeta::new(vault_b.unwrap_or_default(), false),
                AccountMeta::new(user_input_ata, false),
                AccountMeta::new(user_output_ata, false),
                AccountMeta::new_readonly(self.payer_pubkey, true),
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
                ]
            }

            // Raydium CLMM: 10 accounts
            // extra[0]=amm_config, extra[1]=observation_key, extra[2]=tick_array
            // swap.rs: [payer, amm_config, pool_state, input_ata, output_ata,
            //  input_vault, output_vault, observation, token_program, tick_array]
            DexType::RaydiumClmm => {
                let amm_config = extra.first().copied().unwrap_or_default();
                let observation = extra.get(1).copied().unwrap_or_default();
                let tick_array = extra.get(2).copied().unwrap_or_default();
                vec![
                    AccountMeta::new(self.payer_pubkey, true),
                    AccountMeta::new_readonly(amm_config, false),
                    AccountMeta::new(pool, false),
                    AccountMeta::new(user_input_ata, false),
                    AccountMeta::new(user_output_ata, false),
                    AccountMeta::new(input_vault, false),
                    AccountMeta::new(output_vault, false),
                    AccountMeta::new(observation, false),
                    AccountMeta::new_readonly(SPL_TOKEN_PROGRAM_ID, false),
                    AccountMeta::new(tick_array, false),
                ]
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
                // fee_config PDA of fee program: ["fee_config"]
                let (fee_config, _) = Pubkey::find_program_address(
                    &[b"fee_config"],
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
                let user_base_ata = derive_ata(&self.payer_pubkey, &base_mint);
                let user_quote_ata = derive_ata(&self.payer_pubkey, &quote_mint);
                let pool_base_vault = vault_a.unwrap_or_default();
                let pool_quote_vault = vault_b.unwrap_or_default();
                // protocol_fee_recipient_token_account = ATA(protocol_fee_recipient, quote_mint)
                let protocol_fee_recipient_ata = derive_ata(&PUMPSWAP_PROTOCOL_FEE_RECIPIENT, &quote_mint);
                let base_token_prog = if snap.mint_a_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
                let quote_token_prog = if snap.mint_b_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
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
                let coin_creator_vault_ata = derive_ata(&coin_creator_vault_authority, &quote_mint);
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
                // fee_config PDA
                let (fee_config, _) = Pubkey::find_program_address(
                    &[b"fee_config"],
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
                // pool_authority PDA: seeds depend on Meteora impl; commonly ["pool_authority", pool]
                let (pool_authority, _) = Pubkey::find_program_address(
                    &[b"pool_authority", pool.as_ref()],
                    &METEORA_DAMM_V2_PROGRAM,
                );
                let token_a_prog = if snap.mint_a_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
                let token_b_prog = if snap.mint_b_is_2022 { TOKEN_2022_PROGRAM_ID } else { SPL_TOKEN_PROGRAM_ID };
                // event_authority PDA
                let (event_authority, _) = Pubkey::find_program_address(
                    &[b"__event_authority"],
                    &METEORA_DAMM_V2_PROGRAM,
                );
                // referral_token_account: use payer's quote ATA as self-referral (0 fees)
                let referral_token_account = derive_ata(&self.payer_pubkey, &snap.mint_b);
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

    fn calculate_cu_price(&self, expected_profit: u64, base_cu: u32) -> u64 {
        let fee_budget = expected_profit * self.swqos_cu_price_percentage as u64 / 100;
        // micro-lamports per CU
        (fee_budget * 1_000_000) / base_cu as u64
    }
}
