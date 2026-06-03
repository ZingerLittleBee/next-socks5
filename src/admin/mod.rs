//! Local admin/attach support: a Unix-socket endpoint that streams metrics and
//! events to an attached TUI client, plus the wire protocol shared by both ends.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::metrics::{ConnInfo, Event, Snapshot};

mod ring;
pub use ring::{EventRing, ADMIN_EVENT_RING_CAPACITY};

mod server;
pub use server::serve;

mod client;
pub use client::{decode_loop, RemoteState};

/// Wire protocol version. Bump on any breaking change to `Frame`.
pub const PROTO_VERSION: u16 = 1;

/// Maximum accepted frame length (1 MiB). Guards against corrupt/oversized
/// length prefixes causing huge allocations.
pub const MAX_FRAME_LEN: u32 = 1 << 20;

/// A single message on the attach connection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Frame {
    /// Handshake, sent first by the server.
    Hello {
        proto: u16,
        listen_addr: Option<String>,
    },
    /// Periodic snapshot of global counters and active connections.
    Stats {
        snapshot: Snapshot,
        connections: Vec<ConnInfo>,
    },
    /// A log/lifecycle event (replayed history or live).
    Event(Event),
}

/// Error reading or writing a frame.
#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame too large: {0} bytes (max {MAX_FRAME_LEN})")]
    TooLarge(u32),
    #[error("decode error: {0}")]
    Decode(#[from] postcard::Error),
}

/// Write one length-prefixed, postcard-encoded frame.
pub async fn write_frame<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    frame: &Frame,
) -> Result<(), FrameError> {
    let body = postcard::to_allocvec(frame)?;
    let len = body.len() as u32;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&body).await?;
    Ok(())
}

/// Read one length-prefixed, postcard-encoded frame.
pub async fn read_frame<R: AsyncReadExt + Unpin>(r: &mut R) -> Result<Frame, FrameError> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(postcard::from_bytes(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::ConnKind;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn addr() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 1080)
    }

    async fn round_trip(frame: Frame) -> Frame {
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &frame).await.expect("write");
        // tokio implements AsyncRead for &[u8], not std::io::Cursor.
        let mut slice: &[u8] = &buf;
        read_frame(&mut slice).await.expect("read")
    }

    #[tokio::test]
    async fn hello_round_trip() {
        let f = Frame::Hello {
            proto: PROTO_VERSION,
            listen_addr: Some("127.0.0.1:1080".into()),
        };
        assert_eq!(round_trip(f.clone()).await, f);
    }

    #[tokio::test]
    async fn stats_round_trip() {
        let f = Frame::Stats {
            snapshot: Snapshot {
                total_conns: 9,
                ..Default::default()
            },
            connections: vec![ConnInfo {
                id: 1,
                src: addr(),
                target: "x:80".into(),
                kind: ConnKind::Connect,
                up: 1,
                down: 2,
            }],
        };
        assert_eq!(round_trip(f.clone()).await, f);
    }

    #[tokio::test]
    async fn event_round_trip() {
        let f = Frame::Event(Event::Closed { id: 3 });
        assert_eq!(round_trip(f.clone()).await, f);
    }

    #[tokio::test]
    async fn oversized_length_is_rejected() {
        // A length prefix above MAX_FRAME_LEN must be rejected before allocating.
        let mut bytes = (MAX_FRAME_LEN + 1).to_be_bytes().to_vec();
        bytes.extend_from_slice(&[0u8; 8]);
        let mut slice: &[u8] = &bytes;
        let err = read_frame(&mut slice).await.unwrap_err();
        assert!(matches!(err, FrameError::TooLarge(_)));
    }
}
