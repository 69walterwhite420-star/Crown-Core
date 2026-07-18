//! The law of reputation: one rule, one branch (docs/core-spec.md §2).

use crate::book::{Book, Key};
use crate::event::Settled;

/// Version of the law. Bumped only by a conscious change to fold semantics;
/// a frozen indexer reports it via `get_reduce_version`.
pub const REDUCE_VERSION: u32 = 1;

/// The only failure the law can produce: the u128 total would overflow.
/// An error is a value here, never a panic.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReduceError {
    Overflow,
}

/// Folds one settlement into the book. On `Err` the book is unchanged.
pub fn reduce(book: &mut Book, settled: &Settled) -> Result<(), ReduceError> {
    let key: Key = (
        settled.chain.clone(),
        settled.donor.clone(),
        settled.recipient.clone(),
    );
    let total = book
        .get(&key)
        .checked_add(settled.gross)
        .ok_or(ReduceError::Overflow)?;
    book.set(key, total);
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;

    use super::*;
    use crate::event::{Address, ChainId};

    fn key_of(s: &Settled) -> Key {
        (s.chain.clone(), s.donor.clone(), s.recipient.clone())
    }

    fn fold(settlements: &[Settled]) -> Book {
        let mut book = Book::new();
        for s in settlements {
            reduce(&mut book, s).unwrap();
        }
        book
    }

    /// Small key space so random settlements collide on keys; gross bounded by
    /// u64::MAX so no honest fold in these tests can overflow u128.
    fn settled() -> impl Strategy<Value = Settled> {
        (
            prop_oneof![Just("solana-devnet"), Just("solana-mainnet")],
            0u8..3,
            0u8..3,
            0u128..=u128::from(u64::MAX),
        )
            .prop_map(|(chain, donor, recipient, gross)| Settled {
                chain: ChainId(chain.to_string()),
                donor: Address(vec![donor]),
                recipient: Address(vec![recipient]),
                gross,
            })
    }

    fn settlements() -> impl Strategy<Value = Vec<Settled>> {
        proptest::collection::vec(settled(), 0..64)
    }

    proptest! {
        // Monotonicity: no reduce ever lowers a value.
        #[test]
        fn value_never_decreases(ss in settlements(), s in settled()) {
            let mut book = fold(&ss);
            let before = book.get(&key_of(&s));
            reduce(&mut book, &s).unwrap();
            prop_assert!(book.get(&key_of(&s)) >= before);
        }

        // Additivity: any permutation of the settlement set folds into the
        // same book. Cross-chain ordering does not exist and is not needed.
        #[test]
        fn fold_is_permutation_invariant(
            (original, shuffled) in settlements()
                .prop_flat_map(|v| (Just(v.clone()), Just(v).prop_shuffle()))
        ) {
            prop_assert_eq!(fold(&original), fold(&shuffled));
        }

        // Recount: the book is exactly the independent per-key sum.
        #[test]
        fn book_equals_recount(ss in settlements()) {
            let mut expected: BTreeMap<Key, u128> = BTreeMap::new();
            for s in &ss {
                *expected.entry(key_of(s)).or_insert(0) += s.gross;
            }
            let got: BTreeMap<Key, u128> =
                fold(&ss).iter().map(|(k, v)| (k.clone(), v)).collect();
            prop_assert_eq!(got, expected);
        }

        // Key isolation: a settlement of (c, p, s) touches no other key.
        #[test]
        fn settlement_touches_only_its_key(ss in settlements(), s in settled()) {
            let before = fold(&ss);
            let mut after = before.clone();
            reduce(&mut after, &s).unwrap();
            for (k, v) in after.iter() {
                if *k != key_of(&s) {
                    prop_assert_eq!(v, before.get(k));
                }
            }
            for (k, v) in before.iter() {
                if *k != key_of(&s) {
                    prop_assert_eq!(v, after.get(k));
                }
            }
        }

        // Determinism: same input, same book.
        #[test]
        fn fold_is_deterministic(ss in settlements()) {
            prop_assert_eq!(fold(&ss), fold(&ss));
        }
    }

    #[test]
    fn overflow_is_an_error_and_leaves_book_unchanged() {
        let chain = ChainId("solana-devnet".to_string());
        let donor = Address(vec![1]);
        let recipient = Address(vec![2]);
        let key = (chain.clone(), donor.clone(), recipient.clone());
        let mut book = Book::new();

        let max = Settled {
            chain: chain.clone(),
            donor: donor.clone(),
            recipient: recipient.clone(),
            gross: u128::MAX,
        };
        reduce(&mut book, &max).unwrap();

        let one = Settled {
            chain,
            donor,
            recipient,
            gross: 1,
        };
        assert_eq!(reduce(&mut book, &one), Err(ReduceError::Overflow));
        assert_eq!(book.get(&key), u128::MAX);
    }

    // The law is versioned; changing semantics without bumping this is a bug.
    #[test]
    fn reduce_version_is_pinned() {
        assert_eq!(REDUCE_VERSION, 1);
    }
}
