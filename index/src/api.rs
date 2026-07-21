//! The Candid surface: queries, plus the one empty alarm clock
//! (docs/core-spec.md §5–§6). Reading is free and permissionless and never
//! affects the book; the alarm clock affects only when the next chain read
//! happens.

use candid::Nat;
use serde_bytes::ByteBuf;

use crown_reduce::{Address, ChainId};

/// The empty alarm clock: no arguments, no reply, no authorization — the
/// right to ring it is the right to make the book fresher. Clients ring it
/// after their splitter transaction finalizes; the gap inside (lib.rs)
/// bounds how often rings can cost a paid read, and the watchdog timer
/// keeps the book complete when nobody rings at all.
#[ic_cdk::update]
fn ingest_hint() {
    crate::hint();
}

#[ic_cdk::query]
fn get_reputation(chain: String, donor: ByteBuf, recipient: ByteBuf) -> Nat {
    let key = (
        ChainId(chain),
        Address(donor.into_vec()),
        Address(recipient.into_vec()),
    );
    Nat::from(crate::reputation(&key))
}

#[ic_cdk::query]
fn get_cursor(chain: String) -> Option<String> {
    crate::cursor::get(&chain)
}

#[ic_cdk::query]
fn get_reduce_version() -> u32 {
    crown_reduce::REDUCE_VERSION
}

/// The IC certificate over this canister's certified data together with the
/// certified book root. Verifiable offchain against the NNS root key.
#[ic_cdk::query]
fn get_certificate() -> (Option<ByteBuf>, ByteBuf) {
    (
        ic_cdk::api::data_certificate().map(ByteBuf::from),
        ByteBuf::from(crate::certified_root().to_vec()),
    )
}

#[ic_cdk::query]
fn get_anomaly_count() -> u64 {
    crate::anomaly_count()
}
