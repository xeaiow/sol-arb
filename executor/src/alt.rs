use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    message::AddressLookupTableAccount,
    pubkey::Pubkey,
};

pub struct Tier0Alt {
    pub address: Pubkey,
    pub account: AddressLookupTableAccount,
}

impl Tier0Alt {
    /// Load ALT from RPC at startup, cache in memory
    pub async fn load(rpc: &RpcClient, address: Pubkey) -> Result<Self> {
        let account_data = rpc.get_account(&address).await?;
        let addresses = Self::deserialize_alt(&account_data.data)?;
        let lookup_table = AddressLookupTableAccount {
            key: address,
            addresses,
        };
        log::info!(
            "ALT loaded: {} ({} entries)",
            address,
            lookup_table.addresses.len()
        );
        Ok(Self {
            address,
            account: lookup_table,
        })
    }

    fn deserialize_alt(data: &[u8]) -> Result<Vec<Pubkey>> {
        // ALT data: 56 bytes header + 32 bytes per address
        if data.len() < 56 {
            anyhow::bail!("ALT data too short: {} bytes", data.len());
        }
        let addresses_data = &data[56..];
        let count = addresses_data.len() / 32;
        let mut addresses = Vec::with_capacity(count);
        for i in 0..count {
            let start = i * 32;
            let pubkey = Pubkey::try_from(&addresses_data[start..start + 32])
                .map_err(|e| anyhow::anyhow!("Invalid pubkey in ALT: {:?}", e))?;
            addresses.push(pubkey);
        }
        Ok(addresses)
    }
}
