//! SOCKS5 protocol primitives (pure, no IO).

pub mod address;
pub mod handshake;
pub mod reply;
pub mod request;

pub use address::{AddrError, Address};
