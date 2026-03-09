use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    sysvar,
};
use std::collections::HashMap;
use std::str::FromStr;

/// MarginFi V2 program ID (mainnet)
pub const MARGINFI_PROGRAM_ID: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";

/// SPL Token program ID (for borrow/repay accounts)
const SPL_TOKEN_PROGRAM: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Discriminators: SHA256("global:<name>")[0..8]
const START_FLASHLOAN_DISC: [u8; 8] = [14, 131, 33, 220, 81, 186, 180, 107];
const END_FLASHLOAN_DISC: [u8; 8] = [105, 124, 201, 106, 153, 2, 8, 156];
// lending_account_borrow: SHA256("global:lending_account_borrow")[0..8]
const BORROW_DISC: [u8; 8] = [228, 253, 131, 202, 235, 176, 183, 247];
// lending_account_repay: SHA256("global:lending_account_repay")[0..8]
const REPAY_DISC: [u8; 8] = [79, 209, 172, 177, 222, 62, 35, 27];

pub struct BankInfo {
    pub address: Pubkey,
    pub oracle: Pubkey,
    pub vault: Pubkey,
    pub vault_authority: Pubkey,
}

pub struct MarginFiState {
    pub program_id: Pubkey,
    pub group: Pubkey,
    pub account: Pubkey,
    pub banks: HashMap<Pubkey, BankInfo>, // mint → bank info
}

impl MarginFiState {
    /// Initialize by querying all MarginFi state from RPC.
    /// Called once at executor startup.
    pub async fn init(rpc: &RpcClient, payer: &Pubkey) -> Result<Self> {
        let program_id = Pubkey::from_str(MARGINFI_PROGRAM_ID)?;

        // Query MarginFi group (well-known mainnet address)
        // TODO: Query actual group account from on-chain
        let group = Pubkey::default();

        // Query/create marginfi_account for payer
        // TODO: Derive PDA or query existing account
        let account = Pubkey::default();

        // Query all banks, index by mint
        // TODO: Use getProgramAccounts with memcmp filter for group
        let banks = HashMap::new();

        log::info!(
            "MarginFi state initialized: program={}, group={}, account={}, banks={}",
            program_id, group, account, banks.len()
        );

        let _ = (rpc, payer); // will be used when TODOs are filled

        Ok(Self {
            program_id,
            group,
            account,
            banks,
        })
    }

    /// Build start_flashloan instruction
    pub fn build_start_flashloan_ix(&self, authority: &Pubkey, end_index: u64) -> Instruction {
        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&START_FLASHLOAN_DISC);
        data.extend_from_slice(&end_index.to_le_bytes());

        Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new(self.account, false),
                AccountMeta::new_readonly(*authority, true),
                AccountMeta::new_readonly(sysvar::instructions::ID, false),
            ],
            data,
        }
    }

    /// Build end_flashloan instruction
    pub fn build_end_flashloan_ix(
        &self,
        authority: &Pubkey,
        bank_oracle_pairs: &[(Pubkey, Pubkey)],
    ) -> Instruction {
        let data = END_FLASHLOAN_DISC.to_vec();

        let mut accounts = vec![
            AccountMeta::new(self.account, false),
            AccountMeta::new_readonly(*authority, true),
        ];
        for (bank, oracle) in bank_oracle_pairs {
            accounts.push(AccountMeta::new(*bank, false));
            accounts.push(AccountMeta::new_readonly(*oracle, false));
        }

        Instruction {
            program_id: self.program_id,
            accounts,
            data,
        }
    }

    /// Build borrow instruction
    pub fn build_borrow_ix(
        &self,
        authority: &Pubkey,
        mint: &Pubkey,
        dest_ata: &Pubkey,
        amount: u64,
    ) -> Result<Instruction> {
        let bank = self
            .banks
            .get(mint)
            .ok_or_else(|| anyhow::anyhow!("No bank for mint {}", mint))?;

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&BORROW_DISC);
        data.extend_from_slice(&amount.to_le_bytes());

        Ok(Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new_readonly(self.group, false),
                AccountMeta::new(self.account, false),
                AccountMeta::new_readonly(*authority, true),
                AccountMeta::new(bank.address, false),
                AccountMeta::new(*dest_ata, false),
                AccountMeta::new_readonly(bank.vault_authority, false),
                AccountMeta::new(bank.vault, false),
                AccountMeta::new_readonly(SPL_TOKEN_PROGRAM, false),
            ],
            data,
        })
    }

    /// Build repay instruction
    pub fn build_repay_ix(
        &self,
        authority: &Pubkey,
        mint: &Pubkey,
        source_ata: &Pubkey,
        amount: u64,
    ) -> Result<Instruction> {
        let bank = self
            .banks
            .get(mint)
            .ok_or_else(|| anyhow::anyhow!("No bank for mint {}", mint))?;

        let mut data = Vec::with_capacity(16);
        data.extend_from_slice(&REPAY_DISC);
        data.extend_from_slice(&amount.to_le_bytes());

        Ok(Instruction {
            program_id: self.program_id,
            accounts: vec![
                AccountMeta::new_readonly(self.group, false),
                AccountMeta::new(self.account, false),
                AccountMeta::new_readonly(*authority, true),
                AccountMeta::new(bank.address, false),
                AccountMeta::new(*source_ata, false),
                AccountMeta::new(bank.vault, false),
                AccountMeta::new_readonly(SPL_TOKEN_PROGRAM, false),
            ],
            data,
        })
    }
}
