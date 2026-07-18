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
    for ((chain, donor, recipient), value) in book.iter() {
        for part in [
            chain.0.as_bytes(),
            donor.0.as_slice(),
            recipient.0.as_slice(),
        ] {
            hasher.update((part.len() as u32).to_le_bytes());
            hasher.update(part);
        }
        hasher.update(value.to_le_bytes());
    }
    hasher.finalize().into()
}

/// The same digest computed from the stable book without decoding entries:
/// the stored key encoding (lib.rs `key_bytes`) is byte-identical to the
/// length-prefixed entry stream above, which the test below pins.
pub(crate) fn stable_root(entries: impl Iterator<Item = (Vec<u8>, u128)>) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update(crown_reduce::REDUCE_VERSION.to_le_bytes());
    for (key, value) in entries {
        hasher.update(&key);
        hasher.update(value.to_le_bytes());
    }
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use crown_reduce::{Address, ChainId, Settled, reduce};

    use super::*;

    // The offchain recount hashes decoded entries, the canister hashes the
    // stored key bytes: both paths must land on one digest forever, or
    // certificates stop matching recounts.
    #[test]
    fn stable_root_equals_book_root() {
        let mut book = Book::new();
        for (donor, recipient, gross) in [
            (vec![1u8; 32], vec![2u8; 32], 500_000u128),
            (vec![1u8; 32], vec![3u8], 1),
            (vec![4u8; 20], vec![2u8; 32], u128::from(u64::MAX)),
        ] {
            let settled = Settled {
                chain: ChainId("solana-devnet".to_string()),
                donor: Address(donor),
                recipient: Address(recipient),
                gross,
            };
            assert!(reduce(&mut book, &settled).is_ok());
        }
        let stored = book
            .iter()
            .map(|(key, value)| (crate::key_bytes(key), value));
        assert_eq!(stable_root(stored), book_root(&book));
    }

    #[test]
    fn empty_roots_agree() {
        let none = std::iter::empty::<(Vec<u8>, u128)>();
        assert_eq!(stable_root(none), book_root(&Book::new()));
    }
}
