use pinocchio::{
    AccountView, ProgramResult,
    error::ProgramError,
};

/// MarginFi flash borrow (0% fee)
/// Accounts: [marginfi_program, bank, bank_vault]
pub fn flash_borrow(
    accounts: &[AccountView],
    amount: u64,
) -> ProgramResult {
    if accounts.len() < 3 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    // TODO: CPI to MarginFi lending_pool_start_flashloan
    // Discriminator and full account layout depend on MarginFi's program interface
    let _ = (accounts, amount);
    Ok(())
}

/// MarginFi flash repay (0% fee, repay = borrow amount)
/// Accounts: [marginfi_program, bank, bank_vault]
pub fn flash_repay(
    accounts: &[AccountView],
    amount: u64,
) -> ProgramResult {
    if accounts.len() < 3 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    // TODO: CPI to MarginFi lending_pool_end_flashloan
    let _ = (accounts, amount);
    Ok(())
}
