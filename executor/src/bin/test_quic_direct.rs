//! Test our custom QUIC sender (bypassing Astralane SDK).
//! Sends same tx via: 1) our QUIC, 2) SDK QUIC, 3) HTTP Iris
//!
//! Usage: cd executor && RUST_LOG=info cargo run --release --bin test_quic_direct

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

const RPC_URL: &str = "http://45.157.234.194:8899";
const API_KEY: &str = "xyz_06iaBUneeAX291ZtQbMoGEMAStUaW9YXyHo2dIrkVSwSZiEspN75RJjTupMN";
const ASTRALANE_TIP: &str = "astra4uejePWneqNaJKuFFA8oonqCE1sqF6b45kDMZm";

async fn check_sig(rpc: &RpcClient, sig: &solana_sdk::signature::Signature, label: &str) {
    for wait in [3, 5, 10] {
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        match rpc.get_signature_status(sig).await {
            Ok(Some(Ok(()))) => { println!("  ✅ {} CONFIRMED!", label); return; }
            Ok(Some(Err(e))) => { println!("  ❌ {} FAILED: {:?}", label, e); return; }
            _ => { print!("."); }
        }
    }
    println!("\n  ❌ {} NOT FOUND after 18s", label);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let payer = solana_sdk::signer::keypair::read_keypair_file("./id.json")
        .map_err(|e| anyhow::anyhow!("keypair: {}", e))?;
    let tip_account: Pubkey = ASTRALANE_TIP.parse()?;
    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::finalized());

    println!("Payer: {}", payer.pubkey());

    // === Test 1: Our custom QUIC ===
    println!("\n--- Test 1: Custom QUIC direct ---");
    let blockhash = rpc.get_latest_blockhash().await?;
    let ixs = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(20_000),
        ComputeBudgetInstruction::set_compute_unit_price(10_000),
        system_instruction::transfer(&payer.pubkey(), &tip_account, 100_000),
    ];
    let tx = Transaction::new(&[&payer], Message::new(&ixs, Some(&payer.pubkey())), blockhash);
    let tx_bytes = bincode::serialize(&tx)?;
    let sig1 = tx.signatures[0];
    println!("  sig: {}", sig1);

    let sender = arb_executor::sender::astralane_quic::AstralaneQuicSender::new(
        "fr.gateway.astralane.io:7000".to_string(),
    );
    sender.connect(API_KEY).await?;
    let t = Instant::now();
    sender.send(&tx_bytes).await?;
    println!("  sent in {:.3}ms", t.elapsed().as_secs_f64() * 1000.0);
    print!("  checking");
    check_sig(&rpc, &sig1, "Custom QUIC").await;

    // === Test 2: SDK QUIC ===
    println!("\n--- Test 2: SDK QUIC ---");
    let blockhash = rpc.get_latest_blockhash().await?;
    let tx2 = Transaction::new(&[&payer], Message::new(&ixs, Some(&payer.pubkey())), blockhash);
    let tx2_bytes = bincode::serialize(&tx2)?;
    let sig2 = tx2.signatures[0];
    println!("  sig: {}", sig2);

    let sdk_client = astralane_quic_client::AstralaneQuicClient::connect(
        "fr.gateway.astralane.io:7000", API_KEY,
    ).await?;
    let t = Instant::now();
    sdk_client.send_transaction(&tx2_bytes).await?;
    println!("  sent in {:.3}ms", t.elapsed().as_secs_f64() * 1000.0);
    print!("  checking");
    check_sig(&rpc, &sig2, "SDK QUIC").await;

    // === Test 3: HTTP Iris ===
    println!("\n--- Test 3: HTTP Iris ---");
    let blockhash = rpc.get_latest_blockhash().await?;
    let tx3 = Transaction::new(&[&payer], Message::new(&ixs, Some(&payer.pubkey())), blockhash);
    let tx3_bytes = bincode::serialize(&tx3)?;
    let tx3_b64 = base64::engine::general_purpose::STANDARD.encode(&tx3_bytes);
    let sig3 = tx3.signatures[0];
    println!("  sig: {}", sig3);

    let http = reqwest::Client::new();
    let url = format!("https://fr.gateway.astralane.io/iris2?api-key={}&method=sendTransaction", API_KEY);
    let t = Instant::now();
    let resp = http.post(&url).header("Content-Type", "text/plain").body(tx3_b64).send().await?;
    println!("  sent in {:.1}ms, status={}", t.elapsed().as_secs_f64() * 1000.0, resp.status());
    print!("  checking");
    check_sig(&rpc, &sig3, "HTTP Iris").await;

    Ok(())
}
