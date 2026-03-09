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

use arb_engine::opportunity::Opportunity;

use crate::alt::Tier0Alt;
use crate::anti_fp;
use crate::config::ExecutorConfigFile;

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
            program_id: config.executor.program_id.parse().unwrap(),
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

        // Per-hop pool accounts from snapshots
        for snapshot in &opp.pool_snapshots {
            for acct in &snapshot.accounts {
                metas.push(AccountMeta::new(*acct, false));
            }
        }

        metas
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
