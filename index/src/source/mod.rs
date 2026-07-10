//! Chain sources. Solana lands at S3; EVM lands at S4 (docs/build-plan.md).

pub mod evm;
pub mod solana;

/// One row of the config chain table (docs/core-spec.md §7), as baked by
/// build.rs from the selected profile. Exactly these six keys, nothing else.
pub struct ChainSpec {
    pub id: &'static str,
    pub source: &'static str,
    pub consensus: &'static str,
    pub splitter: &'static str,
    pub usdc: &'static str,
    /// The roots of recognition no.2: payers derived from these factories
    /// attribute their settlements to the escrow's donor.
    pub factories: &'static [&'static str],
}

include!(concat!(env!("OUT_DIR"), "/profile.rs"));
