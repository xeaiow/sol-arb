use anyhow::Result;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    transaction::Transaction,
};
use solana_system_interface::instruction as system_instruction;

use arb_engine::opportunity::Opportunity;

use crate::anti_fp;

/// Build the full arbitrage transaction from an Opportunity.
///
/// Returns an unsigned `Transaction` ready for signing and submission.
pub fn build_arb_tx(
    opp: &Opportunity,
    program_id: &Pubkey,
    payer: &Pubkey,
    tip_lamports: u64,
    cu_price_micro_lamports: u64,
    jitter_range: u32,
) -> Result<Transaction> {
    let hop_count = opp.route.hops.len();

    // 1. Estimate CU based on DEX types in the route
    let dex_types: Vec<u8> = opp
        .pool_snapshots
        .iter()
        .map(|ps| ps.dex_type as u8)
        .collect();
    let base_cu = anti_fp::estimate_cu(&dex_types);
    let cu_limit = anti_fp::jittered_cu(base_cu, jitter_range);

    // 2. Build instructions
    let mut ixs: Vec<Instruction> = Vec::with_capacity(4 + hop_count);

    // Compute budget: set CU limit
    ixs.push(ComputeBudgetInstruction::set_compute_unit_limit(cu_limit));

    // Compute budget: set CU price (priority fee)
    ixs.push(ComputeBudgetInstruction::set_compute_unit_price(
        cu_price_micro_lamports,
    ));

    // 3. Build the arb program instruction
    let arb_ix = build_arb_instruction(opp, program_id, payer)?;
    ixs.push(arb_ix);

    // 4. Jito tip transfer (random tip account for anti-fingerprinting)
    let tip_account = anti_fp::random_tip_account();
    ixs.push(system_instruction::transfer(
        payer,
        &tip_account,
        tip_lamports,
    ));

    // 5. Build unsigned transaction
    let tx = Transaction::new_with_payer(&ixs, Some(payer));

    Ok(tx)
}

/// Build the arb program instruction with account metas for all hops.
fn build_arb_instruction(
    opp: &Opportunity,
    program_id: &Pubkey,
    payer: &Pubkey,
) -> Result<Instruction> {
    let accounts = build_account_metas(opp, payer);

    // Instruction data: [hop_count(u8), amount_in(u64), min_profit(u64),
    //   per-hop: [pool_index(u32), is_a_to_b(u8)]]
    let hop_count = opp.route.hops.len() as u8;
    let mut data = Vec::with_capacity(1 + 8 + 8 + (hop_count as usize) * 5);

    data.push(hop_count);
    data.extend_from_slice(&opp.amount_in.to_le_bytes());
    // min_profit = 0 for now (the on-chain program enforces break-even)
    data.extend_from_slice(&0u64.to_le_bytes());

    for hop in opp.route.hops.iter() {
        data.extend_from_slice(&hop.pool_index.to_le_bytes());
        data.push(hop.is_a_to_b as u8);
    }

    Ok(Instruction {
        program_id: *program_id,
        accounts,
        data,
    })
}

/// Build account metas for the arb instruction.
///
/// TODO: Populate with actual accounts from pool_snapshots based on DEX type.
/// Each DEX has different account layouts; this is a scaffold that includes
/// the payer/signer and all pool accounts from the snapshots.
fn build_account_metas(opp: &Opportunity, payer: &Pubkey) -> Vec<AccountMeta> {
    let mut metas = Vec::new();

    // Signer / payer is always first
    metas.push(AccountMeta::new(*payer, true));

    // Add all accounts from each pool snapshot
    for snapshot in &opp.pool_snapshots {
        // Pool address (writable, not signer)
        metas.push(AccountMeta::new(snapshot.address, false));

        // Additional accounts for this pool's DEX
        for acct in &snapshot.accounts {
            metas.push(AccountMeta::new(*acct, false));
        }
    }

    metas
}
