//! Solana source: pulls finalized transactions of the pinned splitter through
//! the SOL RPC canister and folds recognized settlements into the book.
//!
//! Recognition (docs/core-spec.md §4): only event-CPI inner instructions whose
//! program is the pinned splitter. Cross-check (§5): the two transfer legs
//! executed right before the event must move exactly the amounts the event
//! declares, in the configured USDC mint, out of the payer's account. Two
//! independent witnesses — the event and the executed transfers — must agree;
//! otherwise the transaction is rejected and the anomaly counter grows.

use std::str::FromStr;

use candid::Principal;
use crown_reduce::{Address, ChainId, Settled};
use ic_canister_runtime::IcRuntime;
use sol_rpc_client::SolRpcClient;
use sol_rpc_types::{
    CommitmentLevel, ConsensusStrategy, GetSignaturesForAddressLimit,
    GetSignaturesForAddressParams, GetTransactionEncoding, GetTransactionParams, MultiRpcResult,
    RpcConfig, RpcEndpoint, RpcSource, RpcSources, SolanaCluster,
};
use solana_pubkey::Pubkey;
use solana_transaction_status_client_types::{
    EncodedConfirmedTransactionWithStatusMeta, UiCompiledInstruction, UiInnerInstructions,
    UiInstruction, UiLoadedAddresses,
};

use crate::source::ChainSpec;

/// Anchor's event-CPI instruction tag (little-endian `EVENT_IX_TAG`).
const EVENT_IX_TAG: [u8; 8] = [0xe4, 0x45, 0xa5, 0x2e, 0x51, 0xcb, 0x9a, 0x1d];
/// `sha256("event:Settled")[..8]`; pinned by a test below.
const SETTLED_DISCRIMINATOR: [u8; 8] = [0xe8, 0xd2, 0x28, 0x11, 0x8e, 0x7c, 0x91, 0xee];
/// Event-CPI data layout: tag(8) discriminator(8) payer(32) streamer(32)
/// gross(8 LE) fee(8 LE).
const EVENT_DATA_LEN: usize = 96;
/// SPL Token `TransferChecked` data layout: opcode 12, amount u64 LE, decimals.
const TRANSFER_CHECKED_OPCODE: u8 = 12;
const TRANSFER_CHECKED_DATA_LEN: usize = 10;

/// Protocol constants, identical on every cluster.
const TOKEN_PROGRAMS: [Pubkey; 2] = [
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"),
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"),
];

pub struct SolanaChain {
    pub id: ChainId,
    pub splitter: Pubkey,
    pub usdc: Pubkey,
    pub sources: RpcSources,
    pub consensus: ConsensusStrategy,
}

pub fn parse_spec(spec: &ChainSpec) -> Result<SolanaChain, String> {
    let splitter = Pubkey::from_str(spec.splitter)
        .map_err(|e| format!("{}: bad splitter address: {e}", spec.id))?;
    let usdc =
        Pubkey::from_str(spec.usdc).map_err(|e| format!("{}: bad usdc mint: {e}", spec.id))?;
    let sources = match spec.source.split_once(':') {
        Some(("Default", "Mainnet")) => RpcSources::Default(SolanaCluster::Mainnet),
        Some(("Default", "Devnet")) => RpcSources::Default(SolanaCluster::Devnet),
        Some(("Custom", url)) => RpcSources::Custom(vec![RpcSource::Custom(RpcEndpoint {
            url: url.to_string(),
            headers: None,
        })]),
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
    Ok(SolanaChain {
        id: ChainId(spec.id.to_string()),
        splitter,
        usdc,
        sources,
        consensus,
    })
}

fn client(chain: &SolanaChain, sol_rpc: Principal) -> SolRpcClient<IcRuntime> {
    SolRpcClient::builder(IcRuntime::new(), sol_rpc)
        .with_rpc_sources(chain.sources.clone())
        .with_rpc_config(RpcConfig {
            response_consensus: Some(chain.consensus.clone()),
            ..Default::default()
        })
        .with_default_commitment_level(CommitmentLevel::Finalized)
        .build()
}

/// One ingest pass for one chain: fetch signatures newest-to-oldest down to
/// the cursor, then process transactions oldest-to-newest, advancing the
/// cursor after each one. Cursor and book move in the same execution slice,
/// so a trap can never split them apart.
pub async fn ingest(spec: &'static ChainSpec) -> Result<(), String> {
    let chain = parse_spec(spec)?;
    let client = client(&chain, crate::sol_rpc_canister());

    let cursor = match crate::cursor::get(spec.id) {
        None => None,
        Some(text) => Some(
            sol_rpc_types::Signature::from_str(&text)
                .map_err(|e| format!("{}: bad cursor: {e}", spec.id))?,
        ),
    };

    let mut pages = Vec::new();
    let mut before: Option<sol_rpc_types::Signature> = None;
    loop {
        let mut params = GetSignaturesForAddressParams::from(chain.splitter);
        params.commitment = Some(CommitmentLevel::Finalized);
        params.limit = Some(GetSignaturesForAddressLimit::default());
        params.before = before.clone();
        params.until = cursor.clone();
        // Cycles price depends on request and provider set: ask, then attach.
        let request = client.get_signatures_for_address(params);
        let cost = request
            .clone()
            .request_cost()
            .send()
            .await
            .map_err(|e| format!("{}: getSignaturesForAddress cost: {e}", spec.id))?;
        let batch = match request.with_cycles(cost).send().await {
            MultiRpcResult::Consistent(Ok(batch)) => batch,
            MultiRpcResult::Consistent(Err(e)) => {
                return Err(format!("{}: getSignaturesForAddress: {e}", spec.id));
            }
            MultiRpcResult::Inconsistent(_) => {
                return Err(format!(
                    "{}: getSignaturesForAddress: no consensus",
                    spec.id
                ));
            }
        };
        let full_page = batch.len() as u32 == GetSignaturesForAddressLimit::MAX_LIMIT;
        if let Some(oldest) = batch.last() {
            before = Some(oldest.signature.clone());
        }
        if !batch.is_empty() {
            pages.push(batch);
        }
        if !full_page {
            break;
        }
    }

    for info in pages
        .into_iter()
        .rev()
        .flat_map(|page| page.into_iter().rev())
    {
        if info.err.is_none() {
            let params = GetTransactionParams::from(solana_signature::Signature::from(
                info.signature.clone(),
            ));
            let request = client
                .get_transaction(params)
                .with_commitment(CommitmentLevel::Finalized)
                .with_max_supported_transaction_version(0)
                .with_encoding(GetTransactionEncoding::Base64);
            let cost = request
                .clone()
                .request_cost()
                .send()
                .await
                .map_err(|e| format!("{}: getTransaction cost: {e}", spec.id))?;
            let tx = match request.with_cycles(cost).send().await {
                MultiRpcResult::Consistent(Ok(Some(tx))) => tx,
                MultiRpcResult::Consistent(Ok(None)) => {
                    return Err(format!("{}: finalized tx not found (retry)", spec.id));
                }
                MultiRpcResult::Consistent(Err(e)) => {
                    return Err(format!("{}: getTransaction: {e}", spec.id));
                }
                MultiRpcResult::Inconsistent(_) => {
                    return Err(format!("{}: getTransaction: no consensus", spec.id));
                }
            };
            match extract_settlements(&chain.id, &chain.splitter, &chain.usdc, &tx) {
                Verdict::Settlements(settlements) => {
                    for settled in &settlements {
                        if crate::apply_settlement(settled).is_err() {
                            crate::bump_anomalies();
                        }
                    }
                }
                Verdict::Anomaly(reason) => {
                    ic_cdk::println!("{}: anomaly in {}: {reason}", spec.id, info.signature);
                    crate::bump_anomalies();
                }
            }
        }
        crate::cursor::set(spec.id, info.signature.to_string());
    }
    Ok(())
}

pub enum Verdict {
    Settlements(Vec<Settled>),
    Anomaly(&'static str),
}

struct TransferLeg {
    source: Pubkey,
    mint: Pubkey,
    authority: Pubkey,
    amount: u64,
}

fn array<const N: usize>(data: &[u8], start: usize) -> Option<[u8; N]> {
    data.get(start..start.checked_add(N)?)?.try_into().ok()
}

fn as_compiled(instruction: &UiInstruction) -> Option<&UiCompiledInstruction> {
    match instruction {
        UiInstruction::Compiled(compiled) => Some(compiled),
        UiInstruction::Parsed(_) => None,
    }
}

fn transfer_leg(keys: &[Pubkey], compiled: &UiCompiledInstruction) -> Option<TransferLeg> {
    let program = keys.get(compiled.program_id_index as usize)?;
    if !TOKEN_PROGRAMS.contains(program) {
        return None;
    }
    let data = bs58::decode(&compiled.data).into_vec().ok()?;
    if data.len() != TRANSFER_CHECKED_DATA_LEN || data[0] != TRANSFER_CHECKED_OPCODE {
        return None;
    }
    let amount = u64::from_le_bytes(data[1..9].try_into().ok()?);
    let index = |i: usize| keys.get(*compiled.accounts.get(i)? as usize).copied();
    Some(TransferLeg {
        source: index(0)?,
        mint: index(1)?,
        authority: index(3)?,
        amount,
    })
}

/// Extracts recognized settlements from one finalized transaction.
/// Pure: covered by fixture tests below with a real devnet donate.
pub fn extract_settlements(
    chain: &ChainId,
    splitter: &Pubkey,
    usdc: &Pubkey,
    tx: &EncodedConfirmedTransactionWithStatusMeta,
) -> Verdict {
    let Some(meta) = tx.transaction.meta.as_ref() else {
        return Verdict::Anomaly("missing meta");
    };
    if meta.err.is_some() {
        // A failed transaction has no effects and emits nothing.
        return Verdict::Settlements(Vec::new());
    }
    let Some(versioned) = tx.transaction.transaction.decode() else {
        return Verdict::Anomaly("undecodable transaction");
    };
    let mut keys: Vec<Pubkey> = versioned.message.static_account_keys().to_vec();
    let loaded: Option<UiLoadedAddresses> = meta.loaded_addresses.clone().into();
    if let Some(loaded) = loaded {
        for address in loaded.writable.iter().chain(loaded.readonly.iter()) {
            match Pubkey::from_str(address) {
                Ok(key) => keys.push(key),
                Err(_) => return Verdict::Anomaly("undecodable loaded address"),
            }
        }
    }

    let groups: Option<Vec<UiInnerInstructions>> = meta.inner_instructions.clone().into();
    let mut settlements = Vec::new();
    for group in groups.unwrap_or_default() {
        for (position, instruction) in group.instructions.iter().enumerate() {
            let Some(compiled) = as_compiled(instruction) else {
                return Verdict::Anomaly("unexpected parsed-form instruction");
            };
            if keys.get(compiled.program_id_index as usize) != Some(splitter) {
                continue;
            }
            let Ok(data) = bs58::decode(&compiled.data).into_vec() else {
                return Verdict::Anomaly("undecodable splitter instruction data");
            };
            if data.len() < 16 || data[..8] != EVENT_IX_TAG {
                // An instruction to the splitter that is not an event
                // (e.g. donate invoked via CPI); its own event follows.
                continue;
            }
            if data[8..16] != SETTLED_DISCRIMINATOR || data.len() != EVENT_DATA_LEN {
                return Verdict::Anomaly("unknown splitter event");
            }
            let (Some(payer), Some(streamer), Some(gross), Some(fee)) = (
                array::<32>(&data, 16).map(Pubkey::new_from_array),
                array::<32>(&data, 48).map(Pubkey::new_from_array),
                array::<8>(&data, 80).map(u64::from_le_bytes),
                array::<8>(&data, 88).map(u64::from_le_bytes),
            ) else {
                return Verdict::Anomaly("malformed event");
            };

            // The two transfer legs the splitter executed right before the
            // event, at the same call depth.
            let legs = position.checked_sub(2).and_then(|first| {
                let payout = transfer_leg(&keys, as_compiled(group.instructions.get(first)?)?)?;
                let fee = transfer_leg(&keys, as_compiled(group.instructions.get(first + 1)?)?)?;
                let heights = [first, first + 1]
                    .iter()
                    .filter_map(|i| as_compiled(group.instructions.get(*i)?))
                    .map(|c| c.stack_height)
                    .all(|h| h == compiled.stack_height);
                heights.then_some((payout, fee))
            });
            let Some((payout_leg, fee_leg)) = legs else {
                return Verdict::Anomaly("event without adjacent transfer legs");
            };

            let amounts_agree = payout_leg
                .amount
                .checked_add(fee_leg.amount)
                .is_some_and(|total| total == gross)
                && fee_leg.amount == fee
                && fee > 0;
            let structure_agrees = payout_leg.source == fee_leg.source
                && payout_leg.mint == *usdc
                && fee_leg.mint == *usdc
                && payout_leg.authority == payer
                && fee_leg.authority == payer;
            if !(amounts_agree && structure_agrees) {
                return Verdict::Anomaly("event disagrees with executed transfers");
            }

            settlements.push(Settled {
                chain: chain.clone(),
                payer: Address(payer.to_bytes().to_vec()),
                streamer: Address(streamer.to_bytes().to_vec()),
                gross: u128::from(gross),
            });
        }
    }
    Verdict::Settlements(settlements)
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/donate_devnet.json");
    const SPLITTER: &str = "3R4dk7uuLt5rnuD95roDhQkt2ZKV9xMAFjfx1Eb96nxP";
    const USDC: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";
    const PAYER: &str = "2b6JQquqQDsS8o3DFDiaxFLKTFMro1YrvVq7aimV4FzD";
    const STREAMER: &str = "Gt381v8RqGQUX7vdRbC9NdZCzGuzk6ZUgcTDLfUnYdcJ";

    fn fixture() -> EncodedConfirmedTransactionWithStatusMeta {
        serde_json::from_str(FIXTURE).unwrap()
    }

    fn chain() -> ChainId {
        ChainId("solana-devnet".to_string())
    }

    fn extract(
        splitter: &str,
        usdc: &str,
        tx: &EncodedConfirmedTransactionWithStatusMeta,
    ) -> Verdict {
        extract_settlements(
            &chain(),
            &Pubkey::from_str(splitter).unwrap(),
            &Pubkey::from_str(usdc).unwrap(),
            tx,
        )
    }

    #[test]
    fn discriminators_match_anchor_derivation() {
        let mut tag = Sha256::digest(b"anchor:event")[..8].to_vec();
        tag.reverse(); // anchor uses the little-endian u64 of the hash prefix
        assert_eq!(tag, EVENT_IX_TAG);
        assert_eq!(Sha256::digest(b"event:Settled")[..8], SETTLED_DISCRIMINATOR);
    }

    // The real devnet donate parses into exactly its settlement.
    #[test]
    fn real_donate_is_recognized() {
        let Verdict::Settlements(settlements) = extract(SPLITTER, USDC, &fixture()) else {
            panic!("real donate flagged as anomaly");
        };
        assert_eq!(settlements.len(), 1);
        let settled = &settlements[0];
        assert_eq!(settled.chain, chain());
        assert_eq!(
            settled.payer.0,
            Pubkey::from_str(PAYER).unwrap().to_bytes().to_vec()
        );
        assert_eq!(
            settled.streamer.0,
            Pubkey::from_str(STREAMER).unwrap().to_bytes().to_vec()
        );
        assert_eq!(settled.gross, 1_000_000);
    }

    // Recognition: the identical event emitted by any other program id is
    // not counted — pinning another splitter makes this event a stranger's.
    #[test]
    fn settled_from_other_program_is_ignored() {
        let other = Pubkey::new_unique().to_string();
        let Verdict::Settlements(settlements) = extract(&other, USDC, &fixture()) else {
            panic!("stranger's tx must not be an anomaly");
        };
        assert!(settlements.is_empty());
    }

    // Cross-check: an event whose gross disagrees with the executed transfers
    // is rejected as an anomaly.
    #[test]
    fn tampered_gross_is_an_anomaly() {
        let mut tx = fixture();
        let meta = tx.transaction.meta.as_mut().unwrap();
        let mut groups: Vec<UiInnerInstructions> =
            Option::from(meta.inner_instructions.clone()).unwrap();
        let event = groups[1].instructions.last_mut().unwrap();
        let UiInstruction::Compiled(compiled) = event else {
            panic!("expected compiled event instruction");
        };
        let mut data = bs58::decode(&compiled.data).into_vec().unwrap();
        data[80..88].copy_from_slice(&2_000_000u64.to_le_bytes());
        compiled.data = bs58::encode(data).into_string();
        meta.inner_instructions = Some(groups).into();

        assert!(matches!(
            extract(SPLITTER, USDC, &tx),
            Verdict::Anomaly("event disagrees with executed transfers")
        ));
    }

    // A settlement in a different mint than the configured USDC is rejected.
    #[test]
    fn wrong_mint_is_an_anomaly() {
        let other_usdc = Pubkey::new_unique().to_string();
        assert!(matches!(
            extract(SPLITTER, &other_usdc, &fixture()),
            Verdict::Anomaly("event disagrees with executed transfers")
        ));
    }

    // A failed transaction has no effects: nothing counted, no anomaly.
    #[test]
    fn failed_transaction_is_skipped() {
        let mut tx = fixture();
        tx.transaction.meta.as_mut().unwrap().err =
            Some(solana_transaction_error::TransactionError::AccountNotFound.into());
        let Verdict::Settlements(settlements) = extract(SPLITTER, USDC, &tx) else {
            panic!("failed tx must not be an anomaly");
        };
        assert!(settlements.is_empty());
    }
}
