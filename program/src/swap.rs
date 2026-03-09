//! Multi-hop swap execution — implemented in Task 3

use pinocchio::{AccountView, Address, ProgramResult, error::ProgramError};

/// Execute a multi-hop swap with the given number of legs.
///
/// Placeholder: returns `InvalidInstructionData` until real logic is wired up.
pub fn execute(
    _program_id: &Address,
    _accounts: &[AccountView],
    _data: &[u8],
    _hops: u8,
) -> ProgramResult {
    Err(ProgramError::InvalidInstructionData)
}
