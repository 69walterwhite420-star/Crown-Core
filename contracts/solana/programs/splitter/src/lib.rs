//! Crown splitter: immutable 97/3 split executed inside the donor's transaction.
//! The single `donate` instruction lands at S2 (docs/build-plan.md).

use anchor_lang::prelude::*;

declare_id!("3R4dk7uuLt5rnuD95roDhQkt2ZKV9xMAFjfx1Eb96nxP");

#[program]
pub mod splitter {}
