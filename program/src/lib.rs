#![no_std]

mod accounts;
mod flashloan;
mod swap;

use pinocchio::{
    AccountView, Address, ProgramResult,
    error::ProgramError,
};

#[cfg(not(feature = "no-entrypoint"))]
mod entrypoint {
    use pinocchio::{AccountView, Address, ProgramResult, entrypoint};

    entrypoint!(process_instruction);

    pub fn process_instruction(
        program_id: &Address,
        accounts: &[AccountView],
        instruction_data: &[u8],
    ) -> ProgramResult {
        crate::process_instruction(program_id, accounts, instruction_data)
    }
}

// ── Instruction discriminators ──────────────────────────────────────────────

pub const SWAP_2HOP: u8 = 0;
pub const SWAP_3HOP: u8 = 1;
pub const SWAP_4HOP: u8 = 2;

// ── Dispatch ────────────────────────────────────────────────────────────────

pub fn process_instruction(
    program_id: &Address,
    accounts: &[AccountView],
    instruction_data: &[u8],
) -> ProgramResult {
    let (&discriminator, data) = instruction_data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;

    match discriminator {
        SWAP_2HOP => swap::execute(program_id, accounts, data, 2),
        SWAP_3HOP => swap::execute(program_id, accounts, data, 3),
        SWAP_4HOP => swap::execute(program_id, accounts, data, 4),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}
