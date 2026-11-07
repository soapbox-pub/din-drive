//! Din Drive: an encrypted drive on Nostr and Blossom, mounted with FUSE.
//! See SPEC.md for the protocol ("Blossom Drive").

pub mod blossom;
pub mod cache;
pub mod drive;
pub mod fs;
pub mod keys;
pub mod model;
pub mod relays;
pub mod signer;
pub mod store;
pub mod stream;
pub mod tree;

pub type Result<T, E = anyhow::Error> = std::result::Result<T, E>;

pub const DEFAULT_SERVERS: &[&str] = &[
    "https://blossom.ditto.pub",
    "https://blossom.dreamith.to",
    "https://blossom.primal.net",
];
