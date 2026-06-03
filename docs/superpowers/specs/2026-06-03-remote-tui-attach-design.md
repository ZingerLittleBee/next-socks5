# 远程 TUI Attach 设计

- 日期：2026-06-03
- 分支：`feat/remote-tui-attach`
- 状态：已批准，待实现计划

## 背景与问题

`next-socks5` 的 TUI 仪表板是**进程内**界面：`tui::run` 直接读取 `Arc<Metrics>`（拉快照）和
`broadcast::Receiver<Event>`（拉事件），与代理服务共享同一进程的内存。

当服务通过 systemd / openrc 以 `--no-tui` headless 模式运行时（见 `install.sh`），没有 TUI。
此时即便在同机另起一个带 TUI 的 `next-socks5`，它也是**独立进程**——会去抢绑同一端口，
即使换端口也只显示自己的空指标，**无法**观察服务进程的流量。进程之间没有任何 IPC，
TUI 无法 attach 到运行中的服务。

本设计新增一个 **attach 模式**：在同机通过 Unix domain socket 连接到运行中的服务，
复用现有 TUI 渲染层显示其实时指标与事件。

## 目标与非目标

**目标**
- 在同机（典型场景：SSH 进 VPS）通过 `next-socks5 attach` 实时查看运行中服务的仪表板。
- 复用现有 TUI 渲染层（`DashboardState` + `widgets::render`），不重写界面。
- 服务端默认常开 attach 端点，开箱即用。
- attach 连上时回放最近的历史事件，避免日志面板空白。
- 体积增量尽量小。

**非目标**
- 远程（跨主机）网络访问、认证、加密——本设计仅本地 Unix socket。
- attach 客户端对服务的控制能力（只读监控，不下发命令）。
- 断线自动重连（MVP 断线即退出并提示）。

## 关键决策

| 决策点 | 选择 | 理由 |
|--------|------|------|
| 连接拓扑 | 本地 Unix domain socket，无认证 | 典型场景是 SSH 进 VPS 本机 attach；零网络暴露最安全简单 |
| 端点启用 | 默认常开 | 开箱即用；本地 socket + 文件权限隔离，风险极小 |
| 事件历史 | 回放最近 500 条 | 连上即有上下文，与现有 `LOG_CAPACITY` 一致；内存成本极小 |
| 序列化格式 | postcard（紧凑二进制） | 本地 socket 无需可读性；体积增量最小（~20–60 KB） |
| 渲染 | 复用现有 TUI | 抽象数据源即可，渲染零改动 |

## 架构

```
                       本地模式（现状）
  Arc<Metrics> ──┐
                 ├──► tui::run ──► widgets::render
  broadcast ─────┘

                       attach 模式（新增）
  服务进程                          attach 进程
  ┌─────────────┐   Unix socket   ┌──────────────────────┐
  │ admin 监听器 │ ◄────────────► │ 解码 task            │
  │ (推送编码帧)│  /run/next-     │  ├► RemoteState      │──► tui::run
  └─────────────┘  socks5/        │  │   (MetricsSource) │   (复用渲染)
  ┌─────────────┐  admin.sock     │  └► 本地 broadcast   │
  │ event ring  │                 │      <Event>         │
  └─────────────┘                 └──────────────────────┘
```

### 数据源抽象

在 `metrics.rs` 新增 trait，把 TUI 的数据来源抽象出来：

```rust
pub trait MetricsSource: Send + Sync {
    fn snapshot(&self) -> Snapshot;
    fn connections(&self) -> Vec<ConnInfo>;
}

impl MetricsSource for Metrics {
    // 转发到已有的同名方法
}
```

`tui::run` 的签名改动（渲染层不动）：

```rust
// 之前: metrics: Arc<Metrics>
// 之后: source: Arc<dyn MetricsSource>
pub async fn run(
    source: Arc<dyn MetricsSource>,
    events: broadcast::Receiver<Event>,
    shutdown_tx: watch::Sender<bool>,
    shutdown_rx: watch::Receiver<bool>,
) -> io::Result<()>
```

- **本地模式**：传 `Arc<Metrics>` 与服务的 `events` 接收端，行为与现状等价。
- **attach 模式**：传 `Arc<RemoteState>`（内部 `Mutex<(Snapshot, Vec<ConnInfo>)>`，由解码 task
  更新）；events 用 attach 进程内一个本地 `broadcast`，解码 task 收到远端 `Event` 就转发进去。
  `events` 参数类型不变，渲染完全复用。

## 线缆协议

每帧 = `u32`（大端长度前缀）+ postcard 编码的 `Frame`：

```rust
pub const PROTO_VERSION: u16 = 1; // 当前协议版本，破坏兼容时递增
pub const MAX_FRAME_LEN: u32 = 1 << 20; // 帧长上限 1 MiB，超限视为协议错误

#[derive(Serialize, Deserialize)]
enum Frame {
    Hello { proto: u16, listen_addr: Option<String> }, // 握手，带协议版本与监听地址
    Stats { snapshot: Snapshot, connections: Vec<ConnInfo> }, // 周期推送
    Event(Event),                                       // 实时 / 回放
}
```

需要给 `Snapshot` / `ConnInfo` / `ConnKind` / `Event` 加 `#[derive(Serialize, Deserialize)]`
（`SocketAddr` 自带 serde 支持）。

连接生命周期：
1. 服务端 accept 后立即发 `Hello`（`proto = PROTO_VERSION`，`listen_addr` 为服务实际监听地址）。
2. 复制 event ring 当前内容，逐条作为 `Event` 帧回放。
3. 之后进入循环：每 250ms 推一个 `Stats` 帧 + 实时收到的 `Event` 帧。

客户端收到 `Hello` 后校验 `proto == PROTO_VERSION`，不匹配则报错退出（提示双方版本），避免跨版本乱码。
读帧时长度前缀超过 `MAX_FRAME_LEN` 视为协议错误，防止损坏/异常长度导致过量内存分配。
`Hello.listen_addr` 用于填充 attach 端 `DashboardState.listen_addr`，显示在 TUI 标题栏。

## 服务端组件（新模块 `src/admin.rs`）

### 共享 event ring
- 一个常驻 task 订阅 `events` broadcast，维护 `Arc<Mutex<VecDeque<Event>>>`，容量由 admin 模块
  自有常量 `ADMIN_EVENT_RING_CAPACITY = 500` 决定（**不**依赖 `tui::LOG_CAPACITY`，两者职责独立，
  数值恰好相同），满则弹出最旧。
- 新 attach 客户端连上时复制其当前内容用于回放。

### admin 监听器
- `UnixListener::bind`；bind 前若目标路径已存在，**仅当它确为 socket 文件**
  （`metadata.file_type().is_socket()`）时才 unlink，避免误删同名普通文件/目录；
  否则报错退出，不静默覆盖。
- 每个客户端连接 `spawn` 一个 handler：
  - 发 `Hello` → 复制 ring 回放 → `select!` {
    250ms ticker 采样 `snapshot()`/`connections()` 推 `Stats`
    | `events.recv()` 推 `Event`
    | `shutdown_rx` 变更则结束 }。
  - 单个客户端出错只记录日志，不影响代理服务或其他客户端。
- 写入帧失败（客户端断开）即结束该 handler。

### 启动时机与配置
- 在 `main.rs`：只要不是 `attach` 子命令（即正常跑服务，无论 TUI 还是 headless），
  就启动 event ring task + admin 监听器（**默认常开**）。
- socket bind 失败仅 `eprintln` 警告，不让主服务退出——attach 是附属能力。
- **关闭开关（保留，默认开启）**：`--no-admin` 命令行开关，以及 config `[admin] enabled`
  （默认 `true`）。成本很低，作为安全/部署逃生口。
- **服务端 socket 路径可配置**：config `[admin] socket = "<path>"` 与 `--admin-socket <path>`，
  默认 `/run/next-socks5/admin.sock`。优先级与现有 config 合并逻辑一致：CLI 覆盖 config，
  config 覆盖默认。服务端只负责在该路径 bind；**父目录的创建与权限由部署层负责**
  （systemd/openrc/docker/manual，见「部署改动」）。

### feature 边界
- admin 服务端**不依赖** ratatui，headless-only build（无 `tui` feature）也能编译并被 attach。
- postcard 依赖始终需要（服务端编码用）。

## 客户端与 CLI

- CLI 引入**可选子命令**（clap `#[command(subcommand)]`）：
  - 无子命令 = 跑服务，**保持现有 `--no-tui --config <path>` 行为不变**，`install.sh` 无需改调用。
  - `attach [--socket <path>]` = attach 模式。
- 默认 socket 路径：`/run/next-socks5/admin.sock`。
- attach 子命令**需要 `tui` feature**（要渲染）；headless-only build 不含该子命令。
- attach 客户端：连 socket → 启动解码 task（读帧、解码、更新 `RemoteState`、转发 `Event` 到本地
  broadcast）→ 调用 `tui::run` 复用渲染。

## 错误处理与边界

| 情况 | 行为 |
|------|------|
| socket 不存在 / 连不上 | 友好报错：`未找到运行中的服务（socket: <path>），服务在运行吗？` |
| 协议版本不匹配 | 报错提示双方版本，退出 |
| 连接中途断开 | TUI 退出并打印 `connection lost`（MVP 不自动重连） |
| 旧 socket 文件残留 | bind 前 unlink |
| 多个 attach 同时连 | 各自独立 handler，互不影响 |
| 客户端写入失败 | 结束该 handler，服务不受影响 |

**attach 端 decode task 的退出契约**：decode task 在遇到 **EOF、IO 错误，或协议错误（解码失败 /
版本不匹配 / 帧长超过 `MAX_FRAME_LEN`）** 时，**必须**触发 TUI 退出——通过 attach 进程内的
`shutdown` watch 通道（复用现有 `tui::run` 的 shutdown 机制）。`tui::run` 经 `TerminalGuard`
恢复终端后，attach 进程在**终端已恢复**的状态下向 stderr 打印 `connection lost`
（不能在 alternate-screen 内打印，否则会被清屏吞掉）。

## 部署改动（`install.sh`）

socket 父目录（默认 `/run/next-socks5`）的创建与权限由各部署场景负责：

- **systemd**：unit 增加
  - `RuntimeDirectory=next-socks5`（自动创建 `/run/next-socks5`，DynamicUser 进程可写，服务停时清理）
  - `RuntimeDirectoryMode=0710`（owner 读写、同组可进入、其他无权；root 仍可绕 DAC 访问）
  - root（SSH 进去）可绕过 DAC 直接连 socket，符合典型 attach 场景。
- **openrc**：init 脚本 `start_pre` 里 `mkdir -p /run/next-socks5 && chmod 0710 /run/next-socks5`
  （openrc 服务以 root 跑，socket 归 root，attach 也以 root 跑，权限自然匹配）。
- **docker**：host 模式下 socket 在容器内的 `/run/next-socks5`（容器以 root 跑，目录由 entrypoint 或
  进程启动前 `mkdir` 创建）；attach 需 `docker exec -it <container> next-socks5 attach`（文档说明）。
- **manual（`--no-service`）**：进程以当前用户跑，默认 `/run/next-socks5` 普通用户通常不可写。
  文档建议改用可写路径，例如 `--admin-socket /tmp/next-socks5.sock`（或 `$XDG_RUNTIME_DIR` 下），
  attach 时用同一 `--socket` 指向它。
- 安装总结新增一行：`实时仪表板: next-socks5 attach`（docker 则给 exec 形式）。

## 测试策略

- **单元**：
  - 每种 `Frame` 的 postcard round-trip（编码再解码相等）。
  - event ring 的 push / 容量上限 / snapshot 复制。
  - `RemoteState` 的 `MetricsSource` 实现读写正确。
- **集成**：
  - 临时 socket 路径起 admin 监听器 + 模拟 metrics，客户端连上，断言收到
    `Hello` → 回放事件 → `Stats` 的序列与内容正确。
- 现有 smoke 测试（CONNECT / UDP / auth）不受影响。

## 依赖与体积

- 新增依赖仅 **postcard**（no_std，轻量）；`serde` / `serde_derive` 已在依赖树。
- 预估二进制增量 **~20–60 KB（约 1–3%）**，2.44 MB → ~2.5 MB（未 strip，与 CI 发布口径一致）。

## 受影响文件

- `src/metrics.rs`：新增 `MetricsSource` trait 与 derive；为数据结构加 serde derive。
- `src/tui/mod.rs`：`run` 签名改为接受 `Arc<dyn MetricsSource>`。
- `src/admin.rs`（新）：event ring、admin 监听器、`Frame` 协议、attach 客户端。
- `src/config.rs`：CLI 子命令；服务端 `--admin-socket` / `--no-admin` 与 `[admin] { enabled, socket }`
  配置；attach 客户端 `--socket`。
- `src/main.rs`：子命令分发；正常模式下启动 admin 监听器。
- `Cargo.toml`：新增 `postcard` 依赖。
- `install.sh`：systemd `RuntimeDirectory` / openrc `mkdir` / 安装总结提示。
</content>
</invoke>
