# next-socks5

[English](README.md) | **简体中文**

[![Build](https://img.shields.io/github/actions/workflow/status/ZingerLittleBee/next-socks5/build.yml?style=for-the-badge&cacheSeconds=3600)](https://github.com/ZingerLittleBee/next-socks5/actions/workflows/build.yml)
[![Release](https://img.shields.io/github/v/release/ZingerLittleBee/next-socks5?style=for-the-badge&cacheSeconds=3600)](https://github.com/ZingerLittleBee/next-socks5/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/ZingerLittleBee/next-socks5/total?style=for-the-badge&cacheSeconds=3600)](https://github.com/ZingerLittleBee/next-socks5/releases)
[![Container](https://img.shields.io/badge/ghcr.io-next--socks5-2496ED?logo=docker&logoColor=white&style=for-the-badge)](https://github.com/ZingerLittleBee/next-socks5/pkgs/container/next-socks5)
[![License](https://img.shields.io/github/license/ZingerLittleBee/next-socks5?style=for-the-badge&cacheSeconds=3600)](LICENSE)
[![Built with Rust](https://img.shields.io/badge/built_with-Rust-000000?logo=rust&logoColor=white&style=for-the-badge)](https://www.rust-lang.org)

一个用 Rust 编写的轻量、可扩展的 **SOCKS5 服务器**(RFC 1928 + RFC 1929),内置
实时终端仪表盘,并提供适合容器的无界面(headless)模式。协议为手写实现,依赖
刻意保持精简。

![next-socks5 仪表盘](snapshot.gif)

## 特性

- **SOCKS5 命令** —— `CONNECT` 与 `UDP ASSOCIATE`(RFC 1928)。`BIND` 按设计以
  应答码 `0x07` 拒绝。
- **认证** —— 无认证(`0x00`)与 用户名/密码(`0x02`,RFC 1929)。
- **地址类型** —— IPv4、IPv6 与 域名(`ATYP` `0x01` / `0x04` / `0x03`),CONNECT
  与 UDP 目标均在服务端做 DNS 解析。
- **完整的 RFC 错误映射** —— 在适用场景下生成每个应答码 `0x00`–`0x08`(例如
  未知命令 → `0x07`,未知地址类型 → `0x08`,连接数超限 → `0x02`,
  拒绝/不可达/超时 由操作系统错误映射而来)。
- **UDP 中继** —— SOCKS5 封装、丢弃 `FRAG != 0`、源 IP 过滤、向客户端通告可达的
  `BND.ADDR`(绝不为 `0.0.0.0`),以及空闲回收。
- **TUI 仪表盘** —— 实时吞吐量与趋势图、可排序的活动连接表、成功/错误统计,以及
  支持键盘导航的可滚动日志(基于 ratatui)。隐藏的 `--mock` 选项会推送合成数据,
  便于在无真实流量的情况下预览/测试界面。
- **无界面模式** —— `--no-tui` 将事件输出到 stdout,适合 systemd / 容器。TUI 是
  可选的 cargo feature,因此无界面构建会完全去掉 ratatui/crossterm 依赖。
- **健壮性** —— 连接 / TCP 空闲 / UDP 空闲 超时、可选的 `max_connections` 限制、
  支持半开连接的中继,以及优雅关闭。
- **配置** —— TOML 配置文件,支持 CLI 覆盖。
- **小巧、可移植** —— 纯 Rust,无 C 依赖;以完全静态的 musl 二进制和约 3.5 MB 的
  `scratch` 镜像发布。

## 安装

### 方式一 —— 一行命令安装器(推荐)

安装器会自动在 **binary** 与 **docker** 之间选择,生成凭证与空闲端口,并启动服务。

```bash
# 二进制安装,启用认证(自动生成用户名/密码),随机端口:
curl -fsSL https://raw.githubusercontent.com/ZingerLittleBee/next-socks5/main/install.sh | sh

# 带参数(注意用 -s -- 把参数透传给 curl | sh):
curl -fsSL https://raw.githubusercontent.com/ZingerLittleBee/next-socks5/main/install.sh \
  | sh -s -- --port 1080
```

或克隆到本地运行。下面每个示例都带注释;在认证模式下若不提供 `--user` / `--pass`,
安装器会生成用户名和一个 20 位密码,并在结束时连同可直接使用的 `socks5://` URL 与
一条 `curl` 测试命令一起打印出来。

```bash
# 列出所有参数并退出
./install.sh --help

# 最简运行:二进制安装,开启认证(自动生成用户名/密码),随机空闲端口
./install.sh

# 用 Docker 而非原生二进制(host 网络,UDP ASSOCIATE 可用)
./install.sh --method docker

# 开放代理(无认证)+ 固定端口 —— 仅限可信网络
./install.sh --method binary --no-auth --port 1080

# 显式指定凭证 + 固定端口
./install.sh --method docker --auth --user alice --pass secret --port 1080

# 绑定到单个网卡而非 0.0.0.0(此处:仅本机)
./install.sh --no-auth --listen 127.0.0.1 --port 1080

# NAT/Docker 后的 UDP 中继:固定中继端口范围并通告公网 IP
./install.sh --port 1080 --udp-port-range 40000-40100 --udp-advertise 203.0.113.42

# 固定某个发布版本,而非 `latest`
./install.sh --version v0.2.0 --port 1080

# 只安装二进制 + 配置 —— 不创建/启动服务
./install.sh --no-service --port 1080            # 等价于:NO_SERVICE=1 ./install.sh --port 1080

# 自定义位置:二进制安装目录(binary)/ compose 部署目录(docker)
./install.sh --bin-dir /opt/bin --port 1080
./install.sh --method docker --dir ./ns5 --port 1080
```

| 选项 | 说明 | 默认 |
|---|---|---|
| `--method <binary\|docker>` | 原生二进制(systemd/OpenRC)或 Docker Compose | `binary` |
| `--auth` / `--no-auth` | 启用用户名/密码认证,或以开放模式运行 | `--auth` |
| `--user` / `--pass` | 认证模式的凭证(省略则随机) | 随机 |
| `--port <port>` | 监听端口(省略则随机选空闲端口) | 随机 |
| `--listen <addr>` | 绑定地址 | `0.0.0.0` |
| `--udp-port-range <range>` | 将 UDP 中继套接字绑定到闭区间端口范围(如 `40000-40100`) | OS 临时端口 |
| `--udp-advertise <ip>` | NAT/Docker 后通告的 BND IP(客户端可达地址) | 绑定地址 |
| `--version <tag>` | 发布版本,如 `v0.1.0` | `latest` |
| `--bin-dir <dir>` | 二进制安装目录(binary 方式) | `/usr/local/bin` |
| `--dir <dir>` | Docker 部署目录(docker 方式) | `./next-socks5-deploy` |
| `--no-service` | 仅安装二进制 + 配置,不创建/启动服务 | 关闭 |

> 二进制安装面向 Linux(musl x86_64 / aarch64),并配置 **systemd** 或 **OpenRC**
> 服务。若两种 init 系统都不存在,则只安装二进制与配置而**不启动**(且重启后不会
> 自启)—— 需手动启动,或改用 `--method docker` 使用可自重启的容器。安装器使用
> POSIX `sh`(无需 bash)。

### 方式二 —— Docker

最快方式 —— 让安装器生成 `docker-compose.yml` + `config.toml` 并替你启动容器
(host 网络;带 `--auth` 且不给 `--user` / `--pass` 时,凭证会自动生成并在结尾
打印):

```bash
curl -fsSL https://raw.githubusercontent.com/ZingerLittleBee/next-socks5/main/install.sh \
  | sh -s -- --method docker --auth --port 1080
```

这会把两个文件写入 `./next-socks5-deploy/`(可用 `--dir` 覆盖)并执行
`docker compose up -d`。若想手动配置:

```bash
# 无认证,host 网络(UDP ASSOCIATE 可用),监听 1080:
docker run -d --name next-socks5 --network host \
  ghcr.io/zingerlittlebee/next-socks5:latest --listen 0.0.0.0:1080
```

使用配置文件(用于认证):

```bash
docker run -d --name next-socks5 --network host \
  -v "$PWD/config.toml:/etc/next-socks5/config.toml:ro" \
  ghcr.io/zingerlittlebee/next-socks5:latest --config /etc/next-socks5/config.toml
```

或使用 Compose(`docker-compose.yml`):

```yaml
services:
  next-socks5:
    image: ghcr.io/zingerlittlebee/next-socks5:latest
    container_name: next-socks5
    restart: unless-stopped
    network_mode: host
    volumes:
      - ./config.toml:/etc/next-socks5/config.toml:ro
    # admin/attach socket 的可写运行时目录 —— 镜像以非特权用户运行,自己无法创建
    # /run/next-socks5。缺少这一段时,`docker exec ... next-socks5 attach` 无法连接。
    tmpfs:
      - /run/next-socks5
    command: ["--config", "/etc/next-socks5/config.toml"]
```

```bash
docker compose up -d
```

镜像为多架构(`linux/amd64`、`linux/arm64`),同时打上发布版本(如 `0.1.0`)与
`latest` 标签。容器始终以无界面方式运行。

### 方式三 —— 预编译二进制

从 [Releases](https://github.com/ZingerLittleBee/next-socks5/releases) 页面下载静态
musl 构建:

```bash
curl -fL -o next-socks5.tar.gz \
  https://github.com/ZingerLittleBee/next-socks5/releases/latest/download/next-socks5-x86_64-unknown-linux-musl.tar.gz
tar xzf next-socks5.tar.gz
./next-socks5-x86_64-unknown-linux-musl/next-socks5 serve --no-tui --listen 0.0.0.0:1080
```

(ARM64 把 `x86_64` 换成 `aarch64`。)

### 方式四 —— 从源码构建

需要较新的 stable Rust 工具链。

```bash
git clone https://github.com/ZingerLittleBee/next-socks5
cd next-socks5
cargo build --release
./target/release/next-socks5 serve            # TUI 仪表盘
./target/release/next-socks5 serve --no-tui   # 无界面

# 仅无界面构建(去掉 TUI 依赖):
cargo build --release --no-default-features
```

或直接从 git 安装:

```bash
cargo install --git https://github.com/ZingerLittleBee/next-socks5
```

## 配置

配置为 TOML 文件(参见 [`config.example.toml`](config.example.toml));CLI 选项会
覆盖文件中的值。

```toml
listen = "0.0.0.0:1080"

[auth]
method = "password"        # "none" | "password"
# 一个或多个凭证 —— 每个用户加一个 [[auth.users]] 块。
[[auth.users]]
username = "alice"
password = "secret"

[[auth.users]]
username = "bob"
password = "hunter2"

[timeouts]
handshake_ms = 10000       # 问候+认证+请求 的截止时间(防 slowloris)
connect_ms = 10000
tcp_idle_ms = 300000
udp_idle_ms = 60000

[limits]
max_connections = 2048     # 可选:全局并发上限(不设则无限)
max_per_ip = 64            # 可选:单源 IP 并发上限(不设则无限)

[admin]
enabled = true             # 本地 attach 端点(默认开启)
# socket = "/run/next-socks5/admin.sock"   # 覆盖 socket 路径
```

**多用户。** 使用 `method = "password"` 时,每个凭证添加一个 `[[auth.users]]`
块 —— 只要客户端的用户名/密码匹配列表中的**任意一项**即被接受(RFC 1929)。这是
从单端口服务多个用户的推荐方式,无需为每个用户单独开端口。使用 `method = "none"`
时代理为开放模式,`users` 列表被忽略。(仪表盘会把每次认证记录为
`auth ok/failed for '<user>'`;按用户的流量统计尚未在连接表中展示。)

**连接限制。** `[limits]` 下的两个上限都是**可选、默认无限**;服务器在 accept
阶段就强制执行,因此半开/握手中的连接也会计入。它们不会自动设置 —— 需在配置中
显式开启:

- `max_connections` —— 全局并发连接上限;用于兜底,防止文件描述符 / 任务耗尽。
  请按主机规格设置(操作系统的 `RLIMIT_NOFILE` 是最终上限;每条 CONNECT 中继约
  占用 2 个 fd)。
- `max_per_ip` —— 单个源 IP 的并发连接数。阻止单个客户端独占代理或以高并发爆破
  凭证。宽松的取值(如 64–256)不会影响正常客户端;仅当你预期单个 NAT 后没有很多
  用户时才调低。

对于**面向公网**的部署,两个都应设置。代理没有内置的认证限速,因此公网暴露时还应
在监听端口前加主机防火墙 / fail2ban。

**安全默认值。** Egress(出站)过滤**默认开启**:代理拒绝中继到 环回、链路本地
(含 `169.254.169.254` 云元数据地址)以及 私网/RFC1918 段(用于防 SSRF / 开放
转发)。若你确实需要访问内网目标,可通过 `[egress]` 段放开 —— 参见
[`config.example.toml`](config.example.toml)。中继前的握手受 `timeouts.handshake_ms`
(默认 10 秒)限制,以丢弃 slowloris 式的卡住客户端。

### UDP 中继与 NAT / Docker

`CONNECT` 在单个 TCP 监听端口上工作,但 **UDP ASSOCIATE** 使用独立的 UDP 中继套接字。默认情况下,每个关联(association)会绑定一个由操作系统分配的临时 UDP 端口,服务器会通告一个 `BND.ADDR:BND.PORT` 地址,客户端**必须**将其数据报发送到该地址(RFC 1928)。两个 `[udp]` 选项让这一机制能够穿透防火墙和 NAT:

```toml
[udp]
port_range = "40000-40100"   # 将 UDP 中继套接字绑定到此闭区间范围
advertise  = "203.0.113.42"  # 通告的 BND IP(客户端可达的地址)
```

- **`port_range`** —— 将每个关联的 UDP 套接字绑定到已知范围内,而非随机的临时端口,这样防火墙/NAT 只需开放该范围即可。每个关联会绑定各自的套接字,因此范围大小应 **≥ 预期的并发 UDP 客户端数量**;`"40000-40000"` 只有一个端口,会导致 UDP 串行化。当范围耗尽时,UDP ASSOCIATE 会返回通用失败(general failure)应答。
- **`advertise`** —— 写入 UDP ASSOCIATE 应答中的 IP。默认情况下,服务器会通告客户端 TCP 连接抵达时所用的服务器侧 IP(即控制套接字的本地地址);当该 IP 对客户端不可达时(例如服务器位于 NAT 之后,或使用 Docker 桥接网络),请覆盖此项。通告的**端口始终是真实绑定的端口**,因此任何 NAT/转发都必须**端口保持一致(1:1)**。通告地址不可达是「TCP 可用但 UDP 不可用」的头号原因。可接受裸 IP 或 `ip:port` 格式(端口会被忽略);格式错误的值会在启动时被拒绝,而不会被静默忽略。

**Docker。** 随附的 compose 配置使用 `network_mode: host`(Linux),无需任何端口映射。若使用桥接网络,请使用**短语法**发布 TCP 控制端口和 UDP 范围(Compose 长语法不支持范围),并将 `advertise` 设置为宿主机的公网 IP:

```yaml
ports:
  - "1080:1080/tcp"
  - "40000-40100:40000-40100/udp"
```

在默认的 userland 代理下,请保持范围较小(Docker 会为每个已发布端口启动一个 `docker-proxy` 进程);对于较大的范围,请设置 `userland-proxy=false` 或改用 host 网络。

**防火墙**(范围 `40000-40100/udp` + 控制端口 `1080/tcp`):

```bash
# ufw
ufw allow 1080/tcp && ufw allow 40000:40100/udp
# nftables
nft add rule inet filter input udp dport 40000-40100 accept
# iptables
iptables -A INPUT -p udp --dport 40000:40100 -j ACCEPT
```

**端口重映射(PAT / 对称型)NAT** 无法与多端口范围协同工作(转换后所通告的内部端口是错误的)。请使用单个固定端口(`"40000-40000"`)并对该端口做 1:1 转发,或将服务部署在可直接访问的公网 IP 上。

### 命令行

```
next-socks5                        打印帮助(裸命令不会启动服务器)
next-socks5 serve [OPTIONS]        运行服务器(别名:run)
next-socks5 attach [OPTIONS]       attach 到运行中的服务器仪表盘

服务器选项:
  --config <path>       TOML 配置文件路径
  --listen <addr>       覆盖监听地址(如 0.0.0.0:1080)
  --no-tui              无界面运行(事件输出到 stdout),而非仪表盘
  --no-admin            禁用本地 admin/attach 端点
  --admin-socket <path> 覆盖 admin socket 路径
  -h, --help            打印帮助

attach 选项:
  --socket <path>       要连接的 admin socket
                        (默认 /run/next-socks5/admin.sock)
```

## 使用

```bash
# 测试无认证代理:
curl --socks5 127.0.0.1:1080 https://example.com

# 测试密码认证代理:
curl --socks5 alice:secret@127.0.0.1:1080 https://example.com
```

### 仪表盘(TUI)

终端仪表盘默认开启 —— 只要运行服务器时不加 `--no-tui`:

```bash
next-socks5 serve --listen 127.0.0.1:1080
```

它展示实时吞吐量(含 30 秒趋势图)、成功/错误统计、可排序的 **活动连接** 表,以及
滚动的 **日志**。按键:

| 按键 | 动作 |
|---|---|
| `Tab` | 在连接表与日志之间切换滚动焦点(高亮当前聚焦面板) |
| `s` | 循环连接排序键:`ID` → `UP↓` → `DOWN↓` → `AGE↓`(显示在表标题中) |
| `↑` / `↓` 或 `k` / `j` | 聚焦面板滚动一行 |
| `PgUp` / `PgDn` | 聚焦面板滚动一屏 |
| `q` / `Ctrl-C` | 退出 |

#### 用合成数据预览 / 测试仪表盘

要在不发送任何真实流量的情况下体验仪表盘,加上 `--mock`。它用一串合成的连接、
吞吐量和错误来驱动代理所用的同一套指标与事件总线 —— 便于试用排序/滚动按键或截图。
一旦退出,假活动立即停止。

```bash
# 本地预览:打开仪表盘并持续生成模拟数据。
cargo run --release -- serve --listen 127.0.0.1:1080 --mock

# 或使用已安装的二进制:
next-socks5 serve --listen 127.0.0.1:1080 --mock
```

`--mock` 仅用于演示/测试;切勿在真实代理上启用。

### Attach 到运行中的服务

通过 systemd / OpenRC / Docker 安装的服务以**无界面**运行(自身没有 UI),但仍通过
本地 Unix socket(默认 `/run/next-socks5/admin.sock`)提供实时仪表盘。要观察一个
**已在运行**的服务器,从同一台机器 attach 即可 —— 无需重启、无需开启任何选项;该
端点默认开启。

```bash
# 1. SSH 登录运行该服务的主机(默认 socket 需 root):
ssh root@your-server

# 2. Attach —— 默认 socket /run/next-socks5/admin.sock:
next-socks5 attach

# Docker:改为在容器内运行 attach:
docker exec -it next-socks5 next-socks5 attach
```

若服务使用了非默认的 socket 路径(例如自定义路径的手动安装),用 `--socket` 指向
它:

```bash
next-socks5 attach --socket /tmp/ns5.sock
```

该端点仅限本地(不暴露网络、无认证)且只读 —— attach 客户端只能观察,不能控制
服务器。按 `q` 断开;若服务器停止,仪表盘以 `connection lost` 退出。

> 默认 socket 位于 `/run/next-socks5`(权限 `0710`,属主为服务用户)。`root` 始终
> 可以 attach;非 root 用户只能 attach 自己拥有的 socket(例如 `/tmp` 或
> `$XDG_RUNTIME_DIR` 下的手动安装)。
>
> **Docker:** 容器以非特权用户(uid `65534`)运行,需要一个可写的 `/run/next-socks5`
> 来放 admin socket。安装器生成的 Compose(以及上面的示例)通过 `tmpfs` 提供;裸
> `docker run` 需加 `--tmpfs /run/next-socks5`。否则服务会记录
> `admin endpoint disabled: Permission denied`,attach 无法连接。

用 `--no-admin` 或 `[admin] enabled = false` 禁用该端点。

对于手动安装(`--no-service`),进程以你的用户身份运行,默认的 `/run` 路径通常
不可写。用一个可写的 socket 启动服务器,并 attach 到同一路径:

```bash
next-socks5 serve --no-tui --admin-socket /tmp/ns5.sock
next-socks5 attach --socket /tmp/ns5.sock
```

## 性能

在单台 4 核云主机(loopback)上,next-socks5 中继吞吐 **~2 GB/s**、每请求附加延迟
**~1.6 ms**、新建连接 **~6k/s**;剖析显示代理受内核/网络栈而非自身限制、无锁争用
—— 即代理本身不是瓶颈。方法论、可复跑脚本([`tests/scripts/`](tests/scripts/))与
完整数据见 [`docs/PERFORMANCE.md`](docs/PERFORMANCE.md)。

## 许可证

见 [LICENSE](LICENSE)。
