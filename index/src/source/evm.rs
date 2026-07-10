//! EVM source: pulls finalized `Settled` logs of the pinned splitter through
//! the EVM RPC canister and folds them into the book.
//!
//! Recognition (docs/core-spec.md §4): logs are requested and verified by the
//! pinned contract address plus the `Settled` topic; the immutable contract is
//! what makes the numbers honest. Ingest is two calls per batch: a consensus
//! finalized height, then logs over a concrete block range — concrete ranges
//! keep multi-provider responses identical and the cursor exact.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::str::FromStr;

use crown_reduce::{Address, ChainId, Settled};
use evm_rpc_client::{CandidResponseConverter, EvmRpcClient, NoRetry};
use evm_rpc_types::{
    BlockTag, CallArgs, ConsensusStrategy, GetLogsArgs, Hex20, Hex32, LogEntry, MultiRpcResult,
    RpcApi, RpcConfig, RpcError, RpcServices, TransactionRequest,
};
use ic_canister_runtime::IcRuntime;

use crate::source::ChainSpec;

/// keccak256("Settled(address,address,uint256,uint256)"); pinned by a test.
const SETTLED_TOPIC: [u8; 32] = [
    0x16, 0xc4, 0x1a, 0x74, 0x9c, 0xf9, 0x4b, 0xd4, 0x79, 0xb1, 0xfc, 0x5d, 0x82, 0xa6, 0xeb, 0x45,
    0x57, 0xd7, 0x12, 0x62, 0xf1, 0x5d, 0xc3, 0x82, 0xd2, 0xcf, 0x6f, 0x1e, 0xb3, 0xd6, 0x8e, 0x8e,
];

/// Blocks per eth_getLogs call: the EVM RPC canister rejects explicit ranges
/// over 500 blocks (fact, v2.8.0). Makes the catch-up from genesis slow but
/// finite; local runs seed the cursor instead (see `RpcOverrides`).
const BLOCKS_PER_CALL: u64 = 500;
/// Calls per ingest round: bounds cycles burn per round. In steady state a
/// round makes one or two calls; during catch-up it runs at this cap.
const CALLS_PER_ROUND: u32 = 2_000;

/// The EVM RPC canister's `requestCost` occasionally quotes 0 on Custom
/// sources (observed on S4 e2e); attaching 0 fails the call. Excess cycles
/// are refunded, so a floor is free insurance.
const CYCLES_FLOOR: u128 = 5_000_000_000;

/// The escrow seam and the factory constant (docs/factory-spec.md §4):
/// cast sig "salt()" / "donor()" / "ESCROW_INIT_CODE_HASH()".
const SALT_SELECTOR: [u8; 4] = [0xbf, 0xa0, 0xb1, 0x33];
const DONOR_SELECTOR: [u8; 4] = [0x25, 0x22, 0x3b, 0xd4];
const INIT_CODE_HASH_SELECTOR: [u8; 4] = [0x9e, 0x2b, 0xb7, 0xc7];

pub struct EvmChain {
    pub id: ChainId,
    pub splitter: Hex20,
    pub factories: Vec<[u8; 20]>,
    pub sources: RpcServices,
    pub consensus: ConsensusStrategy,
}

pub fn parse_spec(spec: &ChainSpec) -> Result<EvmChain, String> {
    let splitter = Hex20::from_str(spec.splitter)
        .map_err(|e| format!("{}: bad splitter address: {e}", spec.id))?;
    let factories = spec
        .factories
        .iter()
        .map(|f| {
            Hex20::from_str(f)
                .map(Into::into)
                .map_err(|e| format!("{}: bad factory: {e}", spec.id))
        })
        .collect::<Result<Vec<[u8; 20]>, _>>()?;
    let sources = match spec.source.split_once(':') {
        Some(("Default", "Ethereum")) => RpcServices::EthMainnet(None),
        Some(("Default", "Sepolia")) => RpcServices::EthSepolia(None),
        Some(("Default", "Base")) => RpcServices::BaseMainnet(None),
        Some(("Default", "Arbitrum")) => RpcServices::ArbitrumOne(None),
        Some(("Default", "Optimism")) => RpcServices::OptimismMainnet(None),
        Some(("Custom", rest)) => {
            let (chain_id, url) = rest
                .split_once(':')
                .ok_or_else(|| format!("{}: Custom needs `chainid:url`", spec.id))?;
            RpcServices::Custom {
                chain_id: chain_id
                    .parse()
                    .map_err(|e| format!("{}: bad custom chain id: {e}", spec.id))?,
                services: vec![RpcApi {
                    url: url.to_string(),
                    headers: None,
                }],
            }
        }
        _ => return Err(format!("{}: bad source `{}`", spec.id, spec.source)),
    };
    let consensus = match spec.consensus {
        "equality" => ConsensusStrategy::Equality,
        threshold => {
            let (min, total) = threshold
                .split_once("-of-")
                .ok_or_else(|| format!("{}: bad consensus `{threshold}`", spec.id))?;
            ConsensusStrategy::Threshold {
                min: min
                    .parse()
                    .map_err(|e| format!("{}: bad consensus: {e}", spec.id))?,
                total: Some(
                    total
                        .parse()
                        .map_err(|e| format!("{}: bad consensus: {e}", spec.id))?,
                ),
            }
        }
    };
    Ok(EvmChain {
        id: ChainId(spec.id.to_string()),
        splitter,
        factories,
        sources,
        consensus,
    })
}

/// One ingest pass: consensus finalized height, then chunked log scan from
/// cursor + 1 up to it. Cursor and book advance in the same execution slice
/// after each chunk, so a trap can never split them apart.
pub async fn ingest(spec: &'static ChainSpec) -> Result<(), String> {
    let chain = parse_spec(spec)?;
    let client = EvmRpcClient::builder(IcRuntime::new(), crate::evm_rpc_canister())
        .with_rpc_sources(chain.sources.clone())
        .with_rpc_config(RpcConfig {
            response_consensus: Some(chain.consensus.clone()),
            ..Default::default()
        })
        .build();

    let cursor: u64 = match crate::cursor::get(spec.id) {
        None => 0,
        Some(text) => text
            .parse()
            .map_err(|e| format!("{}: bad cursor: {e}", spec.id))?,
    };

    let request = client.get_block_by_number(BlockTag::Finalized);
    let cost = request
        .clone()
        .request_cost()
        .send()
        .await
        .map_err(|e| format!("{}: getBlockByNumber cost: {e}", spec.id))?;
    let finalized: u64 = match request.with_cycles(cost.max(CYCLES_FLOOR)).send().await {
        MultiRpcResult::Consistent(Ok(block)) => u64::try_from(block.number)
            .map_err(|e| format!("{}: finalized height out of u64: {e}", spec.id))?,
        MultiRpcResult::Consistent(Err(e)) => {
            return Err(format!("{}: getBlockByNumber: {e}", spec.id));
        }
        MultiRpcResult::Inconsistent(_) => {
            return Err(format!("{}: getBlockByNumber: no consensus", spec.id));
        }
    };

    let mut from = cursor.saturating_add(1);
    for _ in 0..CALLS_PER_ROUND {
        if from > finalized {
            break;
        }
        let to = from.saturating_add(BLOCKS_PER_CALL - 1).min(finalized);
        let args = GetLogsArgs {
            from_block: Some(BlockTag::Number(from.into())),
            to_block: Some(BlockTag::Number(to.into())),
            addresses: vec![chain.splitter.clone()],
            topics: Some(vec![vec![Hex32::from(SETTLED_TOPIC)]]),
        };
        let request = client.get_logs(args);
        let cost = request
            .clone()
            .request_cost()
            .send()
            .await
            .map_err(|e| format!("{}: getLogs cost: {e}", spec.id))?;
        let logs = match request.with_cycles(cost.max(CYCLES_FLOOR)).send().await {
            MultiRpcResult::Consistent(Ok(logs)) => logs,
            MultiRpcResult::Consistent(Err(e)) => {
                return Err(format!("{}: getLogs: {e}", spec.id));
            }
            MultiRpcResult::Inconsistent(_) => {
                return Err(format!("{}: getLogs: no consensus", spec.id));
            }
        };
        // Attribution awaits first; the applies, the anomaly bumps and the
        // cursor advance stay in one synchronous slice below, so a trap can
        // never split a settlement from its cursor.
        let mut attributed = Vec::with_capacity(logs.len());
        let mut anomalies: u32 = 0;
        for log in &logs {
            match decode_settled(&chain.id, &chain.splitter, log) {
                Ok(mut settled) => {
                    attribute(&client, &chain, &mut settled).await?;
                    attributed.push(settled);
                }
                Err(reason) => {
                    ic_cdk::println!("{}: anomaly: {reason}", spec.id);
                    anomalies = anomalies.saturating_add(1);
                }
            }
        }
        for settled in &attributed {
            if crate::apply_settlement(settled).is_err() {
                crate::bump_anomalies();
            }
        }
        for _ in 0..anomalies {
            crate::bump_anomalies();
        }
        crate::cursor::set(spec.id, to.to_string());
        from = to.saturating_add(1);
    }
    Ok(())
}

type Client = EvmRpcClient<IcRuntime, CandidResponseConverter, NoRetry>;

thread_local! {
    /// ESCROW_INIT_CODE_HASH per factory: an immutable constant of each
    /// pinned factory, read once under consensus and cached for the
    /// canister's life.
    static INIT_CODE_HASHES: RefCell<BTreeMap<[u8; 20], [u8; 32]>> =
        const { RefCell::new(BTreeMap::new()) };
}

/// Recognition root no.2 (docs/core-spec.md §4): when the payer is an escrow
/// born by a pinned factory, the settlement belongs to the escrow's donor.
/// Two eth_calls read the seam (`salt()`, `donor()`); the create2 arithmetic
/// decides — no registry, no trust in events.
async fn attribute(client: &Client, chain: &EvmChain, settled: &mut Settled) -> Result<(), String> {
    if chain.factories.is_empty() {
        return Ok(());
    }
    let payer: [u8; 20] = settled
        .payer
        .0
        .as_slice()
        .try_into()
        .map_err(|_| format!("{}: payer is not an address", chain.id.0))?;

    let Some(salt) = eth_view(client, &chain.id, payer, SALT_SELECTOR).await? else {
        return Ok(());
    };
    let Some(donor_word) = eth_view(client, &chain.id, payer, DONOR_SELECTOR).await? else {
        return Ok(());
    };
    for factory in &chain.factories {
        let hash = init_code_hash(client, &chain.id, *factory).await?;
        if let Some(donor) = escrow_donor(&payer, &salt, &donor_word, *factory, hash) {
            settled.payer = donor;
            break;
        }
    }
    Ok(())
}

/// The pure half of attribution: a 32-byte salt, an address-typed donor word,
/// and the create2 arithmetic must land exactly on the payer. Pure: covered
/// by tests below with the real Sepolia constellation.
pub fn escrow_donor(
    payer: &[u8; 20],
    salt: &[u8],
    donor_word: &[u8],
    factory: [u8; 20],
    init_code_hash: [u8; 32],
) -> Option<Address> {
    let salt: [u8; 32] = salt.try_into().ok()?;
    if donor_word.len() != 32 || donor_word.get(..12)?.iter().any(|b| *b != 0) {
        return None;
    }
    let derived = crown_derive::evm_create2_address(factory, salt, init_code_hash);
    let donor = donor_word.get(12..)?.to_vec();
    (derived == *payer).then_some(Address(donor))
}

async fn init_code_hash(
    client: &Client,
    chain: &ChainId,
    factory: [u8; 20],
) -> Result<[u8; 32], String> {
    if let Some(hash) = INIT_CODE_HASHES.with_borrow(|cache| cache.get(&factory).copied()) {
        return Ok(hash);
    }
    let word = eth_view(client, chain, factory, INIT_CODE_HASH_SELECTOR)
        .await?
        .ok_or_else(|| format!("{}: pinned factory has no ESCROW_INIT_CODE_HASH", chain.0))?;
    let hash: [u8; 32] = word
        .as_slice()
        .try_into()
        .map_err(|_| format!("{}: ESCROW_INIT_CODE_HASH is not 32 bytes", chain.0))?;
    INIT_CODE_HASHES.with_borrow_mut(|cache| {
        cache.insert(factory, hash);
    });
    Ok(hash)
}

/// eth_call of a no-argument view at the finalized block. `Ok(None)` when the
/// nodes evaluated the call and it reverted — a plain wallet or a foreign
/// contract without the seam. `Err` on transport or consensus trouble, so the
/// round retries instead of misattributing forever.
async fn eth_view(
    client: &Client,
    chain: &ChainId,
    to: [u8; 20],
    selector: [u8; 4],
) -> Result<Option<Vec<u8>>, String> {
    let args = CallArgs {
        transaction: TransactionRequest {
            to: Some(Hex20::from(to)),
            input: Some(selector.to_vec().into()),
            ..Default::default()
        },
        block: Some(BlockTag::Finalized),
    };
    let request = client.call(args);
    let cost = request
        .clone()
        .request_cost()
        .send()
        .await
        .map_err(|e| format!("{}: eth_call cost: {e}", chain.0))?;
    match request.with_cycles(cost.max(CYCLES_FLOOR)).send().await {
        MultiRpcResult::Consistent(Ok(bytes)) => {
            let bytes: Vec<u8> = bytes.as_ref().to_vec();
            Ok((!bytes.is_empty()).then_some(bytes))
        }
        MultiRpcResult::Consistent(Err(RpcError::JsonRpcError(_))) => Ok(None),
        MultiRpcResult::Consistent(Err(e)) => Err(format!("{}: eth_call: {e}", chain.0)),
        MultiRpcResult::Inconsistent(_) => Err(format!("{}: eth_call: no consensus", chain.0)),
    }
}

/// Decodes one `Settled` log. Pure: covered by tests below.
pub fn decode_settled(
    chain: &ChainId,
    splitter: &Hex20,
    log: &LogEntry,
) -> Result<Settled, &'static str> {
    if log.removed {
        return Err("removed log in a finalized range");
    }
    if log.address != *splitter {
        return Err("log from a foreign address");
    }
    let [topic0, payer, streamer] = log.topics.as_slice() else {
        return Err("unexpected topic count");
    };
    if topic0.as_ref() != SETTLED_TOPIC {
        return Err("foreign event topic");
    }
    let data: &[u8] = log.data.as_ref();
    if data.len() != 64 {
        return Err("unexpected data length");
    }
    let gross = u256_to_u128(&data[..32]).ok_or("gross out of u128")?;
    Ok(Settled {
        chain: chain.clone(),
        payer: indexed_address(payer).ok_or("payer topic is not an address")?,
        streamer: indexed_address(streamer).ok_or("streamer topic is not an address")?,
        gross,
    })
}

/// An address-typed indexed topic: 12 zero bytes, then the 20 address bytes.
fn indexed_address(topic: &Hex32) -> Option<Address> {
    let bytes: &[u8] = topic.as_ref();
    bytes[..12]
        .iter()
        .all(|b| *b == 0)
        .then(|| Address(bytes[12..].to_vec()))
}

fn u256_to_u128(be: &[u8]) -> Option<u128> {
    let (high, low) = be.split_at(16);
    if high.iter().any(|b| *b != 0) {
        return None;
    }
    Some(u128::from_be_bytes(low.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use sha3::{Digest, Keccak256};

    use super::*;

    const SPLITTER: &str = "0x5FbDB2315678afecb367f032d93F642f64180aa3";
    const PAYER: [u8; 20] = [0x11; 20];
    const STREAMER: [u8; 20] = [0x22; 20];

    fn topic_of(address: [u8; 20]) -> Hex32 {
        let mut bytes = [0u8; 32];
        bytes[12..].copy_from_slice(&address);
        Hex32::from(bytes)
    }

    fn settled_log(gross: u128, fee: u128) -> LogEntry {
        let mut data = [0u8; 64];
        data[16..32].copy_from_slice(&gross.to_be_bytes());
        data[48..64].copy_from_slice(&fee.to_be_bytes());
        LogEntry {
            address: Hex20::from_str(SPLITTER).unwrap(),
            topics: vec![
                Hex32::from(SETTLED_TOPIC),
                topic_of(PAYER),
                topic_of(STREAMER),
            ],
            data: data.to_vec().into(),
            block_number: Some(7u64.into()),
            transaction_hash: None,
            transaction_index: None,
            block_hash: None,
            log_index: Some(0u64.into()),
            removed: false,
        }
    }

    fn chain() -> ChainId {
        ChainId("eth-sepolia".to_string())
    }

    fn splitter() -> Hex20 {
        Hex20::from_str(SPLITTER).unwrap()
    }

    // The real Sepolia constellation (F4): the honest escrow attributes to
    // its donor through the pure create2 arithmetic.
    #[test]
    fn real_escrow_attributes_to_donor() {
        let payer: [u8; 20] = Hex20::from_str("0x07dF9de9860257057a277009A12F8A0d2ad400a0")
            .unwrap()
            .into();
        let factory: [u8; 20] = Hex20::from_str("0xb3e280657477c9effed7f02ff7233faa9ccc6258")
            .unwrap()
            .into();
        let salt: [u8; 32] =
            Hex32::from_str("0xf7003184597be1aee483b09947efb461845f993b801da21188a1633b4af766ca")
                .unwrap()
                .into();
        let hash: [u8; 32] =
            Hex32::from_str("0x5415a5314b9bebe5a6fe092fff7737865b86b2eb8538af8a25ae567781c02951")
                .unwrap()
                .into();
        let donor: [u8; 20] = Hex20::from_str("0x1cB584bbC3B0820DB4cb4619352D9f0140012eAb")
            .unwrap()
            .into();
        let mut donor_word = [0u8; 32];
        donor_word[12..].copy_from_slice(&donor);

        assert_eq!(
            escrow_donor(&payer, &salt, &donor_word, factory, hash),
            Some(Address(donor.to_vec()))
        );

        // A different pinned factory or hash must not recognize it.
        let other: [u8; 20] = Hex20::from_str("0x13C311C01b4A5EC3000e06373A11F4A8c5b1aFD8")
            .unwrap()
            .into();
        assert_eq!(escrow_donor(&payer, &salt, &donor_word, other, hash), None);
        assert_eq!(
            escrow_donor(&payer, &salt, &donor_word, factory, [0xab; 32]),
            None
        );

        // A donor word that is not an address-typed return is refused.
        let mut junk_word = donor_word;
        junk_word[0] = 1;
        assert_eq!(escrow_donor(&payer, &salt, &junk_word, factory, hash), None);
    }

    #[test]
    fn topic_matches_event_signature() {
        assert_eq!(
            Keccak256::digest(b"Settled(address,address,uint256,uint256)").as_slice(),
            SETTLED_TOPIC
        );
    }

    #[test]
    fn valid_log_decodes() {
        let settled = decode_settled(&chain(), &splitter(), &settled_log(1_000_000, 30_000))
            .expect("valid log must decode");
        assert_eq!(settled.chain, chain());
        assert_eq!(settled.payer.0, PAYER.to_vec());
        assert_eq!(settled.streamer.0, STREAMER.to_vec());
        assert_eq!(settled.gross, 1_000_000);
    }

    #[test]
    fn foreign_topic_is_rejected() {
        let mut log = settled_log(1_000_000, 30_000);
        log.topics[0] = Hex32::from([0xab; 32]);
        assert!(decode_settled(&chain(), &splitter(), &log).is_err());
    }

    #[test]
    fn foreign_address_is_rejected() {
        let mut log = settled_log(1_000_000, 30_000);
        log.address = Hex20::from([0xcd; 20]);
        assert!(decode_settled(&chain(), &splitter(), &log).is_err());
    }

    #[test]
    fn removed_log_is_rejected() {
        let mut log = settled_log(1_000_000, 30_000);
        log.removed = true;
        assert!(decode_settled(&chain(), &splitter(), &log).is_err());
    }

    #[test]
    fn oversized_gross_is_rejected() {
        let mut log = settled_log(1, 1);
        let mut data = [0u8; 64];
        data[0] = 1; // 2^248 — beyond u128
        log.data = data.to_vec().into();
        assert!(decode_settled(&chain(), &splitter(), &log).is_err());
    }

    #[test]
    fn malformed_topics_are_rejected() {
        let mut log = settled_log(1, 1);
        log.topics.pop();
        assert!(decode_settled(&chain(), &splitter(), &log).is_err());
    }
}
