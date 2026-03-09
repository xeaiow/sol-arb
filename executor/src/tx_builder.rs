use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::{
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signer::{keypair::Keypair, Signer},
    transaction::Transaction,
};
use solana_system_interface::instruction as system_instruction;

use arb_engine::opportunity::Opportunity;

use crate::anti_fp;
use crate::config::ExecutorConfigFile;

/// Built transaction pair
pub struct TxPair {
    pub jito_tx: Option<Transaction>,  // Variant A: with tip, no CU price
    pub swqos_tx: Option<Transaction>, // Variant B: with CU price, no tip
}

pub struct TxBuilder {
    program_id: Pubkey,
    fee_collectors: Vec<Pubkey>,
    cu_jitter_range: u32,
    jito_tip_percentage: u32,
    jito_min_tip: u64,
    jito_min_operator_profit: u64,
    swqos_cu_price_percentage: u32,
    jito_enabled: bool,
    swqos_enabled: bool,
}

impl TxBuilder {
    pub fn from_config(config: &ExecutorConfigFile) -> Self {
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
            fee_collectors,
            cu_jitter_range: config.executor.anti_fingerprint.cu_jitter_range,
            jito_tip_percentage: jito.map_or(60, |j| j.tip_percentage),
            jito_min_tip: jito.map_or(1000, |j| j.min_tip_lamports),
            jito_min_operator_profit: jito.map_or(5000, |j| j.min_operator_profit_lamports),
            swqos_cu_price_percentage: swqos_pct,
            jito_enabled,
            swqos_enabled: flashblock_enabled || astralane_enabled,
        }
    }

    /// Build two transaction variants from an Opportunity.
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

        // Variant A: Jito bundle (CU limit + arb ix + tip, NO CU price)
        let jito_tx = if self.jito_enabled {
            let tip = self.calculate_jito_tip(opp.expected_profit);
            if let Some(tip) = tip {
                let tip_account = anti_fp::random_tip_account();
                let ixs = vec![
                    ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                    arb_ix.clone(),
                    system_instruction::transfer(&payer.pubkey(), &tip_account, tip),
                ];
                let tx = Transaction::new_signed_with_payer(
                    &ixs,
                    Some(&payer.pubkey()),
                    &[payer],
                    recent_blockhash,
                );
                Some(tx)
            } else {
                None
            }
        } else {
            None
        };

        // Variant B: SWQoS (CU limit + CU price + arb ix, NO tip)
        let swqos_tx = if self.swqos_enabled {
            let cu_price = self.calculate_cu_price(opp.expected_profit, base_cu);
            let ixs = vec![
                ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                ComputeBudgetInstruction::set_compute_unit_price(cu_price),
                arb_ix,
            ];
            let tx = Transaction::new_signed_with_payer(
                &ixs,
                Some(&payer.pubkey()),
                &[payer],
                recent_blockhash,
            );
            Some(tx)
        } else {
            None
        };

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

        // Flags byte: bit0=buy_a_to_b, bit1=sell_a_to_b, bit2=buy_2022, bit3=sell_2022, bit4=flashloan
        let mut flags: u8 = 0;
        if opp.pool_snapshots[0].is_a_to_b {
            flags |= 1;
        }
        if opp.pool_snapshots[hop_count as usize - 1].is_a_to_b {
            flags |= 1 << 1;
        }
        // TODO: set token_2022 bits when needed
        // TODO: set flashloan bit from config
        data.push(flags);

        // Middle hops (3-hop and 4-hop)
        for i in 1..hop_count as usize - 1 {
            data.push(opp.pool_snapshots[i].dex_type as u8);
            let mut mid_flags: u8 = 0;
            if opp.pool_snapshots[i].is_a_to_b {
                mid_flags |= 1;
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

    fn build_account_metas(&self, _opp: &Opportunity) -> Vec<AccountMeta> {
        // TODO: implement full account meta construction
        // Layout: header(8) + intermediate tokens + flashloan(if enabled) + per-hop pool accounts
        Vec::new()
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
