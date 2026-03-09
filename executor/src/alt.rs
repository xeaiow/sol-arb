use solana_sdk::pubkey::Pubkey;

pub struct Tier0Alt {
    pub address: Pubkey,
}

impl Tier0Alt {
    pub fn new(address: Pubkey) -> Self {
        Self { address }
    }
}
