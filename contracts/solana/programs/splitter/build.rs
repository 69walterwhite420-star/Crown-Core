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

    let usdc_mint = value_of(&toml, "usdc_mint");

    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("deploy_params.rs");
    fs::write(
        out,
        format!(
            "/// Native USDC mint; from deploy.toml.\n\
             pub const USDC_MINT: Pubkey = anchor_lang::solana_program::pubkey!(\"{usdc_mint}\");\n"
        ),
    )
    .unwrap();
}
