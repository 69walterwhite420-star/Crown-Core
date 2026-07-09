//! crown-index: ICP canister that folds pinned-splitter settlements into the
//! open reputation book. Query-only Candid surface; ingest is an internal timer.
//!
//! Dependency direction is one-way: index -> reduce. See docs/core-spec.md §5–§6.
