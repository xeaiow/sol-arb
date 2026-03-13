// Orca Whirlpool instruction and account type definitions

/// Orca Whirlpool instruction types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrcaWhirlpoolInstructionType {
    SwapV2,
}

/// Orca Whirlpool account types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrcaWhirlpoolAccountType {
    Whirlpool,
    TickArray,
}
