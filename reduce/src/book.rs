//! The reputation book: `(chain, payer, streamer) -> Σ gross`.

use std::collections::BTreeMap;

use crate::event::{Address, ChainId};

/// Book key. Reputation is per-wallet and local to the streamer.
pub type Key = (ChainId, Address, Address);

/// The whole book. Ordered map, so equal contents are structurally equal
/// regardless of insertion order.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Book {
    entries: BTreeMap<Key, u128>,
}

impl Book {
    pub fn new() -> Self {
        Self::default()
    }

    /// Total gross ever settled for the key; 0 when the key was never settled.
    pub fn get(&self, key: &Key) -> u128 {
        self.entries.get(key).copied().unwrap_or(0)
    }

    /// Entries in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&Key, u128)> {
        self.entries.iter().map(|(key, value)| (key, *value))
    }

    pub(crate) fn set(&mut self, key: Key, value: u128) {
        self.entries.insert(key, value);
    }
}
