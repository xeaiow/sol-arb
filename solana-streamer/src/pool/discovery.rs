use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use crate::streaming::event_parser::DexEvent;

use super::state::DexType;

/// WSOL mint address (used as mint_b for PumpFun bonding curve pools)
const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

#[derive(Debug, Clone)]
pub struct DiscoveredPool {
    pub address: Pubkey,
    pub dex_type: DexType,
    pub mint_a: Option<Pubkey>,
    pub mint_b: Option<Pubkey>,
}

/// Extract pool discovery info from transaction events (swap/create).
/// Returns None for config/position/non-pool events.
pub fn discover_pool(event: &DexEvent) -> Option<DiscoveredPool> {
    match event {
        // ── Raydium AMM V4 ──────────────────────────────────────────
        DexEvent::RaydiumAmmV4SwapEvent(e) => Some(DiscoveredPool {
            address: e.amm,
            dex_type: DexType::RaydiumAmmV4,
            mint_a: None,
            mint_b: None,
        }),
        DexEvent::RaydiumAmmV4Initialize2Event(e) => Some(DiscoveredPool {
            address: e.amm,
            dex_type: DexType::RaydiumAmmV4,
            mint_a: Some(e.coin_mint),
            mint_b: Some(e.pc_mint),
        }),

        // ── Raydium CPMM ────────────────────────────────────────────
        DexEvent::RaydiumCpmmSwapEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::RaydiumCpmm,
            mint_a: Some(e.input_token_mint),
            mint_b: Some(e.output_token_mint),
        }),
        DexEvent::RaydiumCpmmInitializeEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::RaydiumCpmm,
            mint_a: Some(e.token_0_mint),
            mint_b: Some(e.token_1_mint),
        }),

        // ── Raydium CLMM ────────────────────────────────────────────
        DexEvent::RaydiumClmmSwapEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::RaydiumClmm,
            mint_a: None,
            mint_b: None,
        }),
        DexEvent::RaydiumClmmSwapV2Event(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::RaydiumClmm,
            mint_a: Some(e.input_vault_mint),
            mint_b: Some(e.output_vault_mint),
        }),
        DexEvent::RaydiumClmmCreatePoolEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::RaydiumClmm,
            mint_a: Some(e.token_mint0),
            mint_b: Some(e.token_mint1),
        }),

        // ── PumpFun ─────────────────────────────────────────────────
        DexEvent::PumpFunTradeEvent(e) => {
            let wsol = Pubkey::from_str(WSOL_MINT).unwrap();
            Some(DiscoveredPool {
                address: e.bonding_curve,
                dex_type: DexType::PumpFun,
                mint_a: Some(e.mint),
                mint_b: Some(wsol),
            })
        }
        DexEvent::PumpFunCreateTokenEvent(e) => {
            let wsol = Pubkey::from_str(WSOL_MINT).unwrap();
            Some(DiscoveredPool {
                address: e.bonding_curve,
                dex_type: DexType::PumpFun,
                mint_a: Some(e.mint),
                mint_b: Some(wsol),
            })
        }
        DexEvent::PumpFunCreateV2TokenEvent(e) => {
            let wsol = Pubkey::from_str(WSOL_MINT).unwrap();
            Some(DiscoveredPool {
                address: e.bonding_curve,
                dex_type: DexType::PumpFun,
                mint_a: Some(e.mint),
                mint_b: Some(wsol),
            })
        }

        // ── PumpSwap ────────────────────────────────────────────────
        DexEvent::PumpSwapBuyEvent(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::PumpSwap,
            mint_a: Some(e.base_mint),
            mint_b: Some(e.quote_mint),
        }),
        DexEvent::PumpSwapSellEvent(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::PumpSwap,
            mint_a: Some(e.base_mint),
            mint_b: Some(e.quote_mint),
        }),
        DexEvent::PumpSwapCreatePoolEvent(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::PumpSwap,
            mint_a: Some(e.base_mint),
            mint_b: Some(e.quote_mint),
        }),

        // ── Bonk ────────────────────────────────────────────────────
        DexEvent::BonkTradeEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::Bonk,
            mint_a: Some(e.base_token_mint),
            mint_b: Some(e.quote_token_mint),
        }),
        DexEvent::BonkPoolCreateEvent(e) => Some(DiscoveredPool {
            address: e.pool_state,
            dex_type: DexType::Bonk,
            mint_a: None,
            mint_b: None,
        }),

        // ── Meteora DAMM v2 ─────────────────────────────────────────
        DexEvent::MeteoraDammV2SwapEvent(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::MeteoraDammV2,
            mint_a: Some(e.token_a_mint),
            mint_b: Some(e.token_b_mint),
        }),
        DexEvent::MeteoraDammV2Swap2Event(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::MeteoraDammV2,
            mint_a: Some(e.token_a_mint),
            mint_b: Some(e.token_b_mint),
        }),
        DexEvent::MeteoraDammV2InitializePoolEvent(e) => Some(DiscoveredPool {
            address: e.pool,
            dex_type: DexType::MeteoraDammV2,
            mint_a: Some(e.token_a_mint),
            mint_b: Some(e.token_b_mint),
        }),

        // ── Meteora DLMM ───────────────────────────────────────────
        DexEvent::MeteoraDlmmSwap2Event(e) => Some(DiscoveredPool {
            address: e.lb_pair,
            dex_type: DexType::MeteoraDlmm,
            mint_a: Some(e.token_x_mint),
            mint_b: Some(e.token_y_mint),
        }),

        // ── Orca Whirlpool ───────────────────────────────────────
        DexEvent::OrcaWhirlpoolSwapV2Event(e) => Some(DiscoveredPool {
            address: e.whirlpool,
            dex_type: DexType::OrcaWhirlpool,
            mint_a: Some(e.token_mint_a),
            mint_b: Some(e.token_mint_b),
        }),

        // Non-pool events (config, position, account, etc.)
        _ => None,
    }
}
