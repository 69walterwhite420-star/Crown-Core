//! Bakes contracts/solana/deploy.toml into the program as constants, so the
//! deploy parameters have one written source and the binary stays immutable.

use std::env;
use std::fs;
use std::path::Path;

fn value_of(toml: &str, key: &str) -> String {
    for line in toml.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                return v.trim().trim_matches('"').to_string();
            }
        }
    }
    panic!("deploy.toml: missing key `{key}`");
}

fn main() {
    let manifest = env::var("CARGO_MANIFEST_DIR").unwrap();
    let deploy = Path::new(&manifest).join("../../deploy.toml");
    println!("cargo:rerun-if-changed={}", deploy.display());
    let toml = fs::read_to_string(&deploy).unwrap();

    let fee_bps: u64 = value_of(&toml, "fee_bps").parse().unwrap();
    let treasury = value_of(&toml, "treasury");
    let usdc_mint = value_of(&toml, "usdc_mint");

    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("deploy_params.rs");
    fs::write(
        out,
        format!(
            "/// Fee in basis points; from deploy.toml.\n\
             pub const FEE_BPS: u64 = {fee_bps};\n\
             /// Treasury owner wallet; from deploy.toml.\n\
             pub const TREASURY: Pubkey = anchor_lang::solana_program::pubkey!(\"{treasury}\");\n\
             /// Native USDC mint; from deploy.toml.\n\
             pub const USDC_MINT: Pubkey = anchor_lang::solana_program::pubkey!(\"{usdc_mint}\");\n"
        ),
    )
    .unwrap();
}
