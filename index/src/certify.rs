//! Book root for certified data (docs/core-spec.md §6): a deterministic
//! digest of the whole book that anyone can recompute from public settlements
//! via crown-reduce and compare against the NNS-signed certificate.

use crown_reduce::Book;
use sha2::{Digest, Sha256};

/// Domain separator; bump together with a root-scheme change.
const DOMAIN: &[u8] = b"crown-book-root-v1";

/// sha256 over the domain, the reduce version and the length-prefixed entry
/// stream in key order.
pub fn book_root(book: &Book) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update(crown_reduce::REDUCE_VERSION.to_le_bytes());
    for ((chain, payer, streamer), value) in book.iter() {
        for part in [
            chain.0.as_bytes(),
            payer.0.as_slice(),
            streamer.0.as_slice(),
        ] {
            hasher.update((part.len() as u32).to_le_bytes());
            hasher.update(part);
        }
        hasher.update(value.to_le_bytes());
    }
    hasher.finalize().into()
}
