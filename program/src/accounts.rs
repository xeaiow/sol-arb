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
    MeteoraDlmm = 7,
    OrcaWhirlpool = 8,
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
            7 => Ok(Self::MeteoraDlmm),
            8 => Ok(Self::OrcaWhirlpool),
            _ => Err(ProgramError::InvalidInstructionData),
        }
    }

    /// Base number of accounts for CPI swap on this DEX (excluding CLMM extra tick arrays)
    pub fn base_pool_account_count(&self) -> usize {
        match self {
            DexType::RaydiumAmmV4 => 9,
            DexType::RaydiumCpmm => 14,
            DexType::RaydiumClmm => 10, // base only; extra tick arrays added via extra_accounts
            DexType::PumpFun => 16,
            DexType::PumpSwap => 24, // 19 formal + 5 remaining (vol×2, fee×2, pool_v2)
            DexType::Bonk => 17,
            DexType::MeteoraDammV2 => 14,
            DexType::MeteoraDlmm => 16, // 16 fixed + remaining bin_arrays via extra_accounts
            DexType::OrcaWhirlpool => 15, // base only; like CLMM, no extra tick arrays in base
        }
    }
}

/// Parsed hop info
pub struct HopInfo {
    pub dex_type: DexType,
    pub is_a_to_b: bool,
    pub is_token_2022: bool,
    /// Number of extra accounts beyond base (CLMM: 0-2 extra unique tick arrays)
    pub extra_accounts: u8,
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
        extra_accounts: 0,
    }
}

impl SwapInstruction {
    /// Parse instruction data (WITHOUT discriminator byte — already stripped).
    /// 2-hop: 15 bytes, 3-hop: 17 bytes, 4-hop: 19 bytes
    ///
    /// Flags layout:
    ///   data[2] (buy/sell flags):
    ///     bit0 = buy_a_to_b
    ///     bit1 = sell_a_to_b
    ///     bit2 = buy_2022
    ///     bit3 = sell_2022
    ///     bit4-5 = buy extra_accounts (0-3, used for CLMM extra tick arrays)
    ///     bit6-7 = sell extra_accounts (0-3)
    ///   data[4]/data[6] (mid hop flags):
    ///     bit0 = a_to_b
    ///     bit1 = 2022
    ///     bit2-3 = extra_accounts (0-3)
    pub fn parse(data: &[u8], hop_count: u8) -> Result<Self, ProgramError> {
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

        let mut hops = [default_hop(), default_hop(), default_hop(), default_hop()];

        // First hop (buy)
        hops[0] = HopInfo {
            dex_type: buy_dex,
            is_a_to_b: flags & 1 == 1,
            is_token_2022: (flags >> 2) & 1 == 1,
            extra_accounts: (flags >> 4) & 3,
        };

        let amount_offset = match hop_count {
            2 => {
                hops[1] = HopInfo {
                    dex_type: sell_dex,
                    is_a_to_b: (flags >> 1) & 1 == 1,
                    is_token_2022: (flags >> 3) & 1 == 1,
                    extra_accounts: (flags >> 6) & 3,
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
                    extra_accounts: (mid_flags >> 2) & 3,
                };
                hops[2] = HopInfo {
                    dex_type: sell_dex,
                    is_a_to_b: (flags >> 1) & 1 == 1,
                    is_token_2022: (flags >> 3) & 1 == 1,
                    extra_accounts: (flags >> 6) & 3,
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
                    extra_accounts: (mid1_flags >> 2) & 3,
                };
                hops[2] = HopInfo {
                    dex_type: mid2_dex,
                    is_a_to_b: mid2_flags & 1 == 1,
                    is_token_2022: (mid2_flags >> 1) & 1 == 1,
                    extra_accounts: (mid2_flags >> 2) & 3,
                };
                hops[3] = HopInfo {
                    dex_type: sell_dex,
                    is_a_to_b: (flags >> 1) & 1 == 1,
                    is_token_2022: (flags >> 3) & 1 == 1,
                    extra_accounts: (flags >> 6) & 3,
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

/// Calculate the starting index of pool accounts for a given hop.
/// Uses base_pool_account_count + extra_accounts for dynamic sizing (CLMM tick arrays).
pub fn pool_accounts_start(
    hop_count: u8,
    hop_index: u8,
    hops: &[HopInfo; 4],
) -> usize {
    let mut offset = HEADER_SIZE;
    offset += (hop_count as usize - 1) * INTERMEDIATE_ACCOUNTS_PER_TOKEN;
    for i in 0..hop_index as usize {
        offset += hops[i].dex_type.base_pool_account_count() + hops[i].extra_accounts as usize;
    }
    offset
}
