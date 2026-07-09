//! E2e donate client: builds the same transaction a production client would —
//! idempotent streamer-ATA creation plus the donate instruction.
//!
//! Usage: donate <rpc-url> <donor-keypair-path> <streamer-pubkey> <gross-minor-units>

use std::str::FromStr;

use anchor_lang::{InstructionData, ToAccountMetas};
use anchor_spl::associated_token::{get_associated_token_address, spl_associated_token_account};
use anchor_spl::token::spl_token;
use solana_rpc_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Signer, read_keypair_file};
use solana_sdk::transaction::Transaction;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [url, keypair_path, streamer, gross] = &args[1..] else {
        eprintln!("usage: donate <rpc-url> <donor-keypair-path> <streamer-pubkey> <gross>");
        std::process::exit(2);
    };
    let donor = read_keypair_file(keypair_path).expect("cannot read donor keypair");
    let streamer = Pubkey::from_str(streamer).expect("bad streamer pubkey");
    let gross: u64 = gross.parse().expect("bad gross");

    let rpc = RpcClient::new_with_commitment(url.clone(), CommitmentConfig::confirmed());

    let create_streamer_ata =
        spl_associated_token_account::instruction::create_associated_token_account_idempotent(
            &donor.pubkey(),
            &streamer,
            &splitter::USDC_MINT,
            &spl_token::ID,
        );
    let accounts = splitter::accounts::Donate {
        payer: donor.pubkey(),
        streamer,
        mint: splitter::USDC_MINT,
        payer_usdc: get_associated_token_address(&donor.pubkey(), &splitter::USDC_MINT),
        streamer_usdc: get_associated_token_address(&streamer, &splitter::USDC_MINT),
        treasury_usdc: get_associated_token_address(&splitter::TREASURY, &splitter::USDC_MINT),
        token_program: spl_token::ID,
        event_authority: Pubkey::find_program_address(&[b"__event_authority"], &splitter::ID).0,
        program: splitter::ID,
    };
    let donate = Instruction {
        program_id: splitter::ID,
        accounts: accounts.to_account_metas(None),
        data: splitter::instruction::Donate { gross }.data(),
    };

    let blockhash = rpc.get_latest_blockhash().expect("blockhash");
    let tx = Transaction::new_signed_with_payer(
        &[create_streamer_ata, donate],
        Some(&donor.pubkey()),
        &[&donor],
        blockhash,
    );
    let signature = rpc
        .send_and_confirm_transaction(&tx)
        .expect("donate failed");
    println!("{signature}");
}
