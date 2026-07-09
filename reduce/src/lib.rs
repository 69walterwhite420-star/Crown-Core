//! crown-reduce: the pure fold from settlements to the reputation book.
//!
//! Zero dependencies, no I/O — enforced by CI, not by review.
//! Law of reputation: docs/core-spec.md §2.

#![forbid(unsafe_code)]
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing
)]
