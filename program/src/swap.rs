use pinocchio::{
    AccountView, Address, ProgramResult,
    error::ProgramError,
};

use crate::accounts::{
    SwapInstruction, HopInfo, DexType,
    ACCT_USER_BASE_ATA, pool_accounts_start,
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

    // Execute each hop: first hop gets amount_in, subsequent hops get 0 (= use full balance)
    for i in 0..ix.hop_count {
        let hop = &ix.hops[i as usize];
        let pool_start = pool_accounts_start(ix.hop_count, i, &ix.hops);
        let pool_end = pool_start + hop.dex_type.pool_account_count();

        if pool_end > accounts.len() {
            return Err(ProgramError::NotEnoughAccountKeys);
        }

        let amount = if i == 0 { ix.amount_in } else { 0 };
        dispatch_swap(hop, &accounts[pool_start..pool_end], amount)?;
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
fn dispatch_swap(hop: &HopInfo, pool_accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    match hop.dex_type {
        DexType::RaydiumAmmV4 => swap_raydium_amm(pool_accounts, amount_in),
        DexType::RaydiumCpmm => swap_raydium_cp(pool_accounts, amount_in),
        DexType::RaydiumClmm => swap_raydium_clmm(pool_accounts, amount_in),
        DexType::PumpFun => swap_pumpfun(pool_accounts, hop.is_a_to_b, amount_in),
        DexType::PumpSwap => swap_pumpswap(pool_accounts, hop.is_a_to_b, amount_in),
        DexType::Bonk => swap_bonkswap(pool_accounts, amount_in),
        DexType::MeteoraDammV2 => swap_meteora_damm_v2(pool_accounts, amount_in),
    }
}

// ── Per-DEX CPI wrappers ──

/// Raydium AMM V4: swap_base_in_v2 (8 accounts, no OpenBook needed).
fn swap_raydium_amm(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
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
        amount_in,
        minimum_amount_out: 0, // profit verified atomically
    };
    dex_pinocchio_cpi::raydium_amm::swap_base_in_v2(&swap_accounts, &args, &[])
}

/// Raydium CP: swap_base_input (13 accounts).
fn swap_raydium_cp(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
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
        amount_in,
        minimum_amount_out: 0,
    };
    dex_pinocchio_cpi::raydium_cp::swap_base_input(&swap_accounts, &args, &[])
}

/// Raydium CLMM: swap (10 accounts).
fn swap_raydium_clmm(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
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
        amount: amount_in,
        other_amount_threshold: 0, // profit verified atomically
        sqrt_price_limit_x64: 0,
        is_base_input: true,
    };
    dex_pinocchio_cpi::raydium_clmm::swap(&swap_accounts, &args, &[])
}

/// PumpFun: buy or sell based on direction (buy=16 accts, sell=14 accts).
fn swap_pumpfun(accounts: &[AccountView], is_a_to_b: bool, amount_in: u64) -> ProgramResult {
    if is_a_to_b {
        let buy_accounts = dex_pinocchio_cpi::pump_fun::BuyAccounts {
            global: &accounts[0],
            fee_recipient: &accounts[1],
            mint: &accounts[2],
            bonding_curve: &accounts[3],
            associated_bonding_curve: &accounts[4],
            associated_user: &accounts[5],
            user: &accounts[6],
            system_program: &accounts[7],
            token_program: &accounts[8],
            creator_vault: &accounts[9],
            event_authority: &accounts[10],
            program: &accounts[11],
            global_volume_accumulator: &accounts[12],
            user_volume_accumulator: &accounts[13],
            fee_config: &accounts[14],
            fee_program: &accounts[15],
        };
        let args = dex_pinocchio_cpi::pump_fun::BuyArgs {
            amount: amount_in,
            max_sol_cost: u64::MAX,
            track_volume: [0u8; 32],
        };
        dex_pinocchio_cpi::pump_fun::buy(&buy_accounts, &args, &[])
    } else {
        let sell_accounts = dex_pinocchio_cpi::pump_fun::SellAccounts {
            global: &accounts[0],
            fee_recipient: &accounts[1],
            mint: &accounts[2],
            bonding_curve: &accounts[3],
            associated_bonding_curve: &accounts[4],
            associated_user: &accounts[5],
            user: &accounts[6],
            system_program: &accounts[7],
            creator_vault: &accounts[8],
            token_program: &accounts[9],
            event_authority: &accounts[10],
            program: &accounts[11],
            fee_config: &accounts[12],
            fee_program: &accounts[13],
        };
        let args = dex_pinocchio_cpi::pump_fun::SellArgs {
            amount: amount_in,
            min_sol_output: 0,
        };
        dex_pinocchio_cpi::pump_fun::sell(&sell_accounts, &args, &[])
    }
}

/// PumpSwap (pump_fun_amm): buy or sell based on direction (buy=23 accts, sell=21 accts).
fn swap_pumpswap(accounts: &[AccountView], is_a_to_b: bool, amount_in: u64) -> ProgramResult {
    if is_a_to_b {
        let buy_accounts = dex_pinocchio_cpi::pump_fun_amm::BuyAccounts {
            pool: &accounts[0],
            user: &accounts[1],
            global_config: &accounts[2],
            base_mint: &accounts[3],
            quote_mint: &accounts[4],
            user_base_token_account: &accounts[5],
            user_quote_token_account: &accounts[6],
            pool_base_token_account: &accounts[7],
            pool_quote_token_account: &accounts[8],
            protocol_fee_recipient: &accounts[9],
            protocol_fee_recipient_token_account: &accounts[10],
            base_token_program: &accounts[11],
            quote_token_program: &accounts[12],
            system_program: &accounts[13],
            associated_token_program: &accounts[14],
            event_authority: &accounts[15],
            program: &accounts[16],
            coin_creator_vault_ata: &accounts[17],
            coin_creator_vault_authority: &accounts[18],
            global_volume_accumulator: &accounts[19],
            user_volume_accumulator: &accounts[20],
            fee_config: &accounts[21],
            fee_program: &accounts[22],
        };
        let args = dex_pinocchio_cpi::pump_fun_amm::BuyArgs {
            base_amount_out: amount_in,
            max_quote_amount_in: u64::MAX,
            track_volume: [0u8; 32],
        };
        dex_pinocchio_cpi::pump_fun_amm::buy(&buy_accounts, &args, &[])
    } else {
        let sell_accounts = dex_pinocchio_cpi::pump_fun_amm::SellAccounts {
            pool: &accounts[0],
            user: &accounts[1],
            global_config: &accounts[2],
            base_mint: &accounts[3],
            quote_mint: &accounts[4],
            user_base_token_account: &accounts[5],
            user_quote_token_account: &accounts[6],
            pool_base_token_account: &accounts[7],
            pool_quote_token_account: &accounts[8],
            protocol_fee_recipient: &accounts[9],
            protocol_fee_recipient_token_account: &accounts[10],
            base_token_program: &accounts[11],
            quote_token_program: &accounts[12],
            system_program: &accounts[13],
            associated_token_program: &accounts[14],
            event_authority: &accounts[15],
            program: &accounts[16],
            coin_creator_vault_ata: &accounts[17],
            coin_creator_vault_authority: &accounts[18],
            fee_config: &accounts[19],
            fee_program: &accounts[20],
        };
        let args = dex_pinocchio_cpi::pump_fun_amm::SellArgs {
            base_amount_in: amount_in,
            min_quote_amount_out: 0,
        };
        dex_pinocchio_cpi::pump_fun_amm::sell(&sell_accounts, &args, &[])
    }
}

/// Bonkswap: swap (17 accounts).
fn swap_bonkswap(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    let swap_accounts = dex_pinocchio_cpi::bonkswap::SwapAccounts {
        state: &accounts[0],
        pool: &accounts[1],
        token_x: &accounts[2],
        token_y: &accounts[3],
        pool_x_account: &accounts[4],
        pool_y_account: &accounts[5],
        swapper_x_account: &accounts[6],
        swapper_y_account: &accounts[7],
        swapper: &accounts[8],
        referrer_x_account: &accounts[9],
        referrer_y_account: &accounts[10],
        referrer: &accounts[11],
        program_authority: &accounts[12],
        system_program: &accounts[13],
        token_program: &accounts[14],
        associated_token_program: &accounts[15],
        rent: &accounts[16],
    };
    let mut delta_in = [0u8; 32];
    delta_in[..8].copy_from_slice(&amount_in.to_le_bytes());
    let args = dex_pinocchio_cpi::bonkswap::SwapArgs {
        delta_in,
        price_limit: [0u8; 32],
        x_to_y: true, // direction set by account ordering from off-chain
    };
    dex_pinocchio_cpi::bonkswap::swap(&swap_accounts, &args, &[])
}

/// Meteora DAMM V2: swap (14 accounts).
fn swap_meteora_damm_v2(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    let swap_accounts = dex_pinocchio_cpi::meteora_damm_v2::SwapAccounts {
        pool_authority: &accounts[0],
        pool: &accounts[1],
        input_token_account: &accounts[2],
        output_token_account: &accounts[3],
        token_a_vault: &accounts[4],
        token_b_vault: &accounts[5],
        token_a_mint: &accounts[6],
        token_b_mint: &accounts[7],
        payer: &accounts[8],
        token_a_program: &accounts[9],
        token_b_program: &accounts[10],
        referral_token_account: &accounts[11],
        event_authority: &accounts[12],
        program: &accounts[13],
    };
    let mut params = [0u8; 32];
    params[..8].copy_from_slice(&amount_in.to_le_bytes());
    let args = dex_pinocchio_cpi::meteora_damm_v2::SwapArgs { params };
    dex_pinocchio_cpi::meteora_damm_v2::swap(&swap_accounts, &args, &[])
}
