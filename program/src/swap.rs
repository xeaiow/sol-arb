use pinocchio::{
    AccountView, Address, ProgramResult,
    error::ProgramError,
};

use crate::accounts::{
    SwapInstruction, HopInfo, DexType,
    ACCT_USER_BASE_ATA, pool_accounts_start,
    HEADER_SIZE, INTERMEDIATE_ACCOUNTS_PER_TOKEN, FLASHLOAN_ACCOUNT_COUNT,
};

/// Execute a multi-hop swap with profit verification.
pub fn execute(
    _program_id: &Address,
    accounts: &[AccountView],
    data: &[u8],
    hops: u8,
) -> ProgramResult {
    let ix = SwapInstruction::parse(data, hops)?;

    // Read initial balance of user base token ATA
    let initial_balance = read_token_balance(&accounts[ACCT_USER_BASE_ATA])?;

    if ix.use_flashloan {
        let fl_start = HEADER_SIZE + (ix.hop_count as usize - 1) * INTERMEDIATE_ACCOUNTS_PER_TOKEN;
        let fl_accounts = &accounts[fl_start..fl_start + FLASHLOAN_ACCOUNT_COUNT];
        crate::flashloan::flash_borrow(fl_accounts, ix.amount_in)?;
    }

    // Execute each hop
    for i in 0..ix.hop_count {
        let hop = &ix.hops[i as usize];
        let pool_start = pool_accounts_start(
            ix.hop_count,
            ix.use_flashloan,
            i,
            &ix.hops,
        );
        let pool_end = pool_start + hop.dex_type.pool_account_count();

        if pool_end > accounts.len() {
            return Err(ProgramError::NotEnoughAccountKeys);
        }

        dispatch_swap(hop, &accounts[pool_start..pool_end])?;
    }

    if ix.use_flashloan {
        let fl_start = HEADER_SIZE + (ix.hop_count as usize - 1) * INTERMEDIATE_ACCOUNTS_PER_TOKEN;
        let fl_accounts = &accounts[fl_start..fl_start + FLASHLOAN_ACCOUNT_COUNT];
        crate::flashloan::flash_repay(fl_accounts, ix.amount_in)?;
    }

    // Verify profit
    let final_balance = read_token_balance(&accounts[ACCT_USER_BASE_ATA])?;
    if final_balance < initial_balance {
        return Err(ProgramError::Custom(1)); // Loss
    }
    let profit = final_balance - initial_balance;
    if profit < ix.min_profit as u64 {
        return Err(ProgramError::Custom(2)); // Insufficient profit
    }

    Ok(())
}

/// Read SPL token account balance at byte offset 64 (u64 LE).
fn read_token_balance(account: &AccountView) -> Result<u64, ProgramError> {
    let data = account.try_borrow()?;
    if data.len() < 72 {
        return Err(ProgramError::InvalidAccountData);
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[64..72]);
    Ok(u64::from_le_bytes(bytes))
}

/// Route to the correct DEX CPI.
fn dispatch_swap(hop: &HopInfo, pool_accounts: &[AccountView]) -> ProgramResult {
    match hop.dex_type {
        DexType::RaydiumAmmV4 => swap_raydium_amm(pool_accounts),
        DexType::RaydiumCpmm => swap_raydium_cp(pool_accounts),
        DexType::RaydiumClmm => swap_raydium_clmm(pool_accounts),
        DexType::PumpFun => swap_pumpfun(pool_accounts, hop.is_a_to_b),
        DexType::PumpSwap => swap_pumpswap(pool_accounts, hop.is_a_to_b),
        DexType::Bonk => swap_bonkswap(pool_accounts),
        DexType::MeteoraDammV2 => swap_meteora_damm_v2(pool_accounts),
    }
}

// ── Per-DEX CPI wrappers ──

/// Raydium AMM V4: swap_base_in_v2 (8 accounts, no OpenBook needed).
fn swap_raydium_amm(accounts: &[AccountView]) -> ProgramResult {
    let swap_accounts = dex_pinocchio_cpi::raydium_amm::SwapBaseInV2Accounts {
        token_program: &accounts[0],
        amm: &accounts[1],
        amm_authority: &accounts[2],
        amm_coin_vault: &accounts[3],
        amm_pc_vault: &accounts[4],
        user_source: &accounts[5],
        user_destination: &accounts[6],
        user_owner: &accounts[7],
    };
    let args = dex_pinocchio_cpi::raydium_amm::SwapBaseInV2Args {
        amount_in: 0,          // TODO: wire actual amount
        minimum_amount_out: 0, // no slippage — profit verified atomically
    };
    dex_pinocchio_cpi::raydium_amm::swap_base_in_v2(&swap_accounts, &args, &[])
}

/// Raydium CP: swap_base_input (13 accounts).
fn swap_raydium_cp(accounts: &[AccountView]) -> ProgramResult {
    let swap_accounts = dex_pinocchio_cpi::raydium_cp::SwapBaseInputAccounts {
        payer: &accounts[0],
        authority: &accounts[1],
        amm_config: &accounts[2],
        pool_state: &accounts[3],
        input_token_account: &accounts[4],
        output_token_account: &accounts[5],
        input_vault: &accounts[6],
        output_vault: &accounts[7],
        input_token_program: &accounts[8],
        output_token_program: &accounts[9],
        input_token_mint: &accounts[10],
        output_token_mint: &accounts[11],
        observation_state: &accounts[12],
    };
    let args = dex_pinocchio_cpi::raydium_cp::SwapBaseInputArgs {
        amount_in: 0,          // TODO: wire actual amount
        minimum_amount_out: 0,
    };
    dex_pinocchio_cpi::raydium_cp::swap_base_input(&swap_accounts, &args, &[])
}

/// Raydium CLMM: swap (10 accounts).
fn swap_raydium_clmm(accounts: &[AccountView]) -> ProgramResult {
    let swap_accounts = dex_pinocchio_cpi::raydium_clmm::SwapAccounts {
        payer: &accounts[0],
        amm_config: &accounts[1],
        pool_state: &accounts[2],
        input_token_account: &accounts[3],
        output_token_account: &accounts[4],
        input_vault: &accounts[5],
        output_vault: &accounts[6],
        observation_state: &accounts[7],
        token_program: &accounts[8],
        tick_array: &accounts[9],
    };
    let args = dex_pinocchio_cpi::raydium_clmm::SwapArgs {
        amount: 0,                 // TODO: wire actual amount
        other_amount_threshold: 0, // no slippage — profit verified atomically
        sqrt_price_limit_x64: 0,
        is_base_input: true,
    };
    dex_pinocchio_cpi::raydium_clmm::swap(&swap_accounts, &args, &[])
}

/// PumpFun: buy or sell based on direction.
fn swap_pumpfun(_accounts: &[AccountView], _is_a_to_b: bool) -> ProgramResult {
    // PumpFun: buy (16 accts) or sell (14 accts) based on direction
    // TODO: implement buy/sell branching with actual account mapping
    Ok(())
}

/// PumpSwap (pump_fun_amm): buy or sell based on direction.
fn swap_pumpswap(_accounts: &[AccountView], _is_a_to_b: bool) -> ProgramResult {
    // PumpSwap (pump_fun_amm): buy (23 accts) or sell based on direction
    // TODO: implement buy/sell branching with actual account mapping
    Ok(())
}

/// Bonkswap: swap (17 accounts).
fn swap_bonkswap(_accounts: &[AccountView]) -> ProgramResult {
    // Bonkswap: swap (17 accts)
    // TODO: implement with actual account mapping
    Ok(())
}

/// Meteora DAMM V2: swap (14 accounts).
fn swap_meteora_damm_v2(_accounts: &[AccountView]) -> ProgramResult {
    // Meteora DAMM V2: swap (14 accts)
    // TODO: implement with actual account mapping
    Ok(())
}
