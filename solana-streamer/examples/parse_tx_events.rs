use anyhow::Result;
use solana_commitment_config::CommitmentConfig;
use solana_streamer_sdk::streaming::event_parser::core::event_parser::EventParser;
use solana_streamer_sdk::streaming::event_parser::Protocol;
use solana_streamer_sdk::streaming::event_parser::DexEvent;
use std::str::FromStr;
use std::sync::Arc;
/// Get transaction data based on transaction signature
#[tokio::main]
async fn main() -> Result<()> {
    let signatures = vec![
        "4PsHYajH87x2zJPEGZczZtd2ksibuMCFPonC24jk5mTGZ46hzvjpzM5UZuLz9sRv79MkCBbtDqwJapGPTSkCFKoL",
    ];
    // Validate signature format
    let mut valid_signatures = Vec::new();
    for sig_str in &signatures {
        match solana_sdk::signature::Signature::from_str(sig_str) {
            Ok(_) => valid_signatures.push(*sig_str),
            Err(e) => println!("Invalid signature format: {}", e),
        }
    }
    if valid_signatures.is_empty() {
        println!("No valid transaction signatures");
        return Ok(());
    }
    for signature in valid_signatures {
        println!("Starting transaction parsing: {}", signature);
        get_single_transaction_details(signature).await?;
        println!("Transaction parsing completed: {}\n", signature);
        println!("Visit link to compare data: \nhttps://solscan.io/tx/{}\n", signature);
        println!("--------------------------------");
    }

    Ok(())
}

/// Get details of a single transaction
async fn get_single_transaction_details(signature_str: &str) -> Result<()> {
    use solana_sdk::{signature::Signature, pubkey::Pubkey, message::compiled_instruction::CompiledInstruction};
    use solana_transaction_status::{UiTransactionEncoding, InnerInstruction, InnerInstructions, UiInstruction};
    use prost_types::Timestamp;

    let signature = Signature::from_str(signature_str)?;

    // Create Solana RPC client
    let rpc_url = "https://api.mainnet-beta.solana.com";
    println!("Connecting to Solana RPC: {}", rpc_url);

    let client = solana_client::nonblocking::rpc_client::RpcClient::new(rpc_url.to_string());

    match client
        .get_transaction_with_config(
            &signature,
            solana_client::rpc_config::RpcTransactionConfig {
                encoding: Some(UiTransactionEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                max_supported_transaction_version: Some(0),
            },
        )
        .await
    {
        Ok(transaction) => {
            println!("Transaction signature: {}", signature_str);
            println!("Block slot: {}", transaction.slot);

            if let Some(block_time) = transaction.block_time {
                println!("Block time: {}", block_time);
            }

            if let Some(meta) = &transaction.transaction.meta {
                println!("Transaction fee: {} lamports", meta.fee);
                println!("Status: {}", if meta.err.is_none() { "Success" } else { "Failed" });
                if let Some(err) = &meta.err {
                    println!("Error details: {:?}", err);
                }
                // Compute units consumed
                if let solana_transaction_status::option_serializer::OptionSerializer::Some(units) =
                    &meta.compute_units_consumed
                {
                    println!("Compute units consumed: {}", units);
                }
                // Display logs (all)
                if let solana_transaction_status::option_serializer::OptionSerializer::Some(logs) =
                    &meta.log_messages
                {
                    println!("Transaction logs (all {} entries):", logs.len());
                    for (i, log) in logs.iter().enumerate() {
                        println!("  [{}] {}", i + 1, log);
                    }
                }
            }

            // Parse the transaction and extract necessary data
            let versioned_tx = match transaction.transaction.transaction.decode() {
                Some(tx) => tx,
                None => {
                    println!("Failed to decode transaction");
                    return Ok(());
                }
            };

            // Convert inner instructions from meta
            let mut inner_instructions_vec: Vec<InnerInstructions> = Vec::new();
            if let Some(meta) = &transaction.transaction.meta {
                if let solana_transaction_status::option_serializer::OptionSerializer::Some(
                    ui_inner_insts,
                ) = &meta.inner_instructions
                {
                    for ui_inner in ui_inner_insts {
                        let mut converted_instructions = Vec::new();

                        for ui_instruction in &ui_inner.instructions {
                            if let UiInstruction::Compiled(ui_compiled) = ui_instruction {
                                if let Ok(data) = solana_sdk::bs58::decode(&ui_compiled.data).into_vec() {
                                    let compiled_instruction = CompiledInstruction {
                                        program_id_index: ui_compiled.program_id_index,
                                        accounts: ui_compiled.accounts.to_vec(),
                                        data,
                                    };

                                    let inner_instruction = InnerInstruction {
                                        instruction: compiled_instruction,
                                        stack_height: ui_compiled.stack_height,
                                    };

                                    converted_instructions.push(inner_instruction);
                                }
                            }
                        }

                        let inner_instructions = InnerInstructions {
                            index: ui_inner.index,
                            instructions: converted_instructions,
                        };

                        inner_instructions_vec.push(inner_instructions);
                    }
                }
            }

            // Extract address table lookups
            let meta = transaction.transaction.meta;
            let mut address_table_lookups: Vec<Pubkey> = vec![];
            if let Some(meta) = meta {
                if let solana_transaction_status::option_serializer::OptionSerializer::Some(
                    loaded_addresses,
                ) = &meta.loaded_addresses
                {
                    address_table_lookups
                        .reserve(loaded_addresses.writable.len() + loaded_addresses.readonly.len());
                    address_table_lookups.extend(
                        loaded_addresses
                            .writable
                            .iter()
                            .filter_map(|s| s.parse::<Pubkey>().ok())
                            .chain(
                                loaded_addresses
                                    .readonly
                                    .iter()
                                    .filter_map(|s| s.parse::<Pubkey>().ok()),
                            ),
                    );
                }
            }

            // Build complete accounts list
            let mut accounts = Vec::with_capacity(
                versioned_tx.message.static_account_keys().len() + address_table_lookups.len(),
            );
            accounts.extend_from_slice(versioned_tx.message.static_account_keys());
            accounts.extend(address_table_lookups);

            let slot = transaction.slot;
            let block_time = transaction.block_time.map(|t| Timestamp { seconds: t as i64, nanos: 0 });
            let recv_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_micros() as i64;
            let bot_wallet = None;
            let tx_index = None;

            let protocols = vec![
                Protocol::Bonk,
                Protocol::RaydiumClmm,
                Protocol::PumpSwap,
                Protocol::PumpFun,
                Protocol::RaydiumCpmm,
                Protocol::RaydiumAmmV4,
                Protocol::MeteoraDammV2,
            ];

            // Create callback
            let callback = Arc::new(move |event: DexEvent| {
                println!("{:?}\n", event);
            });

            // Call parse_instruction_events_from_versioned_transaction
            EventParser::parse_instruction_events_from_versioned_transaction(
                &protocols,
                None,
                &versioned_tx,
                signature,
                Some(slot),
                block_time,
                recv_us,
                &accounts,
                &inner_instructions_vec,
                bot_wallet,
                tx_index,
                callback,
            )
            .await?;
        }
        Err(e) => {
            println!("Failed to get transaction: {}", e);
        }
    }

    println!("Press Ctrl+C to exit example...");
    tokio::signal::ctrl_c().await?;

    Ok(())
}
