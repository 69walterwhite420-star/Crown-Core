//! Certified book root (docs/core-spec.md §6): a hash tree over the whole
//! book, kept incrementally so certification costs O(log n) per settlement
//! instead of a linear rehash of every entry.
//!
//! The scheme, byte for byte, so a third party can reproduce it:
//!
//! 1. Every book entry is one labeled leaf of an `ic-certified-map` `RbTree`
//!    (the IC interface-spec hash tree: domain-separated sha256 over
//!    `ic-hashtree-{empty,fork,labeled,leaf}`).
//!    - label = the entry key bytes, `lib.rs::key_bytes`: for each of
//!      `chain` (UTF-8), `donor`, `recipient` in that order, the part length
//!      as `u32` little-endian followed by the part itself.
//!    - leaf  = the entry total as `u128` little-endian, 16 bytes. The leaf
//!      commits the number, so a witness proves the value and not merely the
//!      presence of the key.
//! 2. `H_tree` = the root hash of that tree.
//! 3. The certified book root, what goes into `set_certified_data`, is
//!    `sha256(DOMAIN || REDUCE_VERSION as u32 little-endian || H_tree)`.
//!    The seal binds the root to the version of the law that produced it: a
//!    root can never be reinterpreted under a different `reduce`.
//!
//! A key that was never settled has no leaf; `witness` then returns a proof
//! of absence, and that is the proof of the answer 0: the book defines a
//! missing key as 0 (`Book::get`), which is exactly what `get_reputation`
//! answers for it.

use std::cell::RefCell;

use crown_reduce::Book;
use ic_certified_map::{AsHashTree, Hash, HashTree, RbTree};
use sha2::{Digest, Sha256};

/// Domain separator; bump together with a root-scheme change.
/// v1 was the linear whole-book digest, v2 is the hash tree above.
const DOMAIN: &[u8] = b"crown-book-root-v2";

type Tree = RbTree<Vec<u8>, Vec<u8>>;

thread_local! {
    /// The certified tree: entry key bytes → the entry total. Heap state,
    /// mirrored by certified data after every mutation; rebuilt from the
    /// stable book on upgrade.
    static TREE: RefCell<Tree> = const { RefCell::new(RbTree::new()) };
}

/// The certified encoding of an entry total: 16 little-endian bytes.
fn value_bytes(value: u128) -> Vec<u8> {
    value.to_le_bytes().to_vec()
}

/// Step 3 of the scheme: seals a hash-tree root into the certified book
/// root. Public because an offchain verifier needs exactly this step to go
/// from a reconstructed witness to the value the certificate binds.
pub fn seal(tree_root: &Hash) -> Hash {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update(crown_reduce::REDUCE_VERSION.to_le_bytes());
    hasher.update(&tree_root[..]);
    hasher.finalize().into()
}

fn tree_from(entries: impl Iterator<Item = (Vec<u8>, u128)>) -> Tree {
    let mut tree = RbTree::new();
    for (key, value) in entries {
        tree.insert(key, value_bytes(value));
    }
    tree
}

/// The offchain definition of the root: the same tree, built from a book
/// recomputed by anyone from the public settlements through `crown-reduce`.
/// This is the "recount it yourself" contract — it must stay byte-identical
/// to what the canister certifies (index/tests/certificate.rs).
pub fn book_root(book: &Book) -> Hash {
    let entries = book
        .iter()
        .map(|(key, value)| (crate::key_bytes(key), value));
    seal(&tree_from(entries).root_hash())
}

/// The current certified book root. O(1): the tree carries its root hash.
pub(crate) fn root() -> Hash {
    TREE.with_borrow(|tree| seal(&tree.root_hash()))
}

/// Mirrors the tree root into certified data. Every mutation below ends
/// here, so at any commit boundary certified data equals `root()` — the
/// invariant that lets a query serve a witness and a certificate together.
fn recertify() {
    ic_cdk::api::certified_data_set(root());
}

/// Records one entry's new total and re-certifies. O(log n) in the number of
/// book entries: only the path from the leaf to the root is rehashed.
pub(crate) fn set_entry(key_bytes: Vec<u8>, value: u128) {
    TREE.with_borrow_mut(|tree| tree.insert(key_bytes, value_bytes(value)));
    recertify();
}

/// Upgrade path: the tree is heap state, so it is rebuilt from the stable
/// book. This is O(n) in book entries and the ONLY place cost still grows
/// with the book.
///
/// Accepted, not overlooked: upgrades exist only before the blackhole
/// (docs/core-spec.md §9), and after it there are none — the frozen canister
/// never runs this again. The steady state (ingest, certification, reads) is
/// O(log n) or O(1). If the book is ever grown past what one upgrade message
/// can rehash BEFORE freezing, the canister becomes un-upgradable and must
/// be frozen or replaced as is; it keeps serving either way.
pub(crate) fn rebuild(entries: impl Iterator<Item = (Vec<u8>, u128)>) {
    TREE.with_borrow_mut(|tree| *tree = tree_from(entries));
    recertify();
}

/// CBOR witness for one entry key: the pruned hash tree carrying the labeled
/// leaf with the entry total, or a proof of absence. Reconstructing it and
/// sealing the result yields the certified root.
pub(crate) fn witness(key_bytes: &[u8]) -> Vec<u8> {
    TREE.with_borrow(|tree| encode(&tree.witness(key_bytes)))
}

fn encode(tree: &HashTree<'_>) -> Vec<u8> {
    let mut serializer = serde_cbor::Serializer::new(Vec::new());
    serializer
        .self_describe()
        .and_then(|()| serde::Serialize::serialize(tree, &mut serializer))
        .map(|()| serializer.into_inner())
        // Serializing into a Vec has no failure mode; an empty witness fails
        // verification loudly rather than trapping a free query.
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use crown_reduce::{Address, ChainId, Key, Settled, reduce};

    use super::*;

    fn fixture() -> Book {
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
        book
    }

    fn key(donor: Vec<u8>, recipient: Vec<u8>) -> Key {
        (
            ChainId("solana-devnet".to_string()),
            Address(donor),
            Address(recipient),
        )
    }

    /// The offchain recount hashes decoded entries, the canister hashes the
    /// stored key bytes: both paths must land on one root forever, or
    /// certificates stop matching recounts.
    #[test]
    fn stable_stream_matches_book_root() {
        let book = fixture();
        let stored = book
            .iter()
            .map(|(key, value)| (crate::key_bytes(key), value));
        assert_eq!(seal(&tree_from(stored).root_hash()), book_root(&book));
    }

    #[test]
    fn empty_roots_agree() {
        let none = std::iter::empty::<(Vec<u8>, u128)>();
        assert_eq!(seal(&tree_from(none).root_hash()), book_root(&Book::new()));
    }

    /// The root scheme is a public contract with every offchain verifier.
    /// This pins it on a fixed book: a future edit that silently changes the
    /// scheme fails here instead of in the field, where the canister is
    /// frozen and the verifiers are not.
    #[test]
    fn root_scheme_is_pinned() {
        assert_eq!(
            hex(&book_root(&fixture())),
            "834c4dbb10cafd4b097502a31277b7db70e76d1189234a06d55140e2e2c0c600"
        );
        assert_eq!(
            hex(&book_root(&Book::new())),
            "84278f5dd4b1e772e29a784a3452052dc3bbda13c734ac1cdcf7cc50c336c518"
        );
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The offchain verifier, in full, over a third-party implementation of
    /// the interface-spec hash tree: decode the witness, reconstruct and seal
    /// it, check it against the root the certificate binds, then look the key
    /// up. A proven absence is the answer 0; a pruned or undecided branch is
    /// no answer at all.
    fn verify(witness: &[u8], root: Hash, key_bytes: &[u8]) -> Option<u128> {
        let tree: ic_certification::HashTree = serde_cbor::from_slice(witness).ok()?;
        if seal(&tree.digest()) != root {
            return None;
        }
        match tree.lookup_path([key_bytes]) {
            ic_certification::LookupResult::Found(value) => {
                Some(u128::from_le_bytes(<[u8; 16]>::try_from(value).ok()?))
            }
            ic_certification::LookupResult::Absent => Some(0),
            _ => None,
        }
    }

    #[test]
    fn witness_proves_the_value() {
        let book = fixture();
        let tree = tree_from(
            book.iter()
                .map(|(key, value)| (crate::key_bytes(key), value)),
        );
        let root = book_root(&book);

        for (donor, recipient, expected) in [
            (vec![1u8; 32], vec![2u8; 32], 500_000u128),
            (vec![1u8; 32], vec![3u8], 1),
            (vec![4u8; 20], vec![2u8; 32], u128::from(u64::MAX)),
            // Never settled: the witness proves absence, and a missing key
            // is 0 by the book's own definition.
            (vec![9u8; 32], vec![9u8; 32], 0),
        ] {
            let key_bytes = crate::key_bytes(&key(donor, recipient));
            let witness = encode(&tree.witness(&key_bytes));
            assert_eq!(verify(&witness, root, &key_bytes), Some(expected));
        }
    }

    #[test]
    fn forged_value_does_not_verify() {
        let book = fixture();
        let root = book_root(&book);
        let key_bytes = crate::key_bytes(&key(vec![1u8; 32], vec![2u8; 32]));

        // A book claiming a bigger total for the same key produces a real,
        // self-consistent witness — that is exactly the forgery a liar would
        // serve — but it cannot reconstruct to the certified root.
        let mut forged = book.clone();
        let bigger = Settled {
            chain: ChainId("solana-devnet".to_string()),
            donor: Address(vec![1u8; 32]),
            recipient: Address(vec![2u8; 32]),
            gross: 1,
        };
        assert!(reduce(&mut forged, &bigger).is_ok());
        let forged_tree = tree_from(
            forged
                .iter()
                .map(|(key, value)| (crate::key_bytes(key), value)),
        );
        let forged_witness = encode(&forged_tree.witness(&key_bytes));
        assert_eq!(
            verify(&forged_witness, book_root(&forged), &key_bytes),
            Some(500_001)
        );
        assert_eq!(verify(&forged_witness, root, &key_bytes), None);

        // The same for a witness fabricated from nothing but the claim.
        let value = value_bytes(u128::MAX);
        let bare = ic_certified_map::labeled(&key_bytes, HashTree::Leaf(value.as_slice().into()));
        assert_eq!(verify(&encode(&bare), root, &key_bytes), None);
    }
}
