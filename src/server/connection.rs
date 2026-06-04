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

/// Why a protocol read ended without yielding a message.
enum ReadFail {
    /// EOF, IO error, or buffer overflow: just close, no reply is meaningful.
    Closed,
    /// The bytes were a complete-but-invalid message (e.g. bad sub-negotiation
    /// version). The caller may send a best-effort failure reply before closing.
    Parse,
}

/// Drive a single client connection through the SOCKS5 state machine.
///
/// `_permit` keeps the connection counted against the accept-time admission caps
/// for its whole lifetime.
#[allow(clippy::too_many_arguments)]
pub async fn handle(
    mut stream: TcpStream,
    peer: SocketAddr,
    cfg: Arc<Config>,
    metrics: Arc<Metrics>,
    events: broadcast::Sender<Event>,
    _shutdown: watch::Receiver<bool>,
    _permit: Permit,
) {
    // The entire pre-relay negotiation (greeting, optional auth, request) is
    // bounded by a single deadline so a slow/stalled client cannot pin this
    // task and its file descriptor indefinitely (pre-auth slowloris).
    let deadline = Duration::from_millis(cfg.timeouts.handshake_ms);
    let request = match tokio::time::timeout(deadline, negotiate(&mut stream, &cfg, &events)).await
    {
        // A complete request was negotiated.
        Ok(Some(req)) => req,
        // negotiate() already sent any appropriate reply / chose to close.
        Ok(None) => return,
        // Handshake deadline elapsed: drop the half-open connection.
        Err(_) => return,
    };

    // Dispatch on the command. The connection limit is enforced at accept time
    // via the admission permit, so no post-request check is needed here.
    match request.command {
        Command::Connect => {
            connect::run(stream, request.address, cfg, metrics, events, peer).await;
        }
        Command::UdpAssociate => {
            // The TCP stream becomes the control connection that owns the UDP
            // association; the relay runs until the control connection closes.
            super::udp::run(stream, peer, cfg, metrics, events).await;
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

/// Run the greeting, optional username/password auth, and request parse.
///
/// Returns `Some(request)` on success, or `None` after handling its own
/// reply/close on any failure. Designed to be wrapped in a single handshake
/// deadline by [`handle`].
async fn negotiate(
    stream: &mut TcpStream,
    cfg: &Config,
    events: &broadcast::Sender<Event>,
) -> Option<Request> {
    // 1. Greeting + method selection.
    let require_userpass = cfg.auth.method == AuthMethod::Password;
    let offered = match read_message(stream, |b| match parse_greeting(b) {
        Ok(methods) => Ok(Some(methods)),
        // A short read needs more bytes; any other error is fatal.
        Err(HandshakeError::Truncated) => Ok(None),
        Err(_) => Err(()),
    })
    .await
    {
        Ok(m) => m,
        // A malformed greeting (e.g. bad version) has no defined reply: close.
        Err(_) => return None,
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
        let creds = read_message(stream, |b| match parse_userpass(b) {
            Ok(c) => Ok(Some(c)),
            // A short read needs more bytes; any other error is fatal.
            Err(HandshakeError::Truncated) => Ok(None),
            Err(_) => Err(()),
        })
        .await;
        let (user, ok) = match creds {
            Ok((u, p)) => {
                let ok = verify_credentials(&cfg.auth.users, &u, &p);
                (u, ok)
            }
            // A complete-but-malformed auth message gets a best-effort RFC 1929
            // failure reply before closing, rather than a silent TCP close.
            Err(ReadFail::Parse) => {
                let _ = stream.write_all(&userpass_reply(false)).await;
                return None;
            }
            Err(ReadFail::Closed) => return None,
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
    match read_request(stream).await {
        Ok(req) => Some(req),
        Err(maybe_code) => {
            // A precise reply code (when known) is sent before closing; some
            // failures (EOF/IO/overflow/bad version) have no SOCKS reply.
            if let Some(code) = maybe_code {
                reply_failure(stream, code.clone()).await;
                let _ = events.send(Event::Error {
                    code: code.reply_code(),
                    msg: "malformed request".to_string(),
                });
            }
            None
        }
    }
}

/// Read from `stream` into a growing buffer, applying `parse` after each chunk.
///
/// `parse` returns `Ok(Some(value))` once a complete message is available,
/// `Ok(None)` when more bytes are needed, and `Err(())` on a fatal parse error.
/// Returns `Err(ReadFail::Parse)` for a complete-but-invalid message and
/// `Err(ReadFail::Closed)` on EOF, IO error, or buffer overflow.
async fn read_message<T, F>(stream: &mut TcpStream, mut parse: F) -> Result<T, ReadFail>
where
    F: FnMut(&[u8]) -> Result<Option<T>, ()>,
{
    let mut buf = Vec::with_capacity(READ_BUF);
    let mut chunk = [0u8; READ_BUF];
    loop {
        match parse(&buf) {
            Ok(Some(value)) => return Ok(value),
            Ok(None) => {}
            Err(()) => return Err(ReadFail::Parse),
        }
        if buf.len() > READ_BUF {
            return Err(ReadFail::Closed);
        }
        match stream.read(&mut chunk).await {
            Ok(0) => return Err(ReadFail::Closed),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return Err(ReadFail::Closed),
        }
    }
}

/// Read and parse a SOCKS5 request, reassembling across multiple TCP reads.
///
/// On success returns the parsed [`Request`]. On failure returns the SOCKS5
/// reply code that should be sent to the client before closing, or `None` when
/// the connection should simply be closed with no reply (EOF, IO error, buffer
/// overflow, or a bad protocol version that has no defined reply code).
///
/// Truncation — whether of the fixed header ([`RequestError::Truncated`]) or of
/// the address bytes ([`AddrError::Truncated`]) — means "need more bytes", so we
/// keep reading instead of treating it as fatal.
async fn read_request(stream: &mut TcpStream) -> Result<Request, Option<Socks5Error>> {
    let mut buf = Vec::with_capacity(READ_BUF);
    let mut chunk = [0u8; READ_BUF];
    loop {
        match parse_request(&buf) {
            Ok(req) => return Ok(req),
            // Both truncation cases need more bytes; keep reading.
            Err(RequestError::Truncated) | Err(RequestError::Addr(AddrError::Truncated)) => {}
            // Unrecognized command byte: command not supported.
            Err(RequestError::BadCommand(_)) => return Err(Some(Socks5Error::CommandNotSupported)),
            // Unknown ATYP: address type not supported.
            Err(RequestError::Addr(AddrError::BadAtyp(_))) => {
                return Err(Some(Socks5Error::AddressNotSupported))
            }
            // Malformed domain is a generic failure, not an unsupported ATYP.
            Err(RequestError::Addr(AddrError::BadDomain)) => {
                return Err(Some(Socks5Error::General))
            }
            // Bad version has no well-defined SOCKS reply; close the connection.
            Err(RequestError::BadVersion(_)) => return Err(None),
        }
        if buf.len() > READ_BUF {
            return Err(None);
        }
        match stream.read(&mut chunk).await {
            Ok(0) => return Err(None),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return Err(None),
        }
    }
}

/// Send a best-effort failure reply with a zeroed IPv4 BND address.
async fn reply_failure(stream: &mut TcpStream, err: Socks5Error) {
    let mut out = Vec::with_capacity(10);
    let bind = Address::V4(Ipv4Addr::UNSPECIFIED, 0);
    encode_reply(err.reply_code(), &bind, &mut out);
    let _ = stream.write_all(&out).await;
}
