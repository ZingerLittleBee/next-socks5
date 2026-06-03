//! SOCKS5 protocol primitives (pure, no IO).

pub mod address;
pub mod handshake;

pub use address::{AddrError, Address};
