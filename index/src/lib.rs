//! crown-index: ICP canister that folds pinned-splitter settlements into the
//! open reputation book (docs/core-spec.md §4–§7).
//!
//! Ingest is internal: an empty permissionless alarm clock (`ingest_hint`)
//! pulls the next chain read closer, a watchdog timer guarantees the book
//! catches up even when nobody rings. The hint carries no arguments and no
//! reply — it can move the WHEN of a read, never the WHAT: recognition,
//! consensus and the cursor stay untouched. Everything else in the Candid
//! surface is a query. No money, no keys, no signatures, no outcome
//! resolution.

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

/// The minimum spacing between paid chain reads (docs/core-spec.md §5): a
/// hint inside the gap is not dropped, it arms the read at the gap boundary.
/// This is the whole spam ceiling — however many hints arrive, the canister
/// pays for at most one read per gap.
const HINT_GAP: Duration = Duration::from_secs(60);

/// The completeness backstop behind the hint, a profile value: short on
/// testnet so runs never wait for a hint, long on mainnet where hints carry
/// the traffic and the watchdog only collects what nobody rang for.
const WATCHDOG: Duration = Duration::from_secs(source::INGEST_WATCHDOG);

thread_local! {
    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));

    /// The book, as the law's fold state, in stable memory only: queries read
    /// it directly, upgrades carry it as is — no rebuild, no heap copy, so
    /// recovery cost does not grow with the book.
    static BOOK_STABLE: RefCell<StableBTreeMap<Vec<u8>, u128, Memory>> =
        RefCell::new(StableBTreeMap::init(memory(BOOK_MEMORY)));

    /// Count of transactions rejected by the cross-check (docs/core-spec.md §5).
    static ANOMALIES: RefCell<StableCell<u64, Memory>> =
        RefCell::new(StableCell::init(memory(ANOMALY_MEMORY), 0));

    /// Local-testing override of the RPC canister principal; an empty blob on
    /// mainnet, where only the NNS-managed canister is allowed.
    static SOL_RPC_OVERRIDE: RefCell<StableCell<Vec<u8>, Memory>> =
        RefCell::new(StableCell::init(memory(SOL_RPC_MEMORY), Vec::new()));

    /// The root currently in certified data; queries return this, so answer
    /// and certificate always match even while an ingest round is running.
    static CERTIFIED_ROOT: Cell<[u8; 32]> = const { Cell::new([0; 32]) };

    static INGESTING: Cell<bool> = const { Cell::new(false) };

    /// Set by an applied settlement, cleared by recertification: rounds that
    /// found nothing skip the whole-book hash.
    static BOOK_DIRTY: Cell<bool> = const { Cell::new(false) };

    /// Nanosecond moment the armed global timer fires; a hint may only pull
    /// it earlier, never push it back.
    static NEXT_TIMER: Cell<u64> = const { Cell::new(u64::MAX) };

    /// Nanosecond moment the last ingest round started; the hint gap counts
    /// from here.
    static LAST_ROUND: Cell<u64> = const { Cell::new(0) };

    /// A hint arrived while a round was already running: re-arm at the gap
    /// boundary when the round ends, so the late tail is not lost.
    static HINT_PENDING: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn memory(id: MemoryId) -> Memory {
    MEMORY_MANAGER.with_borrow(|manager| manager.get(id))
}

pub(crate) fn key_bytes(key: &Key) -> Vec<u8> {
    let (chain, donor, recipient) = key;
    let mut out = Vec::new();
    for part in [
        chain.0.as_bytes(),
        donor.0.as_slice(),
        recipient.0.as_slice(),
    ] {
        out.extend((part.len() as u32).to_le_bytes());
        out.extend_from_slice(part);
    }
    out
}

/// Applies one settlement through the law against the entry's current total.
/// State changes are atomic within the calling execution slice.
pub(crate) fn apply_settlement(settled: &Settled) -> Result<(), ReduceError> {
    let key: Key = (
        settled.chain.clone(),
        settled.donor.clone(),
        settled.recipient.clone(),
    );
    let bytes = key_bytes(&key);
    // A one-entry fold: seed the law's state with the entry's current total —
    // exactly what folding its whole history produced — then apply. The law
    // stays the only place that adds; the index only carries state.
    let seed = Settled {
        chain: settled.chain.clone(),
        donor: settled.donor.clone(),
        recipient: settled.recipient.clone(),
        gross: BOOK_STABLE.with_borrow(|map| map.get(&bytes)).unwrap_or(0),
    };
    let mut entry = Book::new();
    reduce(&mut entry, &seed)?;
    reduce(&mut entry, settled)?;
    BOOK_STABLE.with_borrow_mut(|map| map.insert(bytes, entry.get(&key)));
    BOOK_DIRTY.with(|flag| flag.set(true));
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
    BOOK_STABLE
        .with_borrow(|map| map.get(&key_bytes(key)))
        .unwrap_or(0)
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

fn recertify() {
    let root = BOOK_STABLE.with_borrow(|map| {
        certify::stable_root(map.iter().map(|entry| (entry.key().clone(), entry.value())))
    });
    CERTIFIED_ROOT.with(|cell| cell.set(root));
    ic_cdk::api::certified_data_set(root);
}

fn schedule_ingest(delay: Duration) {
    let now = ic_cdk::api::time();
    let at = now.saturating_add(delay.as_nanos() as u64);
    ic_cdk::api::global_timer_set(at);
    NEXT_TIMER.with(|cell| cell.set(at));
}

/// Arms the timer at `at` unless it is already armed sooner.
fn schedule_at_if_sooner(at: u64) {
    if at < NEXT_TIMER.with(|cell| cell.get()) {
        ic_cdk::api::global_timer_set(at);
        NEXT_TIMER.with(|cell| cell.set(at));
    }
}

/// The earliest moment a hint arriving at `now` may trigger a read, given
/// when the last round started: never inside the gap, never in the past.
/// Pure — the arithmetic of the spam ceiling, pinned by tests below.
fn hint_boundary(now: u64, last_round: u64, gap: u64) -> u64 {
    now.max(last_round.saturating_add(gap))
}

/// The empty alarm clock (docs/core-spec.md §5). Affects only when the next
/// read happens, never what it finds: no arguments, no reply, recognition
/// and cursor untouched. Clients ring it after their transaction finalizes;
/// an early or repeated ring just lands on the gap boundary.
pub(crate) fn hint() {
    if INGESTING.with(|flag| flag.get()) {
        HINT_PENDING.with(|flag| flag.set(true));
        return;
    }
    let now = ic_cdk::api::time();
    let last = LAST_ROUND.with(|cell| cell.get());
    schedule_at_if_sooner(hint_boundary(now, last, HINT_GAP.as_nanos() as u64));
}

/// Local-testing overrides, for replicas that have no access to the real NNS
/// RPC canisters and no time to scan a public chain from genesis. Forbidden
/// on mainnet: there the full history is the law.
#[derive(candid::CandidType, candid::Deserialize)]
pub struct RpcOverrides {
    pub sol_rpc: Option<Principal>,
    /// (chain id, cursor value) pairs to start ingest from.
    pub cursor_seed: Option<Vec<(String, String)>>,
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
        for (chain_id, value) in overrides.cursor_seed.unwrap_or_default() {
            cursor::set(&chain_id, value);
        }
    }
    recertify();
    schedule_ingest(Duration::from_secs(1));
}

#[ic_cdk::post_upgrade]
fn post_upgrade() {
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
    let started = ic_cdk::api::time();
    LAST_ROUND.with(|cell| cell.set(started));
    for spec in source::CHAINS {
        if let Err(reason) = source::solana::ingest(spec).await {
            ic_cdk::println!("ingest {}: {}", spec.id, reason);
        }
        if BOOK_DIRTY.with(|flag| flag.replace(false)) {
            recertify();
        }
    }
    // A hint that rang mid-round may mean a settlement the round's pages
    // already missed: collect it at the gap boundary, not the watchdog.
    if HINT_PENDING.with(|flag| flag.replace(false)) {
        let now = ic_cdk::api::time();
        schedule_at_if_sooner(hint_boundary(now, started, HINT_GAP.as_nanos() as u64));
    }
}

#[cfg_attr(target_family = "wasm", unsafe(export_name = "canister_global_timer"))]
#[allow(dead_code)]
fn global_timer() {
    // Re-arm first: a trap inside the round must not stop the schedule.
    schedule_ingest(WATCHDOG);
    // futures::internals is not a stable ic-cdk surface; the exact version is
    // pinned by Cargo.lock — revisit this call on any ic-cdk bump.
    ic_cdk::futures::internals::in_executor_context(|| {
        ic_cdk::futures::spawn(ingest_round());
    });
}

#[cfg(test)]
mod tests {
    use super::hint_boundary;

    // The spam ceiling in one function: inside the gap a hint lands on the
    // boundary, outside it fires now, and the past never comes back.
    #[test]
    fn hint_boundary_is_the_gap_law() {
        const GAP: u64 = 60;
        // Quiet canister, stale last round: fire now.
        assert_eq!(hint_boundary(1_000, 0, GAP), 1_000);
        // Inside the gap: land exactly on the boundary.
        assert_eq!(hint_boundary(1_010, 1_000, GAP), 1_060);
        // On the boundary and beyond: fire now.
        assert_eq!(hint_boundary(1_060, 1_000, GAP), 1_060);
        assert_eq!(hint_boundary(2_000, 1_000, GAP), 2_000);
        // Overflow saturates instead of wrapping into the past.
        assert_eq!(hint_boundary(5, u64::MAX, GAP), u64::MAX);
    }
}
