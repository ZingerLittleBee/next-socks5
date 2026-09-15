//! next-socks5: a lightweight SOCKS5 server (RFC 1928 + RFC 1929).
//! Modules are added task-by-task: config, error, protocol, server, metrics, tui.

pub mod admin;
pub mod auth;
pub mod config;
pub mod dns;
pub mod error;
pub mod metrics;
pub mod mock;
pub mod protocol;
pub mod server;
#[cfg(feature = "tui")]
pub mod tui;
