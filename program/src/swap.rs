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
        let pool_end = pool_start + hop.dex_type.base_pool_account_count() + hop.extra_accounts as usize;

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

/// Test mode: execute swaps without profit verification.
/// WARNING: will lose money if swap is unprofitable. For testing only.
pub fn execute_test(
    _program_id: &Address,
    accounts: &[AccountView],
    data: &[u8],
    hops: u8,
) -> ProgramResult {
    let ix = SwapInstruction::parse(data, hops)?;

    for i in 0..ix.hop_count {
        let hop = &ix.hops[i as usize];
        let pool_start = pool_accounts_start(ix.hop_count, i, &ix.hops);
        let pool_end = pool_start + hop.dex_type.base_pool_account_count() + hop.extra_accounts as usize;

        if pool_end > accounts.len() {
            return Err(ProgramError::NotEnoughAccountKeys);
        }

        let amount = if i == 0 { ix.amount_in } else { 0 };
        dispatch_swap(hop, &accounts[pool_start..pool_end], amount)?;
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
/// When amount_in == 0 (non-first hops), reads the user's input token account
/// balance to determine the actual amount. This is necessary because some DEXes
/// (e.g. Raydium CLMM) reject amount=0 with ZeroAmountSpecified.
fn dispatch_swap(hop: &HopInfo, pool_accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    let amount = if amount_in == 0 {
        // Read balance from user's input token account.
        // The input ATA position varies by DEX and direction:
        let input_ata_index = match hop.dex_type {
            DexType::RaydiumAmmV4 => 5,  // user_source
            DexType::RaydiumCpmm => 4,   // input_token_account
            DexType::RaydiumClmm => 3,   // input_token_account
            DexType::PumpFun => 5,       // associated_user (token ATA, used for both buy/sell)
            DexType::PumpSwap => if hop.is_a_to_b { 6 } else { 5 },
                // buy: input=quote [6], sell: input=base [5]
            DexType::Bonk => 6,          // swapper_x_account
            DexType::MeteoraDammV2 => 2, // input_token_account
        DexType::MeteoraDlmm => 4, // user_token_in
        DexType::OrcaWhirlpool => if hop.is_a_to_b { 7 } else { 9 },
            // a_to_b: input=token_owner_account_a [7], b_to_a: input=token_owner_account_b [9]
        };
        read_token_balance(&pool_accounts[input_ata_index])?
    } else {
        amount_in
    };

    match hop.dex_type {
        DexType::RaydiumAmmV4 => swap_raydium_amm(pool_accounts, amount),
        DexType::RaydiumCpmm => swap_raydium_cp(pool_accounts, amount),
        DexType::RaydiumClmm => swap_raydium_clmm(pool_accounts, amount),
        DexType::PumpFun => swap_pumpfun(pool_accounts, hop.is_a_to_b, amount),
        DexType::PumpSwap => swap_pumpswap(pool_accounts, hop.is_a_to_b, amount),
        DexType::Bonk => swap_bonkswap(pool_accounts, amount),
        DexType::MeteoraDammV2 => swap_meteora_damm_v2(pool_accounts, amount),
        DexType::MeteoraDlmm => swap_meteora_dlmm(pool_accounts, amount),
        DexType::OrcaWhirlpool => swap_orca_whirlpool(pool_accounts, amount, hop.is_a_to_b),
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

/// Raydium CLMM: swap with up to 3 unique tick arrays.
/// accounts[0..10] = base swap accounts, accounts[10..N] = extra unique tick arrays.
/// Executor must only pass unique tick arrays (no duplicates) to avoid AccountBorrowFailed.
/// Total accounts: 10 (1 tick array), 11 (2 tick arrays), or 12 (3 tick arrays).
fn swap_raydium_clmm(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    use pinocchio::instruction::{InstructionView, InstructionAccount};
    use pinocchio::cpi::invoke_signed_with_slice;

    // Build instruction data: 8-byte discriminator + SwapArgs
    let args = dex_pinocchio_cpi::raydium_clmm::SwapArgs {
        amount: amount_in,
        other_amount_threshold: 0,
        sqrt_price_limit_x64: 0,
        is_base_input: true,
    };
    let mut data = [0u8; 8 + core::mem::size_of::<dex_pinocchio_cpi::raydium_clmm::SwapArgs>()];
    data[0..8].copy_from_slice(&dex_pinocchio_cpi::raydium_clmm::SWAP);
    unsafe {
        core::ptr::copy_nonoverlapping(
            &args as *const _ as *const u8,
            data.as_mut_ptr().add(8),
            core::mem::size_of::<dex_pinocchio_cpi::raydium_clmm::SwapArgs>(),
        );
    }

    // All tick arrays are unique — executor guarantees no duplicates.
    let total = accounts.len().min(12);

    let instruction_accounts: [InstructionAccount; 12] = [
        InstructionAccount::readonly_signer(accounts[0].address()),
        InstructionAccount::readonly(accounts[1].address()),
        InstructionAccount::writable(accounts[2].address()),
        InstructionAccount::writable(accounts[3].address()),
        InstructionAccount::writable(accounts[4].address()),
        InstructionAccount::writable(accounts[5].address()),
        InstructionAccount::writable(accounts[6].address()),
        InstructionAccount::writable(accounts[7].address()),
        InstructionAccount::readonly(accounts[8].address()),
        InstructionAccount::writable(accounts[9].address()),
        InstructionAccount::writable(accounts[if total > 10 { 10 } else { 9 }].address()),
        InstructionAccount::writable(accounts[if total > 11 { 11 } else { 9 }].address()),
    ];

    let instruction = InstructionView {
        program_id: &dex_pinocchio_cpi::raydium_clmm::ID,
        accounts: &instruction_accounts[..total],
        data: &data,
    };

    let views: [&AccountView; 12] = [
        &accounts[0], &accounts[1], &accounts[2], &accounts[3],
        &accounts[4], &accounts[5], &accounts[6], &accounts[7],
        &accounts[8], &accounts[9],
        &accounts[if total > 10 { 10 } else { 9 }],
        &accounts[if total > 11 { 11 } else { 9 }],
    ];

    invoke_signed_with_slice(&instruction, &views[..total], &[])
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

/// Meteora DLMM: swap2 (16 fixed accounts + remaining bin_arrays).
/// Layout: [lb_pair, bin_array_bitmap_extension, reserve_x, reserve_y,
///  user_token_in, user_token_out, token_x_mint, token_y_mint, oracle,
///  host_fee_in, user, token_x_program, token_y_program, memo_program,
///  event_authority, program, ...bin_arrays]
fn swap_meteora_dlmm(accounts: &[AccountView], amount_in: u64) -> ProgramResult {
    use pinocchio::instruction::{InstructionView, InstructionAccount};
    use pinocchio::cpi::invoke_signed_with_slice;

    // Build instruction data: 8-byte discriminator + Swap2Args
    // Swap2Args: amount_in(u64) + min_amount_out(u64) + remaining_accounts_info([u8;32])
    let mut data = [0u8; 8 + 8 + 8 + 32];
    data[0..8].copy_from_slice(&dex_pinocchio_cpi::meteora_dlmm::SWAP2);
    data[8..16].copy_from_slice(&amount_in.to_le_bytes());
    // min_amount_out = 0 (profit verified atomically)
    // data[16..24] already zeroed

    // remaining_accounts_info: Borsh-serialized RemainingAccountsInfo { slices: Vec<_> }
    // Bin arrays are NOT transfer hook accounts — set slices to empty Vec.
    // First 4 bytes = u32 LE length = 0 (already zeroed), remaining 28 bytes = 0.
    // This matches the reference implementation: `&0u32.to_le_bytes()`

    let total = accounts.len().min(24); // 16 fixed + up to 8 bin arrays

    // Build instruction accounts (AccountMeta layout for DLMM swap2).
    // accounts[1] = bin_array_bitmap_extension (placeholder = DLMM program_id)
    // accounts[9] = host_fee_in (placeholder = DLMM program_id)
    // Both are optional — the placeholder tells DLMM they're absent.
    let instruction_accounts: [InstructionAccount; 24] = [
        InstructionAccount::writable(accounts[0].address()),           // [0] lb_pair
        InstructionAccount::readonly(accounts[1].address()),            // [1] bin_array_bitmap_extension (placeholder)
        InstructionAccount::writable(accounts[2].address()),           // [2] reserve_x
        InstructionAccount::writable(accounts[3].address()),           // [3] reserve_y
        InstructionAccount::writable(accounts[if total > 4 { 4 } else { 0 }].address()),  // [4] user_token_in
        InstructionAccount::writable(accounts[if total > 5 { 5 } else { 0 }].address()),  // [5] user_token_out
        InstructionAccount::readonly(accounts[if total > 6 { 6 } else { 0 }].address()),  // [6] token_x_mint
        InstructionAccount::readonly(accounts[if total > 7 { 7 } else { 0 }].address()),  // [7] token_y_mint
        InstructionAccount::writable(accounts[if total > 8 { 8 } else { 0 }].address()),  // [8] oracle
        InstructionAccount::readonly(accounts[if total > 9 { 9 } else { 0 }].address()),  // [9] host_fee_in (placeholder)
        InstructionAccount::readonly_signer(accounts[if total > 10 { 10 } else { 0 }].address()), // [10] user
        InstructionAccount::readonly(accounts[if total > 11 { 11 } else { 0 }].address()), // [11] token_x_program
        InstructionAccount::readonly(accounts[if total > 12 { 12 } else { 0 }].address()), // [12] token_y_program
        InstructionAccount::readonly(accounts[if total > 13 { 13 } else { 0 }].address()), // [13] memo_program
        InstructionAccount::readonly(accounts[if total > 14 { 14 } else { 0 }].address()), // [14] event_authority
        InstructionAccount::readonly(accounts[if total > 15 { 15 } else { 0 }].address()), // [15] program
        // Remaining bin arrays (writable)
        InstructionAccount::writable(accounts[if total > 16 { 16 } else { 0 }].address()),
        InstructionAccount::writable(accounts[if total > 17 { 17 } else { 0 }].address()),
        InstructionAccount::writable(accounts[if total > 18 { 18 } else { 0 }].address()),
        InstructionAccount::writable(accounts[if total > 19 { 19 } else { 0 }].address()),
        InstructionAccount::writable(accounts[if total > 20 { 20 } else { 0 }].address()),
        InstructionAccount::writable(accounts[if total > 21 { 21 } else { 0 }].address()),
        InstructionAccount::writable(accounts[if total > 22 { 22 } else { 0 }].address()),
        InstructionAccount::writable(accounts[if total > 23 { 23 } else { 0 }].address()),
    ];

    let instruction = InstructionView {
        program_id: &dex_pinocchio_cpi::meteora_dlmm::ID,
        accounts: &instruction_accounts[..total],
        data: &data,
    };

    // Build account views — skip bitmap_ext (index 1) and host_fee_in (index 9).
    // These are optional accounts with placeholder pubkey = DLMM program_id.
    // Runtime resolves them via the program account (index 15, same pubkey).
    // This matches the reference implementation pattern.
    let bin_array_start = 16;
    let mut views_vec: [&AccountView; 22] = [&accounts[0]; 22]; // max: 14 fixed + 8 bin arrays
    let mut vi = 0;
    views_vec[vi] = &accounts[0]; vi += 1;  // lb_pair
    // skip accounts[1] = bitmap_ext
    views_vec[vi] = &accounts[2]; vi += 1;  // reserve_x
    views_vec[vi] = &accounts[3]; vi += 1;  // reserve_y
    if total > 4 { views_vec[vi] = &accounts[4]; vi += 1; }  // user_token_in
    if total > 5 { views_vec[vi] = &accounts[5]; vi += 1; }  // user_token_out
    if total > 6 { views_vec[vi] = &accounts[6]; vi += 1; }  // token_x_mint
    if total > 7 { views_vec[vi] = &accounts[7]; vi += 1; }  // token_y_mint
    if total > 8 { views_vec[vi] = &accounts[8]; vi += 1; }  // oracle
    // skip accounts[9] = host_fee_in
    if total > 10 { views_vec[vi] = &accounts[10]; vi += 1; } // user (signer)
    if total > 11 { views_vec[vi] = &accounts[11]; vi += 1; } // token_x_program
    if total > 12 { views_vec[vi] = &accounts[12]; vi += 1; } // token_y_program
    if total > 13 { views_vec[vi] = &accounts[13]; vi += 1; } // memo_program
    if total > 14 { views_vec[vi] = &accounts[14]; vi += 1; } // event_authority
    if total > 15 { views_vec[vi] = &accounts[15]; vi += 1; } // program
    // bin arrays
    let mut bi = bin_array_start;
    while bi < total {
        views_vec[vi] = &accounts[bi]; vi += 1; bi += 1;
    }

    invoke_signed_with_slice(&instruction, &views_vec[..vi], &[])
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
    // params layout: [0..8] amount_in (u64 LE), [8..16] minimum_amount_out (u64 LE = 0), [16..31] unused
    // minimum_amount_out = 0 is safe — profit verified atomically at transaction level
    let mut params = [0u8; 32];
    params[..8].copy_from_slice(&amount_in.to_le_bytes());
    let args = dex_pinocchio_cpi::meteora_damm_v2::SwapArgs { params };
    dex_pinocchio_cpi::meteora_damm_v2::swap(&swap_accounts, &args, &[])
}

/// Orca Whirlpool: swap_v2 (15 accounts).
/// Layout: [token_program_a, token_program_b, memo_program, token_authority,
///  whirlpool, token_mint_a, token_mint_b, token_owner_account_a, token_vault_a,
///  token_owner_account_b, token_vault_b, tick_array_0, tick_array_1, tick_array_2, oracle]
fn swap_orca_whirlpool(accounts: &[AccountView], amount_in: u64, a_to_b: bool) -> ProgramResult {
    let swap_accounts = dex_pinocchio_cpi::whirlpool::Swapv2Accounts {
        token_program_a: &accounts[0],
        token_program_b: &accounts[1],
        memo_program: &accounts[2],
        token_authority: &accounts[3],
        whirlpool: &accounts[4],
        token_mint_a: &accounts[5],
        token_mint_b: &accounts[6],
        token_owner_account_a: &accounts[7],
        token_vault_a: &accounts[8],
        token_owner_account_b: &accounts[9],
        token_vault_b: &accounts[10],
        tick_array0: &accounts[11],
        tick_array1: &accounts[12],
        tick_array2: &accounts[13],
        oracle: &accounts[14],
    };
    // sqrt_price_limit: 0 means no limit (use MIN_SQRT_PRICE or MAX_SQRT_PRICE)
    // For a_to_b: price goes down, use MIN_SQRT_PRICE_X64 = 4295048016
    // For b_to_a: price goes up, use MAX_SQRT_PRICE_X64 = 79226673515401279992447579055
    let sqrt_price_limit: u128 = if a_to_b {
        4295048016 // MIN_SQRT_PRICE_X64
    } else {
        79226673515401279992447579055 // MAX_SQRT_PRICE_X64
    };
    let args = dex_pinocchio_cpi::whirlpool::Swapv2Args {
        amount: amount_in,
        other_amount_threshold: 0,
        sqrt_price_limit,
        amount_specified_is_input: true,
        a_to_b,
        remaining_accounts_info: None,
    };
    dex_pinocchio_cpi::whirlpool::swap_v2(&swap_accounts, &args, &[])
}
