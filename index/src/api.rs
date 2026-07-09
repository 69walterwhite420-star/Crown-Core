//! The whole Candid surface: queries only (docs/core-spec.md §6).
//! Reading is free and permissionless and never affects the book.

use candid::Nat;
use serde_bytes::ByteBuf;

use crown_reduce::{Address, ChainId};

#[ic_cdk::query]
fn get_reputation(chain: String, payer: ByteBuf, streamer: ByteBuf) -> Nat {
    let key = (
        ChainId(chain),
        Address(payer.into_vec()),
        Address(streamer.into_vec()),
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
