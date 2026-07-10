//! EVM source: pulls finalized `Settled` logs of the pinned splitter through
//! the EVM RPC canister and folds them into the book.
//!
//! Recognition (docs/core-spec.md §4): logs are requested and verified by the
//! pinned contract address plus the `Settled` topic; the immutable contract is
//! what makes the numbers honest. Ingest is two calls per batch: a consensus
//! finalized height, then logs over a concrete block range — concrete ranges
//! keep multi-provider responses identical and the cursor exact.

use std::str::FromStr;

use crown_reduce::{Address, ChainId, Settled};
use evm_rpc_client::EvmRpcClient;
use evm_rpc_types::{
    BlockTag, ConsensusStrategy, GetLogsArgs, Hex20, Hex32, LogEntry, MultiRpcResult, RpcApi,
    RpcConfig, RpcServices,
};
use ic_canister_runtime::IcRuntime;

use crate::source::ChainSpec;

/// keccak256("Settled(address,address,uint256,uint256)"); pinned by a test.
const SETTLED_TOPIC: [u8; 32] = [
    0x16, 0xc4, 0x1a, 0x74, 0x9c, 0xf9, 0x4b, 0xd4, 0x79, 0xb1, 0xfc, 0x5d, 0x82, 0xa6, 0xeb, 0x45,
    0x57, 0xd7, 0x12, 0x62, 0xf1, 0x5d, 0xc3, 0x82, 0xd2, 0xcf, 0x6f, 0x1e, 0xb3, 0xd6, 0x8e, 0x8e,
];

/// Blocks per eth_getLogs call: small enough for provider range caps, big
/// enough to catch up quickly. Bounded per round to cap cycles burn.
const BLOCKS_PER_CALL: u64 = 10_000;
const CALLS_PER_ROUND: u32 = 30;

pub struct EvmChain {
    pub id: ChainId,
    pub splitter: Hex20,
    pub sources: RpcServices,
    pub consensus: ConsensusStrategy,
}

pub fn parse_spec(spec: &ChainSpec) -> Result<EvmChain, String> {
    let splitter = Hex20::from_str(spec.splitter)
        .map_err(|e| format!("{}: bad splitter address: {e}", spec.id))?;
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
    let finalized: u64 = match request.with_cycles(cost).send().await {
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
        let logs = match request.with_cycles(cost).send().await {
            MultiRpcResult::Consistent(Ok(logs)) => logs,
            MultiRpcResult::Consistent(Err(e)) => {
                return Err(format!("{}: getLogs: {e}", spec.id));
            }
            MultiRpcResult::Inconsistent(_) => {
                return Err(format!("{}: getLogs: no consensus", spec.id));
            }
        };
        for log in &logs {
            match decode_settled(&chain.id, &chain.splitter, log) {
                Ok(settled) => {
                    if crate::apply_settlement(&settled).is_err() {
                        crate::bump_anomalies();
                    }
                }
                Err(reason) => {
                    ic_cdk::println!("{}: anomaly: {reason}", spec.id);
                    crate::bump_anomalies();
                }
            }
        }
        crate::cursor::set(spec.id, to.to_string());
        from = to.saturating_add(1);
    }
    Ok(())
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
