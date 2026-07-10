//! crown-index: ICP canister that folds pinned-splitter settlements into the
//! open reputation book (docs/core-spec.md §4–§7).
//!
//! No update methods: ingest runs on the global timer, queries are the whole
//! Candid surface. No money, no keys, no signatures, no outcome resolution.

pub mod api;
pub mod certify;
pub mod cursor;
pub mod source;

use std::cell::{Cell, RefCell};
use std::time::Duration;

use candid::Principal;
use crown_reduce::{Book, Key, ReduceError, Settled, reduce};
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{DefaultMemoryImpl, StableBTreeMap, StableCell};

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

pub(crate) const BOOK_MEMORY: MemoryId = MemoryId::new(0);
pub(crate) const CURSOR_MEMORY: MemoryId = MemoryId::new(1);
pub(crate) const ANOMALY_MEMORY: MemoryId = MemoryId::new(2);
pub(crate) const SOL_RPC_MEMORY: MemoryId = MemoryId::new(3);
pub(crate) const EVM_RPC_MEMORY: MemoryId = MemoryId::new(4);

const INGEST_INTERVAL: Duration = Duration::from_secs(60);

thread_local! {
    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));

    /// The book, as the law's fold state. Rebuilt from the stable mirror on
    /// upgrade; the mirror is updated after every applied settlement.
    static BOOK: RefCell<Book> = RefCell::new(Book::new());
    static BOOK_STABLE: RefCell<StableBTreeMap<Vec<u8>, u128, Memory>> =
        RefCell::new(StableBTreeMap::init(memory(BOOK_MEMORY)));

    /// Count of transactions rejected by the cross-check (docs/core-spec.md §5).
    static ANOMALIES: RefCell<StableCell<u64, Memory>> =
        RefCell::new(StableCell::init(memory(ANOMALY_MEMORY), 0));

    /// Local-testing overrides of the RPC canister principals; empty blobs on
    /// mainnet, where only the NNS-managed canisters are allowed.
    static SOL_RPC_OVERRIDE: RefCell<StableCell<Vec<u8>, Memory>> =
        RefCell::new(StableCell::init(memory(SOL_RPC_MEMORY), Vec::new()));
    static EVM_RPC_OVERRIDE: RefCell<StableCell<Vec<u8>, Memory>> =
        RefCell::new(StableCell::init(memory(EVM_RPC_MEMORY), Vec::new()));

    /// The root currently in certified data; queries return this, so answer
    /// and certificate always match even while an ingest round is running.
    static CERTIFIED_ROOT: Cell<[u8; 32]> = const { Cell::new([0; 32]) };

    static INGESTING: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn memory(id: MemoryId) -> Memory {
    MEMORY_MANAGER.with_borrow(|manager| manager.get(id))
}

fn key_bytes(key: &Key) -> Vec<u8> {
    let (chain, payer, streamer) = key;
    let mut out = Vec::new();
    for part in [
        chain.0.as_bytes(),
        payer.0.as_slice(),
        streamer.0.as_slice(),
    ] {
        out.extend((part.len() as u32).to_le_bytes());
        out.extend_from_slice(part);
    }
    out
}

fn key_from_bytes(bytes: &[u8]) -> Option<Key> {
    let mut rest = bytes;
    let mut parts = Vec::with_capacity(3);
    for _ in 0..3 {
        let (len, tail) = rest.split_at_checked(4)?;
        let len = u32::from_le_bytes(len.try_into().ok()?) as usize;
        let (part, tail) = tail.split_at_checked(len)?;
        parts.push(part.to_vec());
        rest = tail;
    }
    let streamer = crown_reduce::Address(parts.pop()?);
    let payer = crown_reduce::Address(parts.pop()?);
    let chain = crown_reduce::ChainId(String::from_utf8(parts.pop()?).ok()?);
    rest.is_empty().then_some((chain, payer, streamer))
}

/// Applies one settlement through the law and mirrors the touched entry into
/// stable memory. State changes are atomic within the calling execution slice.
pub(crate) fn apply_settlement(settled: &Settled) -> Result<(), ReduceError> {
    BOOK.with_borrow_mut(|book| reduce(book, settled))?;
    let key: Key = (
        settled.chain.clone(),
        settled.payer.clone(),
        settled.streamer.clone(),
    );
    let total = BOOK.with_borrow(|book| book.get(&key));
    BOOK_STABLE.with_borrow_mut(|map| map.insert(key_bytes(&key), total));
    Ok(())
}

pub(crate) fn bump_anomalies() {
    ANOMALIES.with_borrow_mut(|cell| {
        let next = cell.get().saturating_add(1);
        cell.set(next);
    });
}

pub(crate) fn anomaly_count() -> u64 {
    ANOMALIES.with_borrow(|cell| *cell.get())
}

pub(crate) fn reputation(key: &Key) -> u128 {
    BOOK.with_borrow(|book| book.get(key))
}

pub(crate) fn certified_root() -> [u8; 32] {
    CERTIFIED_ROOT.with(|root| root.get())
}

pub(crate) fn sol_rpc_canister() -> Principal {
    SOL_RPC_OVERRIDE.with_borrow(|cell| {
        let bytes = cell.get();
        if bytes.is_empty() {
            sol_rpc_client::SOL_RPC_CANISTER
        } else {
            Principal::from_slice(bytes)
        }
    })
}

pub(crate) fn evm_rpc_canister() -> Principal {
    EVM_RPC_OVERRIDE.with_borrow(|cell| {
        let bytes = cell.get();
        if bytes.is_empty() {
            evm_rpc_client::EVM_RPC_CANISTER
        } else {
            Principal::from_slice(bytes)
        }
    })
}

fn recertify() {
    let root = BOOK.with_borrow(certify::book_root);
    CERTIFIED_ROOT.with(|cell| cell.set(root));
    ic_cdk::api::certified_data_set(root);
}

fn rebuild_book_from_stable() {
    BOOK.with_borrow_mut(|book| {
        BOOK_STABLE.with_borrow(|stable| {
            for entry in stable.iter() {
                let Some((chain, payer, streamer)) = key_from_bytes(entry.key()) else {
                    ic_cdk::trap("stable book: undecodable key");
                };
                let settled = Settled {
                    chain,
                    payer,
                    streamer,
                    gross: entry.value(),
                };
                if reduce(book, &settled).is_err() {
                    ic_cdk::trap("stable book: rebuild overflow");
                }
            }
        });
    });
}

fn schedule_ingest(delay: Duration) {
    let now = ic_cdk::api::time();
    ic_cdk::api::global_timer_set(now.saturating_add(delay.as_nanos() as u64));
}

/// Local-testing overrides of the NNS RPC canisters, for replicas that have
/// no access to the real ones. Forbidden on mainnet.
#[derive(candid::CandidType, candid::Deserialize)]
pub struct RpcOverrides {
    pub sol_rpc: Option<Principal>,
    pub evm_rpc: Option<Principal>,
}

#[ic_cdk::init]
fn init(overrides: Option<RpcOverrides>) {
    if let Some(overrides) = overrides {
        if source::PROFILE == "mainnet" {
            ic_cdk::trap("mainnet profile: RPC canister overrides are forbidden");
        }
        if let Some(principal) = overrides.sol_rpc {
            SOL_RPC_OVERRIDE.with_borrow_mut(|cell| cell.set(principal.as_slice().to_vec()));
        }
        if let Some(principal) = overrides.evm_rpc {
            EVM_RPC_OVERRIDE.with_borrow_mut(|cell| cell.set(principal.as_slice().to_vec()));
        }
    }
    recertify();
    schedule_ingest(Duration::from_secs(1));
}

#[ic_cdk::post_upgrade]
fn post_upgrade() {
    rebuild_book_from_stable();
    recertify();
    schedule_ingest(Duration::from_secs(1));
}

/// Resets the ingest flag even when the round's task is cancelled by a trap,
/// so one failed round can never wedge ingest forever.
struct IngestGuard;

impl Drop for IngestGuard {
    fn drop(&mut self) {
        INGESTING.with(|flag| flag.set(false));
    }
}

async fn ingest_round() {
    if INGESTING.with(|flag| flag.replace(true)) {
        return;
    }
    let _guard = IngestGuard;
    for spec in source::CHAINS {
        // Chain kind by id prefix: "solana-*" is the Solana source,
        // everything else in the table is an EVM network.
        let result = if spec.id.starts_with("solana") {
            source::solana::ingest(spec).await
        } else {
            source::evm::ingest(spec).await
        };
        if let Err(reason) = result {
            ic_cdk::println!("ingest {}: {}", spec.id, reason);
        }
        recertify();
    }
}

#[cfg_attr(target_family = "wasm", unsafe(export_name = "canister_global_timer"))]
#[allow(dead_code)]
fn global_timer() {
    // Re-arm first: a trap inside the round must not stop the schedule.
    schedule_ingest(INGEST_INTERVAL);
    ic_cdk::futures::internals::in_executor_context(|| {
        ic_cdk::futures::spawn(ingest_round());
    });
}
