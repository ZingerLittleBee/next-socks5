# 远程 TUI Attach 实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让用户在同机通过 `next-socks5 attach` 经本地 Unix socket 连接运行中的服务，复用现有 TUI 渲染层查看实时指标与事件。

**Architecture:** 把 TUI 的数据来源抽象成 `MetricsSource` trait；服务端默认常开一个 Unix socket admin 监听器，按 postcard 帧推送 `Hello`/`Stats`/`Event`；attach 客户端解码后填充本地 `RemoteState`（实现 `MetricsSource`）并转发事件到进程内 broadcast，喂给现有 `tui::run`。

**Tech Stack:** Rust 2021、tokio（已含 `net`/`sync`）、postcard（新增）、serde（已有）、clap（已有）、ratatui/crossterm（已有，`tui` feature）。

**设计依据：** `docs/superpowers/specs/2026-06-03-remote-tui-attach-design.md`

---

## 文件结构

| 文件 | 职责 | 动作 |
|------|------|------|
| `Cargo.toml` | 新增 `postcard` 依赖 | 修改 |
| `src/metrics.rs` | 数据结构加 serde derive；新增 `MetricsSource` trait | 修改 |
| `src/admin/mod.rs` | admin 模块入口：常量、`Frame`、帧编解码、re-export | 新建 |
| `src/admin/ring.rs` | `EventRing` 事件环形缓冲 | 新建 |
| `src/admin/server.rs` | admin 监听器 + 单客户端 handler | 新建 |
| `src/admin/client.rs` | `RemoteState` + decode task + attach 入口 | 新建 |
| `src/tui/mod.rs` | `run` 签名改为 `Arc<dyn MetricsSource>` + `listen_addr` 参数 | 修改 |
| `src/config.rs` | CLI 子命令、`AdminConfig`、`--admin-socket`/`--no-admin`/attach `--socket` | 修改 |
| `src/main.rs` | 子命令分发；服务模式启动 admin 监听器 | 修改 |
| `src/lib.rs` | `pub mod admin` | 修改 |
| `install.sh` | systemd `RuntimeDirectory`、openrc `mkdir`、安装总结提示 | 修改 |
| `README.md` | attach 使用说明、docker exec | 修改 |

**约定：** `admin/client.rs` 中需要 ratatui 的部分（attach 入口的 `tui::run` 调用）用 `#[cfg(feature = "tui")]` 守卫；`RemoteState`、decode 逻辑、`Frame`、`server` 不依赖 ratatui，headless build 也能编译并被 attach。

---

## Task 1: 加 postcard 依赖 + 数据结构 serde derive

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/metrics.rs`（`ConnKind`/`ConnInfo`/`Snapshot`/`Event` 的 derive）
- Test: `src/metrics.rs`（`#[cfg(test)] mod tests`）

- [ ] **Step 1: 加依赖**

`Cargo.toml` 的 `[dependencies]` 末尾追加：

```toml
postcard = { version = "1", features = ["use-std"] }
```

- [ ] **Step 2: 写失败测试**

在 `src/metrics.rs` 的 `mod tests` 内追加（先不加 derive，测试应编译失败）：

```rust
#[test]
fn event_postcard_round_trip() {
    let ev = Event::Connect {
        id: 7,
        src: addr(),
        target: "example.com:443".into(),
        kind: ConnKind::Udp,
    };
    let bytes = postcard::to_allocvec(&ev).expect("encode");
    let back: Event = postcard::from_bytes(&bytes).expect("decode");
    assert_eq!(format_event(&ev), format_event(&back));
}

#[test]
fn snapshot_postcard_round_trip() {
    let snap = Snapshot {
        bytes_up: 1,
        bytes_down: 2,
        total_conns: 3,
        active_conns: 4,
        successes: 5,
        failures: 6,
        error_codes: [1, 0, 0, 0, 0, 0, 0, 0, 2],
    };
    let bytes = postcard::to_allocvec(&snap).expect("encode");
    let back: Snapshot = postcard::from_bytes(&bytes).expect("decode");
    assert_eq!(snap.bytes_up, back.bytes_up);
    assert_eq!(snap.error_codes, back.error_codes);
}
```

为支持 `assert_eq!` 比较，给 `Snapshot` 和 `Event` 增加 `PartialEq` 也可，但此处用 `format_event` 比较 `Event`、字段比较 `Snapshot`，无需新增 `PartialEq`。

- [ ] **Step 3: 运行测试验证失败**

Run: `cargo test --lib metrics::tests::event_postcard_round_trip`
Expected: 编译失败，`Event` 未实现 `Serialize`/`Deserialize`。

- [ ] **Step 4: 加 derive**

在 `src/metrics.rs` 顶部 import 处加：

```rust
use serde::{Deserialize, Serialize};
```

给四个类型加 derive（保留现有 derive）：

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnKind { ... }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnInfo { ... }

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot { ... }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event { ... }
```

（`SocketAddr` 在 serde 非人类可读格式即 postcard 下序列化为紧凑二进制，无需额外处理。）

- [ ] **Step 5: 运行测试验证通过**

Run: `cargo test --lib metrics::tests`
Expected: 全部 PASS（含原有测试）。

- [ ] **Step 6: 提交**

```bash
git add Cargo.toml Cargo.lock src/metrics.rs
git commit -m "feat(metrics): add postcard dep and serde derive for wire types"
```

---

## Task 2: MetricsSource trait

**Files:**
- Modify: `src/metrics.rs`
- Test: `src/metrics.rs`（`mod tests`）

- [ ] **Step 1: 写失败测试**

在 `mod tests` 追加：

```rust
#[test]
fn metrics_is_a_source() {
    let m = Metrics::new();
    m.register(addr(), "a:80".into(), ConnKind::Connect);
    let src: std::sync::Arc<dyn MetricsSource> = m.clone();
    assert_eq!(src.snapshot().total_conns, 1);
    assert_eq!(src.connections().len(), 1);
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib metrics::tests::metrics_is_a_source`
Expected: 编译失败，`MetricsSource` 未定义。

- [ ] **Step 3: 定义 trait 并实现**

在 `src/metrics.rs`（`Metrics` 定义之后）加：

```rust
/// Abstract source of dashboard data, so the TUI can render either a local
/// `Arc<Metrics>` or a remote, decoded snapshot.
pub trait MetricsSource: Send + Sync {
    fn snapshot(&self) -> Snapshot;
    fn connections(&self) -> Vec<ConnInfo>;
}

impl MetricsSource for Metrics {
    fn snapshot(&self) -> Snapshot {
        Metrics::snapshot(self)
    }
    fn connections(&self) -> Vec<ConnInfo> {
        Metrics::connections(self)
    }
}
```

- [ ] **Step 4: 运行验证通过**

Run: `cargo test --lib metrics::tests`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add src/metrics.rs
git commit -m "feat(metrics): add MetricsSource trait, impl for Metrics"
```

---

## Task 3: admin 模块 + Frame 协议与帧编解码

**Files:**
- Create: `src/admin/mod.rs`
- Modify: `src/lib.rs`
- Test: `src/admin/mod.rs`（`mod tests`）

- [ ] **Step 1: 注册模块**

`src/lib.rs` 在 `pub mod server;` 后加：

```rust
pub mod admin;
```

- [ ] **Step 2: 写模块骨架 + 失败测试**

新建 `src/admin/mod.rs`：

```rust
//! Local admin/attach support: a Unix-socket endpoint that streams metrics and
//! events to an attached TUI client, plus the wire protocol shared by both ends.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::metrics::{ConnInfo, Event, Snapshot};

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
        let f = Frame::Hello { proto: PROTO_VERSION, listen_addr: Some("127.0.0.1:1080".into()) };
        assert_eq!(round_trip(f.clone()).await, f);
    }

    #[tokio::test]
    async fn stats_round_trip() {
        let f = Frame::Stats {
            snapshot: Snapshot { total_conns: 9, ..Default::default() },
            connections: vec![ConnInfo {
                id: 1, src: addr(), target: "x:80".into(), kind: ConnKind::Connect, up: 1, down: 2,
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
```

`Frame`/`Stats` 比较需要 `Snapshot` 与 `ConnInfo` 实现 `PartialEq`。给 `Snapshot` 和 `ConnInfo` 的 derive 追加 `PartialEq`（`Snapshot` 已可比较其字段；`ConnInfo` 加 `PartialEq`）。`Event` 也需 `PartialEq`——在 `metrics.rs` 给 `Event`、`ConnInfo`、`Snapshot` 的 derive 列表追加 `PartialEq`（`ConnKind` 已有 `PartialEq, Eq`）。

- [ ] **Step 3: 运行验证失败再补 PartialEq**

Run: `cargo test --lib admin::tests`
Expected: 首次因缺 `PartialEq` 编译失败；在 `metrics.rs` 给 `Snapshot`/`ConnInfo`/`Event` 加 `PartialEq` 后通过。

- [ ] **Step 4: 运行验证通过**

Run: `cargo test --lib admin::tests`
Expected: 4 个测试 PASS。

- [ ] **Step 5: 提交**

```bash
git add src/lib.rs src/admin/mod.rs src/metrics.rs
git commit -m "feat(admin): wire protocol Frame with postcard framing"
```

---

## Task 4: EventRing 事件环形缓冲

**Files:**
- Create: `src/admin/ring.rs`
- Modify: `src/admin/mod.rs`（`mod ring;` + re-export）
- Test: `src/admin/ring.rs`（`mod tests`）

- [ ] **Step 1: 挂模块**

`src/admin/mod.rs` 顶部模块声明处加：

```rust
mod ring;
pub use ring::{EventRing, ADMIN_EVENT_RING_CAPACITY};
```

- [ ] **Step 2: 写失败测试 + 实现**

新建 `src/admin/ring.rs`：

```rust
//! A bounded ring of recent events, used to replay history to a newly attached
//! client. Independent of `tui::LOG_CAPACITY` (separate responsibility).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::metrics::Event;

/// How many recent events to retain for replay.
pub const ADMIN_EVENT_RING_CAPACITY: usize = 500;

/// Shared ring of the most recent events.
#[derive(Clone)]
pub struct EventRing {
    inner: Arc<Mutex<VecDeque<Event>>>,
}

impl EventRing {
    pub fn new() -> Self {
        Self { inner: Arc::new(Mutex::new(VecDeque::with_capacity(ADMIN_EVENT_RING_CAPACITY))) }
    }

    /// Push an event, evicting the oldest when at capacity.
    pub fn push(&self, ev: Event) {
        let mut q = self.inner.lock().unwrap();
        if q.len() == ADMIN_EVENT_RING_CAPACITY {
            q.pop_front();
        }
        q.push_back(ev);
    }

    /// Copy the current contents (oldest first) for replay.
    pub fn snapshot(&self) -> Vec<Event> {
        self.inner.lock().unwrap().iter().cloned().collect()
    }

    /// Spawn a task that fills this ring from the event bus until the sender
    /// drops. Returns the spawned task handle.
    pub fn spawn_filler(&self, mut events: broadcast::Receiver<Event>) -> tokio::task::JoinHandle<()> {
        let ring = self.clone();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(ev) => ring.push(ev),
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }
}

impl Default for EventRing {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_oldest_at_capacity() {
        let ring = EventRing::new();
        for i in 0..(ADMIN_EVENT_RING_CAPACITY as u64 + 5) {
            ring.push(Event::Closed { id: i });
        }
        let snap = ring.snapshot();
        assert_eq!(snap.len(), ADMIN_EVENT_RING_CAPACITY);
        // Oldest 5 evicted: first retained id is 5.
        assert!(matches!(snap.first(), Some(Event::Closed { id: 5 })));
    }

    #[tokio::test]
    async fn filler_collects_from_bus() {
        let (tx, rx) = broadcast::channel(16);
        let ring = EventRing::new();
        let handle = ring.spawn_filler(rx);
        tx.send(Event::Closed { id: 1 }).unwrap();
        tx.send(Event::Closed { id: 2 }).unwrap();
        drop(tx); // closes the channel so the filler task ends
        handle.await.unwrap();
        let snap = ring.snapshot();
        assert_eq!(snap.len(), 2);
    }
}
```

- [ ] **Step 3: 运行验证**

Run: `cargo test --lib admin::ring::tests`
Expected: 2 个测试 PASS。

- [ ] **Step 4: 提交**

```bash
git add src/admin/mod.rs src/admin/ring.rs
git commit -m "feat(admin): bounded event ring with bus filler"
```

---

## Task 5: admin 监听器 + 单客户端 handler

**Files:**
- Create: `src/admin/server.rs`
- Modify: `src/admin/mod.rs`（`mod server;` + re-export `serve`）
- Test: `tests/admin_attach.rs`（集成测试，新建）

- [ ] **Step 1: 挂模块**

`src/admin/mod.rs` 加：

```rust
mod server;
pub use server::serve;
```

- [ ] **Step 2: 实现 serve + handler**

新建 `src/admin/server.rs`：

```rust
//! Unix-socket admin listener: accept attach clients and stream Hello → replay
//! → periodic Stats + live Events to each.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, watch};

use crate::metrics::{Event, MetricsSource};

use super::ring::EventRing;
use super::{write_frame, Frame, PROTO_VERSION};

/// How often a client is pushed a fresh Stats frame.
const PUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Bind `socket_path` and serve attach clients until `shutdown` flips true.
///
/// Bind safety: if the path already exists, it is only unlinked when it is
/// actually a socket file; otherwise this returns an error rather than
/// clobbering an unrelated file/dir.
pub async fn serve(
    socket_path: &Path,
    source: Arc<dyn MetricsSource>,
    events: broadcast::Sender<Event>,
    ring: EventRing,
    mut shutdown: watch::Receiver<bool>,
    listen_addr: Option<String>,
) -> std::io::Result<()> {
    unlink_if_socket(socket_path)?;
    let listener = UnixListener::bind(socket_path)?;

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    let source = source.clone();
                    let events_rx = events.subscribe();
                    let replay = ring.snapshot();
                    let shutdown = shutdown.clone();
                    let listen_addr = listen_addr.clone();
                    tokio::spawn(async move {
                        // Per-client errors (e.g. client disconnect) are ignored:
                        // they must not affect the proxy or other clients.
                        let _ = handle_client(stream, source, events_rx, replay, shutdown, listen_addr).await;
                    });
                }
            }
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
    // Best-effort cleanup of the socket file on shutdown.
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

fn unlink_if_socket(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a socket", path.display()),
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

async fn handle_client(
    mut stream: UnixStream,
    source: Arc<dyn MetricsSource>,
    mut events: broadcast::Receiver<Event>,
    replay: Vec<Event>,
    mut shutdown: watch::Receiver<bool>,
    listen_addr: Option<String>,
) -> Result<(), super::FrameError> {
    write_frame(&mut stream, &Frame::Hello { proto: PROTO_VERSION, listen_addr }).await?;
    for ev in replay {
        write_frame(&mut stream, &Frame::Event(ev)).await?;
    }

    let mut ticker = tokio::time::interval(PUSH_INTERVAL);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let frame = Frame::Stats {
                    snapshot: source.snapshot(),
                    connections: source.connections(),
                };
                write_frame(&mut stream, &frame).await?;
            }
            ev = events.recv() => {
                match ev {
                    Ok(ev) => write_frame(&mut stream, &Frame::Event(ev)).await?,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() { break; }
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 3: 写集成测试**

新建 `tests/admin_attach.rs`：

```rust
//! Integration test for the admin attach endpoint: bind a socket, connect a
//! raw client, and assert the Hello → replay → Stats sequence.

use std::sync::Arc;
use std::time::Duration;

use next_socks5::admin::{read_frame, serve, EventRing, Frame, PROTO_VERSION};
use next_socks5::metrics::{ConnKind, Event, Metrics};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, watch};

#[tokio::test]
async fn attach_receives_hello_replay_and_stats() {
    let dir = std::env::temp_dir().join(format!("ns5-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("admin.sock");

    let metrics = Metrics::new();
    metrics.register("127.0.0.1:5000".parse().unwrap(), "host:80".into(), ConnKind::Connect);
    let (events_tx, _evrx) = broadcast::channel::<Event>(64);
    let ring = EventRing::new();
    ring.push(Event::Log("historic line".into()));
    let (sd_tx, sd_rx) = watch::channel(false);

    let source: Arc<dyn next_socks5::metrics::MetricsSource> = metrics.clone();
    let sock2 = sock.clone();
    let server = tokio::spawn(async move {
        serve(&sock2, source, events_tx, ring, sd_rx, Some("127.0.0.1:1080".into()))
            .await
            .unwrap();
    });

    // Give the listener a moment to bind.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut client = UnixStream::connect(&sock).await.expect("connect");

    // 1) Hello.
    match read_frame(&mut client).await.unwrap() {
        Frame::Hello { proto, listen_addr } => {
            assert_eq!(proto, PROTO_VERSION);
            assert_eq!(listen_addr.as_deref(), Some("127.0.0.1:1080"));
        }
        other => panic!("expected Hello, got {other:?}"),
    }
    // 2) Replayed historic event.
    assert!(matches!(read_frame(&mut client).await.unwrap(), Frame::Event(Event::Log(_))));
    // 3) A periodic Stats frame within a couple ticks.
    let stats = read_frame(&mut client).await.unwrap();
    match stats {
        Frame::Stats { snapshot, connections } => {
            assert_eq!(snapshot.total_conns, 1);
            assert_eq!(connections.len(), 1);
        }
        other => panic!("expected Stats, got {other:?}"),
    }

    sd_tx.send(true).unwrap();
    let _ = server.await;
    let _ = std::fs::remove_dir_all(&dir);
}
```

- [ ] **Step 4: 运行验证**

Run: `cargo test --test admin_attach`
Expected: PASS。

- [ ] **Step 5: 提交**

```bash
git add src/admin/mod.rs src/admin/server.rs tests/admin_attach.rs
git commit -m "feat(admin): unix-socket listener streaming hello/replay/stats"
```

---

## Task 6: RemoteState + decode task（attach 客户端核心）

**Files:**
- Create: `src/admin/client.rs`
- Modify: `src/admin/mod.rs`（`mod client;` + re-export）
- Test: `src/admin/client.rs`（`mod tests`）

- [ ] **Step 1: 挂模块**

`src/admin/mod.rs` 加（`attach` 的 re-export 留到 Task 9，因为该函数 Task 9 才定义）：

```rust
mod client;
pub use client::{decode_loop, RemoteState};
```

- [ ] **Step 2: 实现 RemoteState + decode 循环 + 失败测试**

新建 `src/admin/client.rs`：

```rust
//! Attach client: decode incoming frames into a `RemoteState` (a MetricsSource)
//! and forward events onto a local broadcast bus for the TUI to consume.

use std::sync::{Arc, Mutex};

use tokio::io::AsyncReadExt;
use tokio::sync::{broadcast, watch};

use crate::metrics::{ConnInfo, Event, MetricsSource, Snapshot};

use super::{read_frame, Frame};

/// Latest decoded stats, updated by the decode task and read by the TUI.
pub struct RemoteState {
    inner: Mutex<(Snapshot, Vec<ConnInfo>)>,
}

impl RemoteState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new((Snapshot::default(), Vec::new())) })
    }
    fn set(&self, snapshot: Snapshot, connections: Vec<ConnInfo>) {
        *self.inner.lock().unwrap() = (snapshot, connections);
    }
}

impl Default for RemoteState {
    fn default() -> Self {
        Self { inner: Mutex::new((Snapshot::default(), Vec::new())) }
    }
}

impl MetricsSource for RemoteState {
    fn snapshot(&self) -> Snapshot {
        self.inner.lock().unwrap().0.clone()
    }
    fn connections(&self) -> Vec<ConnInfo> {
        self.inner.lock().unwrap().1.clone()
    }
}

/// Run the decode loop until EOF/io/protocol error, updating `state` and
/// forwarding events onto `events_tx`. On any terminating condition it flips
/// `shutdown` to true and sets `lost` so the caller can print "connection lost".
pub async fn decode_loop<R: AsyncReadExt + Unpin>(
    mut reader: R,
    state: Arc<RemoteState>,
    events_tx: broadcast::Sender<Event>,
    shutdown_tx: watch::Sender<bool>,
    lost: Arc<std::sync::atomic::AtomicBool>,
) {
    loop {
        match read_frame(&mut reader).await {
            Ok(Frame::Stats { snapshot, connections }) => state.set(snapshot, connections),
            Ok(Frame::Event(ev)) => {
                let _ = events_tx.send(ev);
            }
            Ok(Frame::Hello { .. }) => {
                // Unexpected mid-stream Hello: ignore.
            }
            Err(_) => {
                lost.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = shutdown_tx.send(true);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{write_frame, Frame};
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn decodes_stats_into_state_and_forwards_events() {
        // Encode a Stats then an Event into a buffer, feed it through decode_loop.
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &Frame::Stats {
            snapshot: Snapshot { total_conns: 42, ..Default::default() },
            connections: vec![],
        }).await.unwrap();
        write_frame(&mut buf, &Frame::Event(Event::Closed { id: 1 })).await.unwrap();

        let state = RemoteState::new();
        let (ev_tx, mut ev_rx) = broadcast::channel(16);
        let (sd_tx, _sd_rx) = watch::channel(false);
        let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let reader: &[u8] = &buf;
        decode_loop(reader, state.clone(), ev_tx, sd_tx, lost.clone()).await;

        // After EOF the loop should have applied the stats and forwarded the event.
        assert_eq!(state.snapshot().total_conns, 42);
        assert!(matches!(ev_rx.try_recv().unwrap(), Event::Closed { id: 1 }));
        // EOF is a terminating condition: lost flag set.
        assert!(lost.load(Ordering::SeqCst));
    }
}
```

- [ ] **Step 3: 运行验证**

Run: `cargo test --lib admin::client::tests`
Expected: PASS。

- [ ] **Step 4: 提交**

```bash
git add src/admin/mod.rs src/admin/client.rs
git commit -m "feat(admin): RemoteState + frame decode loop for attach client"
```

---

## Task 7: tui::run 改为 MetricsSource + listen_addr 参数

**Files:**
- Modify: `src/tui/mod.rs`
- Modify: `src/main.rs`（更新本地调用）
- Test: 依赖 `cargo build` + 现有测试（TUI 运行时无单元测试）

- [ ] **Step 1: 改 run 签名与内部取数**

`src/tui/mod.rs`：

1. import 改为：

```rust
use crate::metrics::{ConnInfo, Event, MetricsSource, Snapshot};
```

2. `run` 签名：

```rust
pub async fn run(
    source: Arc<dyn MetricsSource>,
    mut events: broadcast::Receiver<Event>,
    shutdown_tx: watch::Sender<bool>,
    mut shutdown_rx: watch::Receiver<bool>,
    listen_addr: Option<String>,
) -> io::Result<()> {
```

3. `DashboardState::new(None)` 改为 `DashboardState::new(listen_addr)`。
4. 函数体内 `metrics.snapshot()` → `source.snapshot()`，`metrics.connections()` → `source.connections()`，`let mut last_snapshot = metrics.snapshot();` → `source.snapshot()`。

- [ ] **Step 2: 更新 main.rs 本地调用**

`src/main.rs` 中现有 `tui::run(metrics.clone(), events_rx, shutdown_tx.clone(), shutdown_rx.clone())` 改为：

```rust
let source: std::sync::Arc<dyn next_socks5::metrics::MetricsSource> = metrics.clone();
if let Err(e) = tui::run(
    source,
    events_rx,
    shutdown_tx.clone(),
    shutdown_rx.clone(),
    Some(listen_str.clone()),
)
.await
```

（`listen_str` 已在 main.rs 计算。）

- [ ] **Step 3: 验证编译与现有测试**

Run: `cargo build && cargo test`
Expected: 编译通过；现有测试全部 PASS。

- [ ] **Step 4: 提交**

```bash
git add src/tui/mod.rs src/main.rs
git commit -m "refactor(tui): run on Arc<dyn MetricsSource> with listen_addr param"
```

---

## Task 8: CLI 子命令 + AdminConfig

**Files:**
- Modify: `src/config.rs`
- Test: `src/config.rs`（`mod tests`）

- [ ] **Step 1: 写失败测试**

在 `src/config.rs` 的 `mod tests` 追加：

```rust
#[test]
fn parses_admin_config() {
    let cfg = Config::from_toml_str(
        "listen = \"x\"\n[admin]\nenabled = false\nsocket = \"/tmp/a.sock\"",
    )
    .expect("should parse");
    assert!(!cfg.admin.enabled);
    assert_eq!(cfg.admin.socket.as_deref(), Some("/tmp/a.sock"));
}

#[test]
fn admin_defaults_enabled() {
    let cfg = Config::from_toml_str("listen = \"x\"").expect("should parse");
    assert!(cfg.admin.enabled);
    assert_eq!(cfg.admin.socket, None);
}

#[test]
fn cli_admin_socket_override() {
    let mut cfg = Config::from_toml_str("listen = \"x\"").unwrap();
    let cli = Cli {
        command: None,
        config: None,
        listen: None,
        no_tui: false,
        no_admin: true,
        admin_socket: Some(PathBuf::from("/run/x.sock")),
    };
    apply_overrides(&mut cfg, &cli);
    assert!(!cfg.admin.enabled);
    assert_eq!(cfg.admin.socket.as_deref(), Some("/run/x.sock"));
}
```

- [ ] **Step 2: 运行验证失败**

Run: `cargo test --lib config::tests::parses_admin_config`
Expected: 编译失败（`admin` 字段、`command`/`no_admin`/`admin_socket` 不存在）。

- [ ] **Step 3: 实现**

在 `src/config.rs`：

1. `Config` 加字段：

```rust
    /// Admin/attach endpoint settings.
    #[serde(default)]
    pub admin: AdminConfig,
```

2. 新增类型：

```rust
/// Admin/attach (local Unix socket) configuration.
#[derive(Debug, Clone, serde::Deserialize, PartialEq, Eq)]
pub struct AdminConfig {
    /// Whether the admin endpoint is enabled (default true).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Override the admin socket path (default `/run/next-socks5/admin.sock`).
    #[serde(default)]
    pub socket: Option<String>,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self { enabled: true, socket: None }
    }
}

fn default_true() -> bool {
    true
}

/// Default admin socket path when none configured.
pub const DEFAULT_ADMIN_SOCKET: &str = "/run/next-socks5/admin.sock";
```

3. `Cli` 加子命令与字段：

```rust
#[derive(Debug, clap::Parser)]
#[command(name = "next-socks5", about = "A lightweight SOCKS5 server")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub listen: Option<String>,
    #[arg(long)]
    pub no_tui: bool,
    /// Disable the local admin/attach endpoint.
    #[arg(long)]
    pub no_admin: bool,
    /// Override the admin socket path.
    #[arg(long)]
    pub admin_socket: Option<PathBuf>,
}

/// Subcommands. With no subcommand, the server runs (default).
#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Attach to a running server and show its dashboard.
    Attach {
        /// Path to the admin socket to connect to.
        #[arg(long)]
        socket: Option<PathBuf>,
    },
}
```

4. `default_config()` 加 `admin: AdminConfig::default(),`。

5. `apply_overrides` 追加：

```rust
    if cli.no_admin {
        cfg.admin.enabled = false;
    }
    if let Some(sock) = &cli.admin_socket {
        cfg.admin.socket = Some(sock.to_string_lossy().into_owned());
    }
```

6. 现有 `mod tests` 中构造 `Cli { ... }` 的三处（`cli_listen_override_replaces_listen`、`cli_override_absent_keeps_listen`）补齐新字段 `command: None, no_admin: false, admin_socket: None`。

- [ ] **Step 4: 运行验证通过**

Run: `cargo test --lib config::tests`
Expected: 全部 PASS。

- [ ] **Step 5: 提交**

```bash
git add src/config.rs
git commit -m "feat(config): attach subcommand and [admin] config"
```

---

## Task 9: attach 入口 + main 分发 + 服务模式启动 admin

**Files:**
- Modify: `src/admin/client.rs`（`attach` 入口，`#[cfg(feature = "tui")]`）
- Modify: `src/main.rs`
- Test: `tests/admin_attach.rs`（追加端到端用例）

- [ ] **Step 1: 实现 attach 入口**

先在 `src/admin/mod.rs` 既有 client re-export 之后追加 `attach` 的 re-export：

```rust
#[cfg(feature = "tui")]
pub use client::attach;
```

然后在 `src/admin/client.rs` 末尾追加（feature 守卫）：

```rust
/// Connect to `socket_path`, validate the handshake, then run the TUI fed by
/// the decoded remote stream. Prints "connection lost" to stderr (after the
/// terminal is restored) if the stream ends unexpectedly.
#[cfg(feature = "tui")]
pub async fn attach(socket_path: &std::path::Path) -> std::io::Result<()> {
    use tokio::net::UnixStream;

    let mut stream = match UnixStream::connect(socket_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "未找到运行中的服务（socket: {}）：{e}\n服务在运行吗？",
                socket_path.display()
            );
            std::process::exit(1);
        }
    };

    // First frame must be Hello; validate protocol version.
    let listen_addr = match read_frame(&mut stream).await {
        Ok(Frame::Hello { proto, listen_addr }) => {
            if proto != super::PROTO_VERSION {
                eprintln!(
                    "协议版本不匹配：服务端 {proto}，本客户端 {}。请使用同版本的 next-socks5。",
                    super::PROTO_VERSION
                );
                std::process::exit(1);
            }
            listen_addr
        }
        Ok(other) => {
            eprintln!("协议错误：期望 Hello，收到 {other:?}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("读取握手失败：{e}");
            std::process::exit(1);
        }
    };

    let state = RemoteState::new();
    let (ev_tx, ev_rx) = broadcast::channel::<Event>(1024);
    let (sd_tx, sd_rx) = watch::channel(false);
    let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Decode task reads the rest of the stream.
    let (reader, _writer) = stream.into_split();
    let decode = tokio::spawn(decode_loop(
        reader,
        state.clone(),
        ev_tx,
        sd_tx.clone(),
        lost.clone(),
    ));

    let source: Arc<dyn MetricsSource> = state;
    let res = crate::tui::run(source, ev_rx, sd_tx, sd_rx, listen_addr).await;

    decode.abort();
    // Terminal is restored by tui::run's guard before we return; print here.
    if lost.load(std::sync::atomic::Ordering::SeqCst) {
        eprintln!("connection lost");
    }
    res
}
```

（`stream.into_split()` 返回 `OwnedReadHalf`，实现 `AsyncReadExt`，满足 `decode_loop` 约束。）

- [ ] **Step 2: main 分发与服务模式启动 admin**

`src/main.rs` 改造：

1. 顶部 import 加 `use next_socks5::config::{AuthMethod, Cli, Command, Config, DEFAULT_ADMIN_SOCKET};` 与 `use next_socks5::admin;` 以及 `use std::path::PathBuf;`。

2. `main()` 解析 CLI 后，最前面分发 attach 子命令（在加载完整服务配置之前，attach 只需 socket 路径）：

```rust
    let cli = Cli::parse();

    // Attach subcommand: connect to a running server, render its dashboard.
    #[cfg(feature = "tui")]
    if let Some(Command::Attach { socket }) = &cli.command {
        let path = socket
            .clone()
            .unwrap_or_else(|| PathBuf::from(DEFAULT_ADMIN_SOCKET));
        if let Err(e) = admin::attach(&path).await {
            eprintln!("attach error: {e}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(not(feature = "tui"))]
    if matches!(cli.command, Some(Command::Attach { .. })) {
        eprintln!("attach 需要启用 tui feature 的构建");
        std::process::exit(1);
    }
```

3. 在 server 启动后（`server_handle` spawn 之后、前端运行之前）启动 admin 监听器（仅服务模式、enabled 时）：

```rust
    // Start the admin/attach endpoint unless disabled.
    if cfg.admin.enabled {
        let sock = cfg
            .admin
            .socket
            .clone()
            .unwrap_or_else(|| DEFAULT_ADMIN_SOCKET.to_string());
        let ring = admin::EventRing::new();
        ring.spawn_filler(events_tx.subscribe());
        let source: Arc<dyn next_socks5::metrics::MetricsSource> = metrics.clone();
        let events_tx2 = events_tx.clone();
        let shutdown_admin = shutdown_rx.clone();
        let listen_for_admin = Some(listen_str.clone());
        tokio::spawn(async move {
            if let Err(e) = admin::serve(
                std::path::Path::new(&sock),
                source,
                events_tx2,
                ring,
                shutdown_admin,
                listen_for_admin,
            )
            .await
            {
                eprintln!("admin endpoint disabled: {e}");
            }
        });
    }
```

- [ ] **Step 3: 端到端集成测试**

在 `tests/admin_attach.rs` 追加（直接复用 decode + RemoteState，模拟客户端读取真实 server 推送，验证 RemoteState 被更新）：

```rust
#[tokio::test]
async fn end_to_end_remote_state_updates() {
    use next_socks5::admin::RemoteState;
    use next_socks5::metrics::MetricsSource;
    use std::sync::atomic::AtomicBool;

    let dir = std::env::temp_dir().join(format!("ns5-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sock = dir.join("admin.sock");

    let metrics = Metrics::new();
    metrics.register("127.0.0.1:6000".parse().unwrap(), "h:80".into(), ConnKind::Connect);
    let (events_tx, _rx) = broadcast::channel::<Event>(64);
    let ring = EventRing::new();
    let (sd_tx, sd_rx) = watch::channel(false);

    let source: Arc<dyn MetricsSource> = metrics.clone();
    let sock2 = sock.clone();
    let server = tokio::spawn(async move {
        serve(&sock2, source, events_tx, ring, sd_rx, None).await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    let stream = UnixStream::connect(&sock).await.unwrap();
    let (reader, _w) = stream.into_split();

    // Skip the Hello frame manually is unnecessary: decode_loop ignores Hello.
    let state = RemoteState::new();
    let (ev_tx, _evrx) = broadcast::channel(64);
    let (dsd_tx, _dsd_rx) = watch::channel(false);
    let lost = Arc::new(AtomicBool::new(false));
    let decode = tokio::spawn(next_socks5::admin::decode_loop(
        reader, state.clone(), ev_tx, dsd_tx, lost,
    ));

    // Within a few ticks the first Stats frame updates the remote state.
    let mut ok = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if state.snapshot().total_conns == 1 {
            ok = true;
            break;
        }
    }
    assert!(ok, "remote state should reflect server stats");

    decode.abort();
    sd_tx.send(true).unwrap();
    let _ = server.await;
    let _ = std::fs::remove_dir_all(&dir);
}
```

（`decode_loop` 已在 Task 6 于 `src/admin/mod.rs` re-export，集成测试可直接以 `next_socks5::admin::decode_loop` 调用，无需 tui feature。）

- [ ] **Step 4: 运行验证**

Run: `cargo test --test admin_attach && cargo build`
Expected: 两个集成测试 PASS；二进制构建成功（含 attach 子命令）。

- [ ] **Step 5: 手动冒烟（可选但推荐）**

```bash
# 终端 A：跑服务（admin socket 默认需要 /run 写权限，本地用 /tmp）
cargo run -- --listen 127.0.0.1:1080 --no-tui --admin-socket /tmp/ns5.sock
# 终端 B：attach
cargo run -- attach --socket /tmp/ns5.sock
```
Expected: 终端 B 显示仪表板，标题栏含 `127.0.0.1:1080`；终端 A 退出后 B 显示 `connection lost`。

- [ ] **Step 6: 提交**

```bash
git add src/admin/mod.rs src/admin/client.rs src/main.rs tests/admin_attach.rs
git commit -m "feat(admin): attach entrypoint, main dispatch, server-side endpoint wiring"
```

---

## Task 10: install.sh / systemd / openrc 部署改动

**Files:**
- Modify: `install.sh`
- Test: `sh -n install.sh`（语法）+ 人工 review

- [ ] **Step 1: systemd unit 加 RuntimeDirectory**

在 `install.sh` 生成 systemd unit 的 here-doc 中（`[Service]` 段，`ExecStart` 附近）加入：

```
RuntimeDirectory=next-socks5
RuntimeDirectoryMode=0710
```

- [ ] **Step 2: openrc 创建运行目录**

在 openrc init 脚本 here-doc 中加入 `start_pre`：

```sh
start_pre() {
    checkpath -d -m 0710 /run/next-socks5
}
```

（`checkpath` 是 openrc 提供的幂等目录创建工具；等效于 `mkdir -p && chmod`。）

- [ ] **Step 3: 安装总结提示**

在打印管理命令/总结的区块追加一行（systemd/openrc 场景）：

```sh
log "实时仪表板: ${BIN_DIR}/${BIN_NAME} attach"
```

docker 场景追加：

```sh
log "实时仪表板: docker exec -it next-socks5 ${BIN_NAME} attach"
```

- [ ] **Step 4: 验证语法**

Run: `sh -n install.sh`
Expected: 无输出（语法正确）。

- [ ] **Step 5: 提交**

```bash
git add install.sh
git commit -m "feat(install): create admin socket runtime dir, show attach hint"
```

---

## Task 11: README 文档

**Files:**
- Modify: `README.md`
- Test: 人工 review

- [ ] **Step 1: 加 attach 章节**

在 `README.md` 适当位置（TUI/usage 附近）新增小节，包含：

- 说明：服务以 headless/服务模式运行时默认监听本地 Unix socket（`/run/next-socks5/admin.sock`）。
- 用法：
  ```bash
  # 同机查看运行中的服务（默认 socket）
  next-socks5 attach

  # 自定义 socket 路径
  next-socks5 attach --socket /tmp/ns5.sock

  # docker 部署
  docker exec -it next-socks5 next-socks5 attach
  ```
- 关闭端点：`--no-admin` 或 config `[admin] enabled = false`。
- manual（`--no-service`）场景：默认 `/run` 可能不可写，用 `--admin-socket /tmp/ns5.sock` 启动并以同路径 attach。

- [ ] **Step 2: 提交**

```bash
git add README.md
git commit -m "docs: document remote TUI attach usage"
```

---

## 完成后

- 运行完整测试套件：`cargo test`（含 `--no-default-features` 验证 headless build 编译：`cargo build --no-default-features`）。
- 运行 smoke 脚本确认代理本体未受影响：`tests/scripts/run_all.sh`。
- 用 superpowers:finishing-a-development-branch 决定合并/PR。
</content>
