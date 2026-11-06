//! Reward Vault: host-funded reward campaigns for Passage events.
//!
//! A host funds a campaign up front (PASG plus the platform fee, or pre-minted NFTs plus a
//! fixed fee per NFT). Passage's distributor pays authorized claims in batches. The contract
//! enforces the total cap, the per-address cap, single-use claim ids and the claim and
//! reservation windows. Whatever is left goes back to the host.

pub mod contract;
pub mod error;
pub mod msg;
pub mod state;

pub use crate::error::ContractError;

#[cfg(test)]
mod tests;
