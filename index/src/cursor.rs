//! Ingest cursor: the only ingest state (docs/core-spec.md §5).
//! Cursor monotone + finality irreversible => exactly-once without a set of
//! seen transactions. For Solana the cursor is the last processed finalized
//! signature, stored as base58 text.

use std::cell::RefCell;

use ic_stable_structures::StableBTreeMap;

use crate::{CURSOR_MEMORY, Memory, memory};

thread_local! {
    static CURSORS: RefCell<StableBTreeMap<String, String, Memory>> =
        RefCell::new(StableBTreeMap::init(memory(CURSOR_MEMORY)));
}

pub fn get(chain_id: &str) -> Option<String> {
    CURSORS.with_borrow(|map| map.get(&chain_id.to_string()))
}

pub fn set(chain_id: &str, cursor: String) {
    CURSORS.with_borrow_mut(|map| map.insert(chain_id.to_string(), cursor));
}
