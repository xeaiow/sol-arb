//! Verification tool: compare off-chain DAMM V2 quotes with on-chain data.
//!
//! Usage: cargo run --bin verify_damm_v2 [pool_address]
//!
//! Default pool: 8Pm2kZpnxD3hoMmt4bjStX2Pw2Z9abpbHzZxMPqxPmie (SOL/USDC)

use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

const RPC_URL: &str = "https://beta.helius-rpc.com/?api-key=89ed37ec-971c-48e0-99db-921d568354e6";
const DEFAULT_POOL: &str = "8Pm2kZpnxD3hoMmt4bjStX2Pw2Z9abpbHzZxMPqxPmie";

/// DAMM V2 pool account layout offsets (after 8-byte Anchor discriminator)
/// All offsets below are relative to data[0] (i.e., data[8] in raw account)
mod offsets {
    // pool_fees: 160 bytes (0..160)
    //   base_fee_info: BaseFeeInfo (40 bytes)
    //     mode: u8 (0)
    //     padding: [u8; 7] (1..8)
    //     data: [u64; 4] (8..40)
    //       data[0] = cliff_fee_numerator
    pub const CLIFF_FEE_NUMERATOR: usize = 8; // data[8..16] in raw

    // Dynamic fee struct starts at offset 44 within pool_fees
    //   protocol_fee_percent: u8 (40)
    //   referral_fee_percent: u8 (41)
    //   compounding_fee_bps: u16 (42)
    //   dynamic_fee: DynamicFeeStruct (44..140)
    pub const PROTOCOL_FEE_PERCENT: usize = 8 + 40;
    pub const REFERRAL_FEE_PERCENT: usize = 8 + 41;
    pub const COMPOUNDING_FEE_BPS: usize = 8 + 42;

    // DynamicFeeStruct (96 bytes, at offset 44 within pool_fees = 8+44=52 in raw)
    //   initialized: u8
    //   padding: [u8; 7]
    //   max_volatility_accumulator: u32
    //   variable_fee_control: u32
    //   bin_step: u16
    //   filter_period: u16
    //   decay_period: u16
    //   reduction_factor: u16
    //   padding_1: [u8; 8]
    //   bin_step_u128: u128
    //   last_updated_timestamp: u64
    //   volatility_accumulator: u32
    //   volatility_reference: u32
    //   index_reference: i32
    //   ...
    pub const DYN_FEE_START: usize = 8 + 44;
    pub const DYN_FEE_INITIALIZED: usize = DYN_FEE_START;
    pub const DYN_FEE_MAX_VOL_ACC: usize = DYN_FEE_START + 8;
    pub const DYN_FEE_VAR_FEE_CTRL: usize = DYN_FEE_START + 12;
    pub const DYN_FEE_BIN_STEP: usize = DYN_FEE_START + 16;
    pub const DYN_FEE_FILTER_PERIOD: usize = DYN_FEE_START + 18;
    pub const DYN_FEE_DECAY_PERIOD: usize = DYN_FEE_START + 20;
    pub const DYN_FEE_REDUCTION_FACTOR: usize = DYN_FEE_START + 22;
    // padding_1: 8 bytes at offset 24
    pub const DYN_FEE_BIN_STEP_U128: usize = DYN_FEE_START + 32;
    pub const DYN_FEE_LAST_UPDATED_TS: usize = DYN_FEE_START + 48;
    pub const DYN_FEE_VOL_ACC: usize = DYN_FEE_START + 56;
    pub const DYN_FEE_VOL_REF: usize = DYN_FEE_START + 60;
    pub const DYN_FEE_IDX_REF: usize = DYN_FEE_START + 64;

    // init_sqrt_price: u128 at offset 140 within pool_fees = 8+140=148
    pub const INIT_SQRT_PRICE: usize = 8 + 140;

    // After pool_fees (160 bytes):
    pub const TOKEN_A_MINT: usize = 8 + 160;     // 168
    pub const TOKEN_B_MINT: usize = 8 + 192;     // 200
    pub const TOKEN_A_VAULT: usize = 8 + 224;    // 232
    pub const TOKEN_B_VAULT: usize = 8 + 256;    // 264
    // whitelisted_vault: 32 bytes (296..328)
    // padding_0: 32 bytes (328..360)
    pub const LIQUIDITY: usize = 368;              // u128, 16-byte aligned (8 bytes padding before)
    // padding_1: 16 bytes (384..400)
    pub const PROTOCOL_A_FEE: usize = 392;        // u64 (shifted by alignment)
    pub const PROTOCOL_B_FEE: usize = 400;        // u64
    // padding_2: 16 bytes (408..424)
    pub const SQRT_MIN_PRICE: usize = 8 + 416;   // 424
    pub const SQRT_MAX_PRICE: usize = 8 + 432;   // 440
    pub const SQRT_PRICE: usize = 8 + 448;       // 456
    pub const ACTIVATION_POINT: usize = 8 + 464; // 472
    pub const ACTIVATION_TYPE: usize = 8 + 472;  // 480
    pub const POOL_STATUS: usize = 8 + 473;      // 481
    pub const TOKEN_A_FLAG: usize = 8 + 474;     // 482
    pub const TOKEN_B_FLAG: usize = 8 + 475;     // 483
    pub const COLLECT_FEE_MODE: usize = 8 + 476; // 484
    pub const POOL_TYPE: usize = 8 + 477;        // 485

    // token_a_amount / token_b_amount (for Compounding mode)
    pub const TOKEN_A_AMOUNT: usize = 8 + 672;   // 680
    pub const TOKEN_B_AMOUNT: usize = 8 + 680;   // 688
}

fn read_u8(data: &[u8], offset: usize) -> u8 {
    data[offset]
}

fn read_u16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(data[offset..offset + 2].try_into().unwrap())
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn read_i32(data: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn read_u64(data: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

fn read_u128(data: &[u8], offset: usize) -> u128 {
    u128::from_le_bytes(data[offset..offset + 16].try_into().unwrap())
}

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey {
    Pubkey::try_from(&data[offset..offset + 32]).unwrap()
}

const FEE_DENOMINATOR: u64 = 1_000_000_000;
const Q64: f64 = 18446744073709551616.0; // 2^64

/// Current off-chain ConstantProduct quote (what we use now — WRONG for non-compounding)
fn quote_constant_product(
    reserve_a: u64,
    reserve_b: u64,
    fee_numerator: u64,
    amount_in: u64,
    is_a_to_b: bool,
) -> u64 {
    let (r_in, r_out) = if is_a_to_b {
        (reserve_a as f64, reserve_b as f64)
    } else {
        (reserve_b as f64, reserve_a as f64)
    };
    if r_in == 0.0 || r_out == 0.0 {
        return 0;
    }
    let fee = fee_numerator as f64 / FEE_DENOMINATOR as f64;
    let amt = amount_in as f64 * (1.0 - fee);
    let out = (r_out * amt) / (r_in + amt);
    out as u64
}

/// Concentrated liquidity quote using Q64 fixed-point math (matches DAMM V2 on-chain)
///
/// sqrt_price is Q64 (real_value * 2^64). On-chain uses u128/U256 integer math.
/// Off-chain: we use f64 with the raw Q64 values directly, then scale output.
///
///   A→B (sell A, get B): price decreases
///     next_sp = L * sp / (L + amount_in * sp)
///     output  = L * (sp - next_sp) / 2^64    (delta_b)
///
///   B→A (sell B, get A): price increases
///     next_sp = sp + amount_in * 2^64 / L
///     output  = L * (next_sp - sp) / (sp * next_sp) * 2^64  (delta_a)
fn quote_concentrated(
    sqrt_price: u128,
    sqrt_min_price: u128,
    sqrt_max_price: u128,
    liquidity: u128,
    fee_numerator: u64,
    amount_in: u64,
    is_a_to_b: bool,
    collect_fee_mode: u8,
) -> u64 {
    if liquidity == 0 || sqrt_price == 0 {
        return 0;
    }

    let fee_rate = fee_numerator as f64 / FEE_DENOMINATOR as f64;

    // Determine if fees are on input or output
    let fees_on_input = match collect_fee_mode {
        0 => false, // BothToken: fees on output
        1 => !is_a_to_b, // OnlyB: fees on input when adding B
        _ => false,
    };

    let actual_amount_in = if fees_on_input {
        let fee_amount = ((amount_in as f64) * fee_rate).ceil() as u64;
        amount_in.saturating_sub(fee_amount)
    } else {
        amount_in
    };

    // Convert Q64 fixed-point to real f64 values
    let sp = sqrt_price as f64 / Q64;
    let sp_min = sqrt_min_price as f64 / Q64;
    let sp_max = sqrt_max_price as f64 / Q64;
    let l = liquidity as f64;
    let amt = actual_amount_in as f64;

    let output = if is_a_to_b {
        // A→B: price decreases
        // next_sp_real = L * sp_real / (L + amt * sp_real)
        let next_sp = l * sp / (l + amt * sp);
        let next_sp = next_sp.max(sp_min);
        // delta_b = L * (sp_real - next_sp_real)
        l * (sp - next_sp)
    } else {
        // B→A: price increases
        // next_sp_real = sp_real + amt / L
        let next_sp = sp + amt / l;
        let next_sp = next_sp.min(sp_max);
        // delta_a = L * (1/sp_real - 1/next_sp_real)
        l * (1.0 / sp - 1.0 / next_sp)
    };

    let final_output = if !fees_on_input {
        let fee_amount = (output * fee_rate).ceil();
        (output - fee_amount).max(0.0)
    } else {
        output
    };

    final_output as u64
}

/// Compounding mode quote (constant product using token_a_amount * token_b_amount)
fn quote_compounding(
    token_a_amount: u64,
    token_b_amount: u64,
    fee_numerator: u64,
    amount_in: u64,
    is_a_to_b: bool,
) -> u64 {
    quote_constant_product(token_a_amount, token_b_amount, fee_numerator, amount_in, is_a_to_b)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let pool_address = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_POOL.to_string());
    let pool_pubkey = Pubkey::from_str(&pool_address)?;

    println!("=== Meteora DAMM V2 Quote Verification ===");
    println!("Pool: {}", pool_pubkey);
    println!();

    let rpc = RpcClient::new(RPC_URL.to_string());

    // Fetch pool account
    let pool_account = rpc.get_account(&pool_pubkey).await?;
    let data = &pool_account.data;
    println!("Account data length: {} bytes", data.len());
    println!();

    // === Parse all fields ===
    let cliff_fee_numerator = read_u64(data, offsets::CLIFF_FEE_NUMERATOR);
    let protocol_fee_percent = read_u8(data, offsets::PROTOCOL_FEE_PERCENT);
    let referral_fee_percent = read_u8(data, offsets::REFERRAL_FEE_PERCENT);
    let compounding_fee_bps = read_u16(data, offsets::COMPOUNDING_FEE_BPS);

    // Dynamic fee
    let dyn_fee_initialized = read_u8(data, offsets::DYN_FEE_INITIALIZED);
    let max_vol_acc = read_u32(data, offsets::DYN_FEE_MAX_VOL_ACC);
    let var_fee_control = read_u32(data, offsets::DYN_FEE_VAR_FEE_CTRL);
    let bin_step = read_u16(data, offsets::DYN_FEE_BIN_STEP);
    let filter_period = read_u16(data, offsets::DYN_FEE_FILTER_PERIOD);
    let decay_period = read_u16(data, offsets::DYN_FEE_DECAY_PERIOD);
    let reduction_factor = read_u16(data, offsets::DYN_FEE_REDUCTION_FACTOR);
    let bin_step_u128 = read_u128(data, offsets::DYN_FEE_BIN_STEP_U128);
    let vol_acc = read_u32(data, offsets::DYN_FEE_VOL_ACC);
    let vol_ref = read_u32(data, offsets::DYN_FEE_VOL_REF);
    let idx_ref = read_i32(data, offsets::DYN_FEE_IDX_REF);

    let token_a_mint = read_pubkey(data, offsets::TOKEN_A_MINT);
    let token_b_mint = read_pubkey(data, offsets::TOKEN_B_MINT);
    let token_a_vault = read_pubkey(data, offsets::TOKEN_A_VAULT);
    let token_b_vault = read_pubkey(data, offsets::TOKEN_B_VAULT);

    let liquidity = read_u128(data, offsets::LIQUIDITY);
    let protocol_a_fee = read_u64(data, offsets::PROTOCOL_A_FEE);
    let protocol_b_fee = read_u64(data, offsets::PROTOCOL_B_FEE);
    let sqrt_min_price = read_u128(data, offsets::SQRT_MIN_PRICE);
    let sqrt_max_price = read_u128(data, offsets::SQRT_MAX_PRICE);
    let sqrt_price = read_u128(data, offsets::SQRT_PRICE);
    let pool_status = read_u8(data, offsets::POOL_STATUS);
    let collect_fee_mode = read_u8(data, offsets::COLLECT_FEE_MODE);
    let pool_type = read_u8(data, offsets::POOL_TYPE);

    let token_a_amount = read_u64(data, offsets::TOKEN_A_AMOUNT);
    let token_b_amount = read_u64(data, offsets::TOKEN_B_AMOUNT);

    // === Print all fields ===
    println!("--- Pool State ---");
    println!("token_a_mint:    {} ({})", token_a_mint,
        if token_a_mint.to_string().starts_with("So1") { "SOL" } else { "token" });
    println!("token_b_mint:    {} ({})", token_b_mint,
        if token_b_mint.to_string().starts_with("EPjF") { "USDC" } else { "token" });
    println!("token_a_vault:   {}", token_a_vault);
    println!("token_b_vault:   {}", token_b_vault);
    println!("pool_status:     {} ({})", pool_status, if pool_status == 0 { "enabled" } else { "disabled" });
    println!("collect_fee_mode:{} ({})", collect_fee_mode,
        match collect_fee_mode { 0 => "BothToken", 1 => "OnlyB", 2 => "Compounding", _ => "unknown" });
    println!("pool_type:       {} ({})", pool_type,
        match pool_type { 0 => "Permissionless", 1 => "Customizable", _ => "unknown" });
    println!();

    println!("--- Concentrated Liquidity State ---");
    println!("sqrt_price:      {} ({:.10} in f64)", sqrt_price, sqrt_price as f64 / Q64);
    println!("sqrt_min_price:  {} ({:.10} in f64)", sqrt_min_price, sqrt_min_price as f64 / Q64);
    println!("sqrt_max_price:  {} ({:.10} in f64)", sqrt_max_price, sqrt_max_price as f64 / Q64);
    println!("liquidity:       {}", liquidity);
    let price = (sqrt_price as f64 / Q64).powi(2);
    println!("implied price:   {:.6} (token_b per token_a)", price);
    println!();

    println!("--- Pool Tracked Amounts (for Compounding mode) ---");
    println!("token_a_amount:  {} ({:.6} SOL)", token_a_amount, token_a_amount as f64 / 1e9);
    println!("token_b_amount:  {} ({:.6} USDC)", token_b_amount, token_b_amount as f64 / 1e6);
    println!("protocol_a_fee:  {} ({:.6})", protocol_a_fee, protocol_a_fee as f64 / 1e9);
    println!("protocol_b_fee:  {} ({:.6})", protocol_b_fee, protocol_b_fee as f64 / 1e6);
    println!();

    println!("--- Fee Configuration ---");
    println!("cliff_fee_numerator:    {} ({:.4}%)", cliff_fee_numerator,
        cliff_fee_numerator as f64 / FEE_DENOMINATOR as f64 * 100.0);
    println!("protocol_fee_percent:   {}%", protocol_fee_percent);
    println!("referral_fee_percent:   {}%", referral_fee_percent);
    println!("compounding_fee_bps:    {} ({:.2}%)", compounding_fee_bps,
        compounding_fee_bps as f64 / 100.0);
    println!();

    println!("--- Dynamic Fee ---");
    println!("initialized:           {}", dyn_fee_initialized != 0);
    println!("max_vol_accumulator:   {}", max_vol_acc);
    println!("variable_fee_control:  {}", var_fee_control);
    println!("bin_step:              {}", bin_step);
    println!("bin_step_u128:         {}", bin_step_u128);
    println!("filter_period:         {}", filter_period);
    println!("decay_period:          {}", decay_period);
    println!("reduction_factor:      {}", reduction_factor);
    println!("volatility_accumulator:{}", vol_acc);
    println!("volatility_reference:  {}", vol_ref);
    println!("index_reference:       {}", idx_ref);

    // Calculate total fee (base + dynamic)
    let dynamic_fee = if dyn_fee_initialized != 0 && var_fee_control > 0 && bin_step > 0 {
        let square_vfa_bin = (vol_acc as u128) * (bin_step as u128);
        let square_vfa_bin = square_vfa_bin * square_vfa_bin;
        let v_fee = square_vfa_bin * (var_fee_control as u128);
        let scaled = (v_fee + 99_999_999_999) / 100_000_000_000; // ceil
        scaled as u64
    } else {
        0
    };
    let total_fee_numerator = cliff_fee_numerator.saturating_add(dynamic_fee);
    println!("dynamic_fee_numerator: {}", dynamic_fee);
    println!("total_fee_numerator:   {} ({:.4}%)", total_fee_numerator,
        total_fee_numerator as f64 / FEE_DENOMINATOR as f64 * 100.0);
    println!();

    // === Fetch vault balances ===
    println!("--- Vault Balances (live) ---");
    let vault_a_account = rpc.get_account(&token_a_vault).await?;
    let vault_b_account = rpc.get_account(&token_b_vault).await?;
    let vault_a_balance = u64::from_le_bytes(vault_a_account.data[64..72].try_into()?);
    let vault_b_balance = u64::from_le_bytes(vault_b_account.data[64..72].try_into()?);
    println!("vault_a balance: {} ({:.6} SOL)", vault_a_balance, vault_a_balance as f64 / 1e9);
    println!("vault_b balance: {} ({:.6} USDC)", vault_b_balance, vault_b_balance as f64 / 1e6);
    println!();

    // === Quote Comparison ===
    let test_amounts = [
        100_000_000u64,   // 0.1 SOL
        500_000_000,      // 0.5 SOL
        1_000_000_000,    // 1 SOL
        5_000_000_000,    // 5 SOL
    ];

    println!("=== Quote Comparison: A→B (SOL→USDC) ===");
    println!("{:<15} {:<20} {:<20} {:<15}",
        "Input (SOL)", "CP Quote (USDC)", "CL Quote (USDC)", "Diff %");
    for &amount in &test_amounts {
        let cp_out = quote_constant_product(
            vault_a_balance, vault_b_balance, cliff_fee_numerator,
            amount, true,
        );
        let cl_out = quote_concentrated(
            sqrt_price, sqrt_min_price, sqrt_max_price, liquidity,
            total_fee_numerator, amount, true, collect_fee_mode,
        );
        let diff_pct = if cl_out > 0 {
            ((cp_out as f64 - cl_out as f64) / cl_out as f64) * 100.0
        } else {
            f64::NAN
        };
        println!("{:<15.4} {:<20.6} {:<20.6} {:<+15.2}%",
            amount as f64 / 1e9,
            cp_out as f64 / 1e6,
            cl_out as f64 / 1e6,
            diff_pct,
        );
    }
    println!();

    println!("=== Quote Comparison: B→A (USDC→SOL) ===");
    let usdc_amounts = [
        10_000_000u64,    // 10 USDC
        50_000_000,       // 50 USDC
        100_000_000,      // 100 USDC
        500_000_000,      // 500 USDC
    ];
    println!("{:<15} {:<20} {:<20} {:<15}",
        "Input (USDC)", "CP Quote (SOL)", "CL Quote (SOL)", "Diff %");
    for &amount in &usdc_amounts {
        let cp_out = quote_constant_product(
            vault_a_balance, vault_b_balance, cliff_fee_numerator,
            amount, false,
        );
        let cl_out = quote_concentrated(
            sqrt_price, sqrt_min_price, sqrt_max_price, liquidity,
            total_fee_numerator, amount, false, collect_fee_mode,
        );
        let diff_pct = if cl_out > 0 {
            ((cp_out as f64 - cl_out as f64) / cl_out as f64) * 100.0
        } else {
            f64::NAN
        };
        println!("{:<15.2} {:<20.9} {:<20.9} {:<+15.2}%",
            amount as f64 / 1e6,
            cp_out as f64 / 1e9,
            cl_out as f64 / 1e9,
            diff_pct,
        );
    }
    println!();

    // Also show compounding mode quote if applicable
    if collect_fee_mode == 2 {
        println!("=== Compounding Mode: using token_a_amount/token_b_amount ===");
        for &amount in &test_amounts {
            let comp_out = quote_compounding(
                token_a_amount, token_b_amount, cliff_fee_numerator,
                amount, true,
            );
            println!("  {:.4} SOL → {:.6} USDC (compounding)", amount as f64 / 1e9, comp_out as f64 / 1e6);
        }
        println!();
    }

    println!("=== Summary ===");
    println!("collect_fee_mode = {} → {}",
        collect_fee_mode,
        if collect_fee_mode == 2 {
            "Compounding: should use token_a_amount/token_b_amount CP math"
        } else {
            "Non-Compounding: should use sqrt-price concentrated liquidity math"
        }
    );
    println!("Current off-chain uses: ConstantProduct with vault balances as reserves");
    println!("Dynamic fee component: {} ({:.4}% of total {:.4}%)",
        dynamic_fee,
        dynamic_fee as f64 / FEE_DENOMINATOR as f64 * 100.0,
        total_fee_numerator as f64 / FEE_DENOMINATOR as f64 * 100.0,
    );

    Ok(())
}
