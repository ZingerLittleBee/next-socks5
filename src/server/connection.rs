//! Per-connection SOCKS5 state machine: handshake, optional auth, request
//! parsing, and command dispatch.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, watch};

use crate::auth::verify_credentials;
use crate::config::{AuthMethod, Config};
use crate::error::Socks5Error;
use crate::metrics::{Event, Metrics};
use crate::protocol::address::{AddrError, Address};
use crate::protocol::handshake::{
    method_reply, parse_greeting, parse_userpass, select_method, userpass_reply, HandshakeError,
    METHOD_NONE_ACCEPTABLE, METHOD_USERPASS,
};
use crate::protocol::reply::encode_reply;
use crate::protocol::request::{parse_request, Command, Request, RequestError};

use super::admission::Permit;
use super::connect;

/// Maximum bytes we will buffer while reading a single protocol message.
/// A greeting/request/auth message is small and bounded by the wire format.
const READ_BUF: usize = 512;

/// Drive a single client connection through the SOCKS5 state machine.
///
/// `_permit` keeps the connection counted against the accept-time admission caps
/// for its whole lifetime. `shutdown` is forwarded into the relays so an active
/// transfer winds down on server shutdown.
#[allow(clippy::too_many_arguments)]
pub async fn handle(
    mut stream: TcpStream,
    peer: SocketAddr,
    cfg: Arc<Config>,
    metrics: Arc<Metrics>,
    events: broadcast::Sender<Event>,
    shutdown: watch::Receiver<bool>,
    _permit: Permit,
) {
    // The entire pre-relay negotiation (greeting, optional auth, request) is
    // bounded by a single deadline so a slow/stalled client cannot pin this
    // task and its file descriptor indefinitely (pre-auth slowloris).
    let deadline = Duration::from_millis(cfg.timeouts.handshake_ms);
    let (request, initial) =
        match tokio::time::timeout(deadline, negotiate(&mut stream, &cfg, &events)).await {
            // A complete request was negotiated (with any pipelined tail bytes).
            Ok(Some(v)) => v,
            // negotiate() already sent any appropriate reply / chose to close.
            Ok(None) => return,
            // Handshake deadline elapsed: drop the half-open connection.
            Err(_) => return,
        };

    // Dispatch on the command.
    match request.command {
        Command::Connect => {
            connect::run(
                stream,
                request.address,
                initial,
                cfg,
                metrics,
                events,
                peer,
                shutdown,
            )
            .await;
        }
        Command::UdpAssociate => {
            // The TCP stream becomes the control connection that owns the UDP
            // association; the relay runs until the control connection closes.
            super::udp::run(stream, peer, cfg, metrics, events, shutdown).await;
        }
        Command::Bind => {
            // BIND is intentionally unsupported per the project spec.
            reply_failure(&mut stream, Socks5Error::CommandNotSupported).await;
            let _ = events.send(Event::Error {
                code: Socks5Error::CommandNotSupported.reply_code(),
                msg: "BIND not supported".to_string(),
            });
        }
    }
}

/// Run the greeting, optional username/password auth, and request parse over a
/// single persistent buffer, so bytes a client pipelines past one message
/// boundary are preserved for the next step (and the request's trailing bytes
/// are returned as the relay's initial payload).
///
/// Returns `Some((request, pipelined_tail))` on success, or `None` after
/// handling its own reply/close on any failure. Designed to be wrapped in a
/// single handshake deadline by [`handle`].
async fn negotiate(
    stream: &mut TcpStream,
    cfg: &Config,
    events: &broadcast::Sender<Event>,
) -> Option<(Request, Vec<u8>)> {
    let require_userpass = cfg.auth.method == AuthMethod::Password;
    let mut buf: Vec<u8> = Vec::with_capacity(READ_BUF);

    // 1. Greeting + method selection.
    let offered = loop {
        match parse_greeting(&buf) {
            Ok(methods) => {
                // VER + NMETHODS + methods.
                buf.drain(..2 + methods.len());
                break methods;
            }
            // A short read needs more bytes; any other error is fatal.
            Err(HandshakeError::Truncated) => {}
            Err(_) => return None,
        }
        if buf.len() > READ_BUF || !read_more(stream, &mut buf).await {
            return None;
        }
    };

    let chosen = select_method(&offered, require_userpass);
    if stream.write_all(&method_reply(chosen)).await.is_err() {
        return None;
    }
    if chosen == METHOD_NONE_ACCEPTABLE {
        return None;
    }

    // 2. Username/password authentication (RFC 1929) when required.
    if chosen == METHOD_USERPASS {
        let (user, ok) = loop {
            match parse_userpass(&buf) {
                Ok((u, p)) => {
                    // VER + ULEN + UNAME + PLEN + PASSWD.
                    buf.drain(..2 + u.len() + 1 + p.len());
                    let ok = verify_credentials(&cfg.auth.users, &u, &p);
                    break (u, ok);
                }
                // A short read needs more bytes; any other error is fatal.
                Err(HandshakeError::Truncated) => {}
                // A complete-but-malformed auth message gets a best-effort RFC
                // 1929 failure reply before closing, not a silent TCP close.
                Err(_) => {
                    let _ = stream.write_all(&userpass_reply(false)).await;
                    return None;
                }
            }
            if buf.len() > READ_BUF || !read_more(stream, &mut buf).await {
                return None;
            }
        };
        let _ = events.send(Event::Auth {
            ok,
            user: user.clone(),
        });
        if stream.write_all(&userpass_reply(ok)).await.is_err() || !ok {
            return None;
        }
    }

    // 3. SOCKS request.
    let request = loop {
        match parse_request(&buf) {
            Ok(req) => {
                // VER + CMD + RSV + encoded address.
                let mut encoded = Vec::new();
                req.address.encode(&mut encoded);
                buf.drain(..3 + encoded.len());
                break req;
            }
            // Both truncation cases need more bytes; keep reading.
            Err(RequestError::Truncated) | Err(RequestError::Addr(AddrError::Truncated)) => {}
            // Unrecognized command byte: command not supported.
            Err(RequestError::BadCommand(_)) => {
                return reply_request_error(stream, events, Socks5Error::CommandNotSupported).await;
            }
            // Unknown ATYP: address type not supported.
            Err(RequestError::Addr(AddrError::BadAtyp(_))) => {
                return reply_request_error(stream, events, Socks5Error::AddressNotSupported).await;
            }
            // Malformed domain is a generic failure, not an unsupported ATYP.
            Err(RequestError::Addr(AddrError::BadDomain)) => {
                return reply_request_error(stream, events, Socks5Error::General).await;
            }
            // Bad version has no well-defined SOCKS reply; close the connection.
            Err(RequestError::BadVersion(_)) => return None,
        }
        if buf.len() > READ_BUF || !read_more(stream, &mut buf).await {
            return None;
        }
    };

    // Any bytes left after the request are pipelined relay payload.
    Some((request, buf))
}

/// Read one chunk from `stream`, appending it to `buf`. Returns `false` on EOF
/// or IO error.
async fn read_more(stream: &mut TcpStream, buf: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; READ_BUF];
    match stream.read(&mut chunk).await {
        Ok(0) | Err(_) => false,
        Ok(n) => {
            buf.extend_from_slice(&chunk[..n]);
            true
        }
    }
}

/// Send the failure reply for a malformed request and emit an error event, then
/// return `None` to close the connection.
async fn reply_request_error(
    stream: &mut TcpStream,
    events: &broadcast::Sender<Event>,
    err: Socks5Error,
) -> Option<(Request, Vec<u8>)> {
    reply_failure(stream, err.clone()).await;
    let _ = events.send(Event::Error {
        code: err.reply_code(),
        msg: "malformed request".to_string(),
    });
    None
}

/// Send a best-effort failure reply with a zeroed IPv4 BND address.
async fn reply_failure(stream: &mut TcpStream, err: Socks5Error) {
    let mut out = Vec::with_capacity(10);
    let bind = Address::V4(Ipv4Addr::UNSPECIFIED, 0);
    encode_reply(err.reply_code(), &bind, &mut out);
    let _ = stream.write_all(&out).await;
}
