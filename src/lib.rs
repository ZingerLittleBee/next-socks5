//! next-socks5: a lightweight SOCKS5 server (RFC 1928 + RFC 1929).
//! Modules are added task-by-task: config, error, protocol, server, metrics, tui.

pub mod auth;
pub mod config;
pub mod error;
pub mod metrics;
pub mod protocol;
pub mod server;
pub mod admin;
pub mod mock;
#[cfg(feature = "tui")]
pub mod tui;
