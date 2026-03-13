// Meteora DLMM instruction and account type definitions

/// Meteora DLMM instruction types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeteoraDlmmInstructionType {
    Swap2,
}

/// Meteora DLMM account types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeteoraDlmmAccountType {
    LbPair,
}
