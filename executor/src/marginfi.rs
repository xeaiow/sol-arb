use anyhow::Result;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::RpcProgramAccountsConfig,
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    sysvar,
};
use std::collections::HashMap;

/// MarginFi V2 program ID (mainnet)
const MARGINFI_PROGRAM: Pubkey = solana_sdk::pubkey!("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA");

/// Well-known mainnet MarginFi group
const MARGINFI_GROUP: Pubkey = solana_sdk::pubkey!("4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8");

/// Bank account data size (8 discriminator + 1856 struct)
const BANK_DATA_SIZE: usize = 1864;

/// Byte offsets within Bank account data
const BANK_MINT_OFFSET: usize = 8;
const BANK_GROUP_OFFSET: usize = 41;
const BANK_VAULT_OFFSET: usize = 112;
const BANK_VAULT_AUTH_BUMP_OFFSET: usize = 145;
const BANK_ORACLE_KEY_OFFSET: usize = 610;

/// MarginfiAccount data: authority at offset 40, group at offset 8
const ACCOUNT_GROUP_OFFSET: usize = 8;
const ACCOUNT_AUTHORITY_OFFSET: usize = 40;

/// SPL Token program ID (for borrow/repay accounts)
const SPL_TOKEN_PROGRAM: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Discriminators: SHA256("global:<name>")[0..8]
const START_FLASHLOAN_DISC: [u8; 8] = [14, 131, 33, 220, 81, 186, 180, 107];
const END_FLASHLOAN_DISC: [u8; 8] = [105, 124, 201, 106, 153, 2, 8, 156];
// lending_account_borrow: SHA256("global:lending_account_borrow")[0..8]
const BORROW_DISC: [u8; 8] = [4, 126, 116, 53, 48, 5, 212, 31];
// lending_account_repay: SHA256("global:lending_account_repay")[0..8]
const REPAY_DISC: [u8; 8] = [79, 209, 172, 177, 222, 51, 173, 151];

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
    /// Called once at executor startup; all data pre-loaded for zero hot-path queries.
    pub async fn init(rpc: &RpcClient, payer: &Pubkey) -> Result<Self> {
        let program_id = MARGINFI_PROGRAM;
        let group = MARGINFI_GROUP;

        // Query payer's marginfi_account via getProgramAccounts (filter by group + authority)
        let account = Self::find_marginfi_account(rpc, &group, payer).await?;

        // Query all banks for this group, index by mint
        let banks = Self::load_all_banks(rpc, &program_id, &group).await?;

        log::info!(
            "MarginFi state initialized: program={}, group={}, account={}, banks={}",
            program_id, group, account, banks.len()
        );

        Ok(Self {
            program_id,
            group,
            account,
            banks,
        })
    }

    /// Find the payer's MarginFi account by querying on-chain with memcmp filters.
    async fn find_marginfi_account(
        rpc: &RpcClient,
        group: &Pubkey,
        authority: &Pubkey,
    ) -> Result<Pubkey> {
        let filters = vec![
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                ACCOUNT_GROUP_OFFSET,
                group.to_bytes().to_vec(),
            )),
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                ACCOUNT_AUTHORITY_OFFSET,
                authority.to_bytes().to_vec(),
            )),
        ];

        let config = RpcProgramAccountsConfig {
            filters: Some(filters),
            ..Default::default()
        };

        #[allow(deprecated)]
        let accounts = rpc
            .get_program_accounts_with_config(&MARGINFI_PROGRAM, config)
            .await?;

        let (pubkey, _) = accounts
            .into_iter()
            .next()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No MarginFi account found for authority {}. \
                     Create one at app.marginfi.com first.",
                    authority
                )
            })?;

        Ok(pubkey)
    }

    /// Load all banks for a group, deserialize key fields, index by mint.
    async fn load_all_banks(
        rpc: &RpcClient,
        program_id: &Pubkey,
        group: &Pubkey,
    ) -> Result<HashMap<Pubkey, BankInfo>> {
        let filters = vec![
            RpcFilterType::DataSize(BANK_DATA_SIZE as u64),
            RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                BANK_GROUP_OFFSET,
                group.to_bytes().to_vec(),
            )),
        ];

        let config = RpcProgramAccountsConfig {
            filters: Some(filters),
            ..Default::default()
        };

        #[allow(deprecated)]
        let accounts = rpc
            .get_program_accounts_with_config(program_id, config)
            .await?;

        let mut banks = HashMap::with_capacity(accounts.len());

        for (bank_address, account_data) in &accounts {
            let data = &account_data.data;
            if data.len() < BANK_DATA_SIZE {
                continue;
            }

            let mint = Pubkey::try_from(&data[BANK_MINT_OFFSET..BANK_MINT_OFFSET + 32])
                .map_err(|e| anyhow::anyhow!("Invalid mint in bank {}: {:?}", bank_address, e))?;
            let vault = Pubkey::try_from(&data[BANK_VAULT_OFFSET..BANK_VAULT_OFFSET + 32])
                .map_err(|e| anyhow::anyhow!("Invalid vault in bank {}: {:?}", bank_address, e))?;
            let oracle = Pubkey::try_from(&data[BANK_ORACLE_KEY_OFFSET..BANK_ORACLE_KEY_OFFSET + 32])
                .map_err(|e| anyhow::anyhow!("Invalid oracle in bank {}: {:?}", bank_address, e))?;

            // Derive liquidity_vault_authority PDA from bank address + stored bump
            let vault_auth_bump = data[BANK_VAULT_AUTH_BUMP_OFFSET];
            let vault_authority = Pubkey::create_program_address(
                &[
                    b"liquidity_vault_auth",
                    bank_address.as_ref(),
                    &[vault_auth_bump],
                ],
                program_id,
            )
            .map_err(|e| {
                anyhow::anyhow!("Failed to derive vault authority for bank {}: {:?}", bank_address, e)
            })?;

            banks.insert(
                mint,
                BankInfo {
                    address: *bank_address,
                    oracle,
                    vault,
                    vault_authority,
                },
            );
        }

        Ok(banks)
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
