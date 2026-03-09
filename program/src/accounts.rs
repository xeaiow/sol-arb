use pinocchio::error::ProgramError;

/// DEX type IDs (must match off-chain DexType enum in engine)
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DexType {
    RaydiumAmmV4 = 0,
    RaydiumCpmm = 1,
    RaydiumClmm = 2,
    PumpFun = 3,
    PumpSwap = 4,
    Bonk = 5,
    MeteoraDammV2 = 6,
}

impl DexType {
    pub fn from_u8(v: u8) -> Result<Self, ProgramError> {
        match v {
            0 => Ok(Self::RaydiumAmmV4),
            1 => Ok(Self::RaydiumCpmm),
            2 => Ok(Self::RaydiumClmm),
            3 => Ok(Self::PumpFun),
            4 => Ok(Self::PumpSwap),
            5 => Ok(Self::Bonk),
            6 => Ok(Self::MeteoraDammV2),
            _ => Err(ProgramError::InvalidInstructionData),
        }
    }

    /// Number of accounts required for CPI swap on this DEX
    pub fn pool_account_count(&self) -> usize {
        match self {
            DexType::RaydiumAmmV4 => 8,
            DexType::RaydiumCpmm => 13,
            DexType::RaydiumClmm => 14,
            DexType::PumpFun => 16,
            DexType::PumpSwap => 23,
            DexType::Bonk => 17,
            DexType::MeteoraDammV2 => 14,
        }
    }
}

/// Parsed hop info
pub struct HopInfo {
    pub dex_type: DexType,
    pub is_a_to_b: bool,
    pub is_token_2022: bool,
}

/// Parsed instruction data (discriminator already stripped by lib.rs)
pub struct SwapInstruction {
    pub hop_count: u8,
    pub hops: [HopInfo; 4],
    pub amount_in: u64,
    pub min_profit: u32,
}

/// Default HopInfo for array initialization
fn default_hop() -> HopInfo {
    HopInfo {
        dex_type: DexType::RaydiumAmmV4,
        is_a_to_b: false,
        is_token_2022: false,
    }
}

impl SwapInstruction {
    /// Parse instruction data (WITHOUT discriminator byte — already stripped).
    /// 2-hop: 15 bytes, 3-hop: 17 bytes, 4-hop: 19 bytes
    pub fn parse(data: &[u8], hop_count: u8) -> Result<Self, ProgramError> {
        // data layout:
        // [0] buy_dex_type
        // [1] sell_dex_type
        // [2] flags: bit0=buy_a_to_b, bit1=sell_a_to_b, bit2=buy_2022, bit3=sell_2022, bit4=flashloan
        // For 3-hop: [3] mid_dex, [4] mid_flags (bit0=a_to_b, bit1=2022)
        // For 4-hop: [3] mid1_dex, [4] mid1_flags, [5] mid2_dex, [6] mid2_flags
        // Last 12 bytes: amount_in(u64 LE) + min_profit(u32 LE)

        let expected_len = match hop_count {
            2 => 15, // 3 + 8 + 4
            3 => 17, // 3 + 2 + 8 + 4
            4 => 19, // 3 + 4 + 8 + 4
            _ => return Err(ProgramError::InvalidInstructionData),
        };
        if data.len() < expected_len {
            return Err(ProgramError::InvalidInstructionData);
        }

        let buy_dex = DexType::from_u8(data[0])?;
        let sell_dex = DexType::from_u8(data[1])?;
        let flags = data[2];
        // flags: bit0=buy_a_to_b, bit1=sell_a_to_b, bit2=buy_2022, bit3=sell_2022

        let mut hops = [default_hop(), default_hop(), default_hop(), default_hop()];

        // First hop (buy)
        hops[0] = HopInfo {
            dex_type: buy_dex,
            is_a_to_b: flags & 1 == 1,
            is_token_2022: (flags >> 2) & 1 == 1,
        };

        let amount_offset = match hop_count {
            2 => {
                hops[1] = HopInfo {
                    dex_type: sell_dex,
                    is_a_to_b: (flags >> 1) & 1 == 1,
                    is_token_2022: (flags >> 3) & 1 == 1,
                };
                3
            }
            3 => {
                let mid_dex = DexType::from_u8(data[3])?;
                let mid_flags = data[4];
                hops[1] = HopInfo {
                    dex_type: mid_dex,
                    is_a_to_b: mid_flags & 1 == 1,
                    is_token_2022: (mid_flags >> 1) & 1 == 1,
                };
                hops[2] = HopInfo {
                    dex_type: sell_dex,
                    is_a_to_b: (flags >> 1) & 1 == 1,
                    is_token_2022: (flags >> 3) & 1 == 1,
                };
                5
            }
            4 => {
                let mid1_dex = DexType::from_u8(data[3])?;
                let mid1_flags = data[4];
                let mid2_dex = DexType::from_u8(data[5])?;
                let mid2_flags = data[6];
                hops[1] = HopInfo {
                    dex_type: mid1_dex,
                    is_a_to_b: mid1_flags & 1 == 1,
                    is_token_2022: (mid1_flags >> 1) & 1 == 1,
                };
                hops[2] = HopInfo {
                    dex_type: mid2_dex,
                    is_a_to_b: mid2_flags & 1 == 1,
                    is_token_2022: (mid2_flags >> 1) & 1 == 1,
                };
                hops[3] = HopInfo {
                    dex_type: sell_dex,
                    is_a_to_b: (flags >> 1) & 1 == 1,
                    is_token_2022: (flags >> 3) & 1 == 1,
                };
                7
            }
            _ => return Err(ProgramError::InvalidInstructionData),
        };

        // amount_in: u64 LE at amount_offset
        let mut amt_bytes = [0u8; 8];
        amt_bytes.copy_from_slice(&data[amount_offset..amount_offset + 8]);
        let amount_in = u64::from_le_bytes(amt_bytes);

        // min_profit: u32 LE
        let mut profit_bytes = [0u8; 4];
        profit_bytes.copy_from_slice(&data[amount_offset + 8..amount_offset + 12]);
        let min_profit = u32::from_le_bytes(profit_bytes);

        Ok(SwapInstruction {
            hop_count,
            hops,
            amount_in,
            min_profit,
        })
    }
}

// ── Account layout constants ──

/// Fixed header size (8 accounts)
pub const HEADER_SIZE: usize = 8;
pub const ACCT_PAYER: usize = 0;
pub const ACCT_BASE_MINT: usize = 1;
pub const ACCT_USER_BASE_ATA: usize = 2;
pub const ACCT_FEE_COLLECTOR: usize = 3;
pub const ACCT_SPL_TOKEN: usize = 4;
pub const ACCT_TOKEN_2022: usize = 5;
pub const ACCT_ATA_PROGRAM: usize = 6;
pub const ACCT_SYSTEM: usize = 7;

/// Per-intermediate-token: 3 accounts (mint, token_program, user_ata)
pub const INTERMEDIATE_ACCOUNTS_PER_TOKEN: usize = 3;

/// Calculate the starting index of pool accounts for a given hop
pub fn pool_accounts_start(
    hop_count: u8,
    hop_index: u8,
    hops: &[HopInfo; 4],
) -> usize {
    let mut offset = HEADER_SIZE;
    offset += (hop_count as usize - 1) * INTERMEDIATE_ACCOUNTS_PER_TOKEN;
    for i in 0..hop_index as usize {
        offset += hops[i].dex_type.pool_account_count();
    }
    offset
}
