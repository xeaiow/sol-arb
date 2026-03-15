//! Test: send a transaction via Astralane QUIC and verify it lands on-chain.
//!
//! Matches the official astralane-quic-client example exactly:
//! - finalized commitment for blockhash
//! - 100,000 lamports tip to Astralane tip account
//! - CU limit 20,000, CU price 10,000
//!
//! Usage:
//!   cd executor && RUST_LOG=info cargo run --release --bin test_quic_send

use std::time::Instant;
use base64::Engine as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::{
    message::{self, Message},
    pubkey::Pubkey,
    signer::{keypair::Keypair, Signer},
    transaction::{Transaction, VersionedTransaction},
};
use solana_system_interface::instruction as system_instruction;
use astralane_quic_client::AstralaneQuicClient;

const RPC_URL: &str = "http://45.157.234.194:8899";
const QUIC_ENDPOINT: &str = "fr.gateway.astralane.io:7000";
const API_KEY: &str = "xyz_06iaBUneeAX291ZtQbMoGEMAStUaW9YXyHo2dIrkVSwSZiEspN75RJjTupMN";
// Official Astralane tip account from SDK example
const ASTRALANE_TIP: &str = "astra4uejePWneqNaJKuFFA8oonqCE1sqF6b45kDMZm";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let keypair_path = std::env::var("KEYPAIR_PATH")
        .unwrap_or_else(|_| "./id.json".to_string());
    let payer = solana_sdk::signer::keypair::read_keypair_file(&keypair_path)
        .map_err(|e| anyhow::anyhow!("Failed to read keypair {}: {}", keypair_path, e))?;
    let tip_account: Pubkey = ASTRALANE_TIP.parse()?;
    println!("Payer: {}", payer.pubkey());

    // Use finalized commitment (same as SDK example)
    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::finalized());
    let blockhash = rpc.get_latest_blockhash().await?;
    println!("Blockhash (finalized): {}", blockhash);

    // Test 1: Legacy Transaction (same as SDK example)
    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(20_000),
        ComputeBudgetInstruction::set_compute_unit_price(10_000),
        system_instruction::transfer(&payer.pubkey(), &tip_account, 100_000),
    ];
    let msg = Message::new(&ixs, Some(&payer.pubkey()));
    let legacy_tx = Transaction::new(&[&payer], msg.clone(), blockhash);
    let legacy_bytes = bincode::serialize(&legacy_tx)?;

    // Test 2: VersionedTransaction v0 (what our bot actually sends)
    let v0_msg = message::v0::Message::try_compile(
        &payer.pubkey(), &ixs, &[], blockhash,
    )?;
    let versioned_tx = VersionedTransaction::try_new(
        message::VersionedMessage::V0(v0_msg), &[&payer],
    )?;
    let versioned_bytes = bincode::serialize(&versioned_tx)?;

    println!("Legacy TX:    {} bytes, sig={}", legacy_bytes.len(), legacy_tx.signatures[0]);
    println!("Versioned TX: {} bytes, sig={}", versioned_bytes.len(), versioned_tx.signatures[0]);

    // Use legacy for QUIC test (matches SDK example)
    let tx_bytes = legacy_bytes.clone();
    let sig = legacy_tx.signatures[0];
    let versioned_sig = versioned_tx.signatures[0];
    println!("TX sig: {}", sig);
    println!("TX size: {} bytes", tx_bytes.len());

    // Connect and send via multiple QUIC endpoints
    println!("\n--- QUIC Test (multiple endpoints) ---");
    let quic_endpoints = vec![
        "fr.gateway.astralane.io:7000",
        "ams.gateway.astralane.io:7000",
        "ny.gateway.astralane.io:7000",
    ];
    for ep in &quic_endpoints {
        print!("  {} ... ", ep);
        match AstralaneQuicClient::connect(ep, API_KEY).await {
            Ok(client) => {
                let t = Instant::now();
                match client.send_transaction(&tx_bytes).await {
                    Ok(_) => println!("sent in {:.3}ms", t.elapsed().as_secs_f64() * 1000.0),
                    Err(e) => println!("send err: {}", e),
                }
            }
            Err(e) => println!("connect err: {}", e),
        }
    }
    // Also send versioned tx via one endpoint
    println!("\n  Versioned TX via fr ...");
    let client = AstralaneQuicClient::connect("fr.gateway.astralane.io:7000", API_KEY).await?;
    match client.send_transaction(&versioned_bytes).await {
        Ok(_) => println!("  versioned sent OK ({} bytes)", versioned_bytes.len()),
        Err(e) => println!("  versioned send err: {}", e),
    }

    // Check both legacy and versioned on-chain
    for wait in [3, 5, 10] {
        println!("Waiting {}s...", wait);
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        let legacy_status = rpc.get_signature_status(&sig).await?;
        let versioned_status = rpc.get_signature_status(&versioned_sig).await?;
        match legacy_status {
            Some(Ok(())) => println!("  ✅ QUIC legacy CONFIRMED!"),
            Some(Err(e)) => println!("  ❌ QUIC legacy FAILED: {:?}", e),
            None => println!("  legacy: not found"),
        }
        match versioned_status {
            Some(Ok(())) => println!("  ✅ QUIC versioned CONFIRMED!"),
            Some(Err(e)) => println!("  ❌ QUIC versioned FAILED: {:?}", e),
            None => println!("  versioned: not found"),
        }
        if legacy_status.is_some() || versioned_status.is_some() {
            return Ok(());
        }
    }
    println!("❌ QUIC Transactions NOT FOUND after 18s");

    // Fallback: try HTTP Iris with same parameters
    println!("\n--- HTTP Iris Test (comparison) ---");
    let blockhash2 = rpc.get_latest_blockhash().await?;
    let ixs2 = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(20_000),
        ComputeBudgetInstruction::set_compute_unit_price(10_000),
        system_instruction::transfer(&payer.pubkey(), &tip_account, 100_000),
    ];
    let msg2 = Message::new(&ixs2, Some(&payer.pubkey()));
    let tx2 = Transaction::new(&[&payer], msg2, blockhash2);
    let tx2_bytes = bincode::serialize(&tx2)?;
    let tx2_base64 = base64::engine::general_purpose::STANDARD.encode(&tx2_bytes);
    let sig2 = tx2.signatures[0];
    println!("HTTP TX sig: {}", sig2);

    let http_client = reqwest::Client::new();
    let url = format!(
        "https://fr.gateway.astralane.io/iris2?api-key={}&method=sendTransaction",
        API_KEY
    );
    let t2 = Instant::now();
    let resp = http_client
        .post(&url)
        .header("Content-Type", "text/plain")
        .body(tx2_base64)
        .send()
        .await?;
    let status = resp.status();
    let text = resp.text().await?;
    println!("HTTP sent in {:.1}ms, status={}, response={}", t2.elapsed().as_secs_f64() * 1000.0, status, text);

    for wait in [3, 5, 10] {
        println!("Waiting {}s...", wait);
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        match rpc.get_signature_status(&sig2).await? {
            Some(Ok(())) => {
                println!("✅ HTTP Transaction CONFIRMED!");
                return Ok(());
            }
            Some(Err(e)) => {
                println!("❌ HTTP Transaction FAILED: {:?}", e);
                return Ok(());
            }
            None => {
                println!("  ...not found yet");
            }
        }
    }
    println!("❌ HTTP Transaction also NOT FOUND after 18s");

    Ok(())
}
