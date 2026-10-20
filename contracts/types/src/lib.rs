#![no_std]

//! Shared types for the Sylox protocol, imported by every contract so
//! encodings never drift between them. See technical-doc.md Section 4.

pub mod assets;
pub mod events;
pub mod governance;
pub mod network_limits;
pub mod price_guard;
pub mod score;
pub mod series;
pub mod staking;
pub mod time;
pub mod treasury;
pub mod treasury_deposit;
pub mod ttl;
#[cfg(feature = "testutils")]
pub mod ttl_check;
pub mod upgrade;

pub use assets::*;
pub use events::*;
pub use governance::*;
pub use price_guard::*;
pub use score::*;
pub use series::*;
pub use staking::*;
pub use treasury::*;

/// Fixed point scale for prices and ratios (1e7). See technical-doc.md Section 1.4.
pub const SCALE: i128 = 10_000_000;

#[cfg(test)]
mod test;
