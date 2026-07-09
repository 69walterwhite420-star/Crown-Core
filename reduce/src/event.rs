//! The settlement event as the fold consumes it. Constructed at the boundary
//! (crown-index) from chain data; reduce itself never parses anything.

/// Internal chain id of the core, e.g. "solana-devnet". Comes from `config/`.
/// The book key includes it: a wallet on Solana and a wallet on Base are
/// different subjects (docs/core-spec.md §2).
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct ChainId(pub String);

/// Raw on-chain address bytes; interpretation is chain-local.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Address(pub Vec<u8>);

/// One recognized settlement: `gross` USDC minor units moved from `payer`
/// through the pinned splitter. Fields the law does not consume (fee) are
/// dropped at the boundary.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Settled {
    pub chain: ChainId,
    pub payer: Address,
    pub streamer: Address,
    pub gross: u128,
}
