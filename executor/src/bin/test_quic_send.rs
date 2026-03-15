//! Test: send a minimal SOL transfer via Astralane QUIC and check if it lands.
//!
//! Usage:
//!   cargo run --release --bin test_quic_send
//!
//! This sends a 0 SOL self-transfer (memo) to verify QUIC delivery works.

use std::time::Instant;
use base64::Engine as _;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::{
    message::Message,
    pubkey::Pubkey,
    signer::{keypair::Keypair, Signer},
    transaction::Transaction,
};
use solana_system_interface::instruction as system_instruction;
use astralane_quic_client::AstralaneQuicClient;

const RPC_URL: &str = "http://45.157.234.194:8899";
const QUIC_ENDPOINT: &str = "fr.gateway.astralane.io:7000";
const API_KEY: &str = "xyz_06iaBUneeAX291ZtQbMoGEMAStUaW9YXyHo2dIrkVSwSZiEspN75RJjTupMN";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Load keypair
    let keypair_path = std::env::var("KEYPAIR_PATH")
        .unwrap_or_else(|_| "./id.json".to_string());
    let payer = solana_sdk::signer::keypair::read_keypair_file(&keypair_path)
        .map_err(|e| anyhow::anyhow!("Failed to read keypair {}: {}", keypair_path, e))?;
    println!("Payer: {}", payer.pubkey());

    // Get fresh blockhash
    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed());
    let blockhash = rpc.get_latest_blockhash().await?;
    println!("Blockhash: {}", blockhash);

    // Build a simple self-transfer with Astralane tip
    let astralane_tip: Pubkey = "astra4uejePWneqNaJKuFFA8oonqCE1sqF6b45kDMZm".parse()?;
    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_price(100_000),
        ComputeBudgetInstruction::set_compute_unit_limit(50_000),
        system_instruction::transfer(&payer.pubkey(), &payer.pubkey(), 1000),
        system_instruction::transfer(&payer.pubkey(), &astralane_tip, 10_000),
    ];
    let msg = Message::new(&ixs, Some(&payer.pubkey()));
    let tx = Transaction::new(&[&payer], msg, blockhash);
    let tx_bytes = bincode::serialize(&tx)?;
    let sig = tx.signatures[0];
    println!("TX sig: {}", sig);
    println!("TX size: {} bytes", tx_bytes.len());

    // Connect to Astralane QUIC
    println!("\nConnecting to {} ...", QUIC_ENDPOINT);
    let client = AstralaneQuicClient::connect(QUIC_ENDPOINT, API_KEY).await?;
    println!("Connected!");

    // Send
    let t = Instant::now();
    client.send_transaction(&tx_bytes).await?;
    println!("Sent in {:.1}ms", t.elapsed().as_secs_f64() * 1000.0);

    // Wait and check
    println!("\nWaiting 5s then checking on-chain...");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    match rpc.get_signature_status(&sig).await? {
        Some(result) => {
            match result {
                Ok(()) => println!("✅ Transaction CONFIRMED on-chain!"),
                Err(e) => println!("❌ Transaction on-chain but FAILED: {:?}", e),
            }
        }
        None => {
            println!("❌ Transaction NOT FOUND on-chain after 5s");
            println!("Waiting 10 more seconds...");
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            match rpc.get_signature_status(&sig).await? {
                Some(result) => {
                    match result {
                        Ok(()) => println!("✅ Transaction CONFIRMED on-chain (took >5s)!"),
                        Err(e) => println!("❌ Transaction on-chain but FAILED: {:?}", e),
                    }
                }
                None => {
                    println!("❌ Transaction still NOT FOUND after 15s");
                    println!("\nNow trying same tx via HTTP Iris for comparison...");

                    // Rebuild with new blockhash
                    let blockhash2 = rpc.get_latest_blockhash().await?;
                    let msg2 = Message::new(&vec![
                        ComputeBudgetInstruction::set_compute_unit_price(100_000),
                        ComputeBudgetInstruction::set_compute_unit_limit(50_000),
                        system_instruction::transfer(&payer.pubkey(), &payer.pubkey(), 1000),
                        system_instruction::transfer(&payer.pubkey(), &astralane_tip, 10_000),
                    ], Some(&payer.pubkey()));
                    let tx2 = Transaction::new(&[&payer], msg2, blockhash2);
                    let tx2_bytes = bincode::serialize(&tx2)?;
                    let tx2_base64 = base64::engine::general_purpose::STANDARD.encode(&tx2_bytes);
                    let sig2 = tx2.signatures[0];
                    println!("HTTP TX sig: {}", sig2);

                    let http_client = reqwest::Client::new();
                    let body = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "sendTransaction",
                        "params": [tx2_base64, {"encoding": "base64", "skipPreflight": true}]
                    });
                    let url = format!("https://fr.gateway.astralane.io/iris2?api-key={}&method=sendTransaction", API_KEY);
                    let t2 = Instant::now();
                    let resp = http_client.post(&url)
                        .header("Content-Type", "text/plain")
                        .body(tx2_base64)
                        .send()
                        .await?;
                    println!("HTTP sent in {:.1}ms, status={}", t2.elapsed().as_secs_f64() * 1000.0, resp.status());
                    let resp_text = resp.text().await?;
                    println!("HTTP response: {}", resp_text);

                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    match rpc.get_signature_status(&sig2).await? {
                        Some(result) => {
                            match result {
                                Ok(()) => println!("✅ HTTP Transaction CONFIRMED!"),
                                Err(e) => println!("❌ HTTP Transaction FAILED: {:?}", e),
                            }
                        }
                        None => println!("❌ HTTP Transaction also NOT FOUND"),
                    }
                }
            }
        }
    }

    Ok(())
}
