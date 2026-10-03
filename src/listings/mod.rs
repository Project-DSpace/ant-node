//! Listed files.
//!
//! A listed file stores its data map in an envelope (see the `ant-listings`
//! crate). When this node stores an envelope chunk it records a listing that
//! carries the chunk's on-chain payment, and it answers listing requests on
//! [`ant_listings::TOPIC`], separately from the chunk protocol.

mod chain;
mod service;
mod store;

pub use service::ListingService;
