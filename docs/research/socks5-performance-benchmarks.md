# SOCKS5 Performance: Benchmark Gaps & Bottleneck Candidates

**Date:** 2026-07-07
**Scope:** Gap analysis of the existing benchmark methodology
([`docs/PERFORMANCE.md`](../PERFORMANCE.md), harness in
[`tests/scripts/`](../../tests/scripts/)) against RFC 9411, plus a code-level review of the
hot paths (`src/server/connect.rs`, `src/server/udp.rs`, `src/metrics.rs`) with a concrete
confirm/refute experiment for each bottleneck candidate. Code references are at commit `beeb60c`;
tokio claims are verified against the locked dependency (tokio **1.52.3** per `Cargo.lock`).

## What already exists (baseline)

`docs/PERFORMANCE.md` covers TCP CONNECT well: throughput, added latency, and CPS per RFC
2647/3511 terminology, a reproducible harness (`bench.sh`, `socks5_cps.go`), a reference run
(~2 GB/s, ~1.6 ms, ~6k CPS on a 4-core loopback host), honest loopback caveats, and a
perf/pidstat/mpstat profile showing the workload is kernel-syscall-bound with no observed lock
contention. This document does **not** repeat any of that; it lists what's missing and what the
existing profile could not have seen.

## Part 1 — Methodology gaps vs. RFC 9411

RFC 9411 (March 2023) obsoletes RFC 3511, which `PERFORMANCE.md` already notes. Its §7 test
suite, mapped to a SOCKS5 proxy:

| RFC 9411 test | Covered today? | Gap |
| --- | --- | --- |
| §7.2 TCP Connections Per Second | ✅ `socks5_cps.go` | — |
| §7.3/§7.1 Throughput | ✅ `bench.sh` (bulk streams) | No mixed-object-size traffic profile; bulk-only overstates real-world goodput |
| §7.4 Transaction Latency | ⚠️ partial | `bench.sh` measures latency mostly **unloaded** (sequential curl). RFC 9411 §7.4 measures latency *while the DUT is under sustained load*; latency under concurrent CPS/throughput load is unmeasured |
| §7.5 Concurrent TCP Connection Capacity | ❌ | No ramp-and-hold test: open N associations/connections, hold them idle-but-alive, find the max N and the RSS/fd cost per connection. Directly relevant here because each connection is a spawned task holding ≥2×16 KiB relay buffers (see Part 2, D) |
| UDP relay performance | ❌ | `smoke_udp.sh` is functional only. No pps, no UDP goodput, no association-churn benchmark — despite UDP ASSOCIATE having the most per-packet userspace work (decap → resolve → egress check → encap, `src/server/udp.rs:121-244`) |

**Recommended harness additions (priority order):**

1. **UDP pps/goodput bench** — extend `socks5_cps.go` (or a sibling `socks5_udp.go`) to do UDP
   ASSOCIATE, then blast N-byte datagrams at a UDP sink; report pps, goodput, and drop rate.
   Datagram sizes: 64 B (pps-bound) and 1400 B (bandwidth-bound).
2. **Concurrent-capacity ramp** (RFC 9411 §7.5): a `-hold` mode in `socks5_cps.go` that opens
   connections and keeps them open; record max concurrent, proxy RSS, and fds at each step.
   Validates `max_connections` sizing and measures per-connection memory (Part 2, D).
3. **Latency-under-load** (RFC 9411 §7.4): run the latency probe *while* the CPS or throughput
   load runs; report p50/p99 degradation vs. the unloaded numbers already in `PERFORMANCE.md`.

## Part 2 — Code-level bottleneck candidates

The existing profile (loopback, 8 bulk streams / CPS churn) showed no userspace bottleneck. The
candidates below are things that profile **could not have seen** — they surface at higher
concurrency, on real NICs, or on the unprofiled UDP path. Each comes with an experiment to
confirm or refute; none should be "fixed" before its experiment shows a win.

### A. Global registry `Mutex` on the per-chunk byte-count path — highest priority

`CLAUDE.md` describes metrics as "atomic counters (hot path, per-byte)", but `add_up`/`add_down`
also take the **global** `registry: Mutex<HashMap>` on every call
(`src/metrics.rs:159-173`), and they are called once per relayed chunk from both relay
directions (`src/server/connect.rs:298-299`) and per UDP datagram
(`src/server/udp.rs:205`, `:218`). At 2 GB/s in 16 KiB chunks that is ~128k global lock
acquisitions/s; with hundreds of concurrent bulk streams every relay task serializes on one
mutex. The reference profile ran only 8 streams — far too few for this to show as `futex` time.

- **Experiment:** re-run the throughput bench sweeping stream count 8 → 64 → 256. If aggregate
  throughput stops scaling and `perf` starts showing `futex`/`__lll_lock_wait`, confirmed.
  A/B against a build where `add_up`/`add_down` skip the registry update.
- **Fix direction (if confirmed):** hand each relay an `Arc<(AtomicU64, AtomicU64)>` at
  `register()` time; the registry holds the same `Arc` and `connections()` reads the atomics.
  Hot path becomes two relaxed atomic adds, zero locks.

### B. `tokio::io::split` locks on every poll; `TcpStream::split` is free

The relay splits both streams with the generic `tokio::io::split`
(`src/server/connect.rs:214-215`). In tokio 1.52.3 the generic split wraps the stream in a
`Mutex` and acquires it on **every** `poll_read`/`poll_write`/`poll_flush`
(`tokio-1.52.3/src/io/split.rs:54-143`). `TcpStream::split()` is the specialized, borrow-based
split tokio documents as "more efficient than `into_split`"
(`tokio-1.52.3/src/net/tcp/stream.rs:1408`) — it needs no lock because the halves borrow the
stream. Cost today: 2 streams × 2 lock ops per chunk on top of finding A.

- **Experiment:** swap to `client.split()` / `upstream.split()` (drop-in for this code shape)
  and A/B the multi-stream throughput bench. Uncontended mutexes are cheap, so expect a small
  single-digit % win at most — but it is free to take.

### C. `TCP_NODELAY` is never set

No `set_nodelay` call exists anywhere in `src/`. Both the client-facing socket and the upstream
dial are Nagle-enabled, so small request/response exchanges relayed through the proxy can pick
up Nagle/delayed-ACK stalls (classically up to ~40 ms per stall on WAN paths; invisible on
loopback where RTT ≈ 0). Every mainstream proxy (HAProxy, Envoy, nginx stream) sets NODELAY on
relay sockets.

- **Experiment:** `stream.set_nodelay(true)` on both the accepted socket
  (`src/server/mod.rs`, post-accept) and the upstream (`src/server/connect.rs:68-90`,
  post-connect), then measure small-object latency percentiles across a real (non-loopback)
  RTT path. Loopback runs will show ~nothing; a WAN/ LAN path is required.
- Also affects the SOCKS reply itself: the success reply (`connect.rs:100-103`) is a 10-byte
  write that Nagle may delay if it coalesces with relay traffic.

### D. Relay buffer size and per-connection memory

Each relay direction uses a 16 KiB stack buffer inside its future (`src/server/connect.rs:274`),
i.e. ≥32 KiB baked into every connection task (tokio's own `copy_bidirectional` defaults to
8 KiB per direction — `tokio-1.52.3/src/io/util/copy.rs`, `DEFAULT_BUF_SIZE`). This is a
throughput/memory trade with two ends:

- **Throughput end:** for bulk relay, larger buffers (64–256 KiB) mean fewer syscalls per byte;
  since the profile shows syscall dominance, a buffer sweep (8/16/64/256 KiB) on the throughput
  bench is the cheapest possibly-large win available.
- **Memory end:** at the §7.5 concurrent-capacity test (Part 1, item 2), buffer size × 2 ×
  connections dominates RSS (100k conns × 32 KiB ≈ 3.2 GB). If capacity matters more than
  bulk speed, the answer may be *smaller*, or lazily allocated on first relayed byte.

Run both experiments together — the ramp test tells you what the sweep costs.

### E. UDP relay path: per-datagram allocations and uncached DNS

The UDP loop does meaningful userspace work per datagram, none of it profiled yet:

1. `decap` copies the payload into a fresh `Vec` (`src/protocol/udp.rs:40`), and each
   target→client reply allocates a fresh framed `Vec` (`src/server/udp.rs:211-212`) — two
   allocations + copies per relayed datagram pair.
2. A domain-target datagram triggers `lookup_host` **every time** — there is no resolution
   cache (`src/server/udp.rs:169-178`). A client sending DNS-over-SOCKS-style traffic to a
   domain target pays a full resolver round trip per datagram, bounded only by
   `timeouts.connect_ms`. This is both a latency cliff and a self-inflicted pps ceiling.
3. One `recv_from` per datagram: tokio's `UdpSocket` exposes no `recvmmsg`/`sendmmsg` batching,
   so high pps means one syscall per datagram each way. Nothing to change in-project without
   `unsafe`/`socket2`; just a known ceiling worth quantifying.

- **Experiment:** requires the UDP bench from Part 1 first. Then: (a) measure pps with IP
  targets (allocation cost) vs. domain targets (DNS cost) — the gap isolates finding 2;
  (b) profile alloc rate under load (`heaptrack` or jemalloc stats) for finding 1. Fix for 2 is
  a small per-association `HashMap<String, (SocketAddr, Instant)>` TTL cache; fix for 1 is
  reusing a scratch encap buffer (the relay loop is single-tasked per association, so one
  reusable `Vec` suffices).

### F. Single accept loop (CPS ceiling on bigger hardware)

All accepts flow through one `tokio::select!` loop that also reaps finished tasks
(`src/server/mod.rs:36-80`). At the measured ~6k CPS the kernel was the bottleneck, but on a
many-core DUT with a real load generator, a single accept task is the classic next ceiling; the
standard fix is N listeners with `SO_REUSEPORT`. **Do not act on this** until the two-host
setup from `PERFORMANCE.md` Step 3 shows the accept loop saturated (one core pinned in the
accept task while others idle) — on current evidence the kernel saturates first.

## Prioritized summary

| # | Item | Type | Expected impact | Effort |
| --- | --- | --- | --- | --- |
| 1 | UDP bench tool + first-ever UDP numbers | harness | unlocks E entirely | S |
| 2 | Metrics registry mutex → per-conn atomics (A) | code, after experiment | high at high concurrency | S |
| 3 | Buffer-size sweep + concurrent-capacity ramp (D + §7.5) | harness + tuning | medium | S |
| 4 | `TCP_NODELAY` on both relay sockets (C) | code | high for WAN request/response latency | XS |
| 5 | `TcpStream::split` instead of `tokio::io::split` (B) | code | small, free | XS |
| 6 | UDP DNS cache + encap buffer reuse (E) | code, after #1 | high for domain-target UDP | S |
| 7 | Latency-under-load mode (§7.4) | harness | measurement fidelity | S |
| 8 | `SO_REUSEPORT` multi-accept (F) | code | unknown until two-host test | M |

## Sources

- RFC 9411, "Benchmarking Methodology for Network Security Device Performance", §7.1–7.5 —
  https://www.rfc-editor.org/rfc/rfc9411 (fetched 2026-07-07)
- RFC 2647 (terminology), RFC 3511 (obsoleted by 9411) — already cited in `docs/PERFORMANCE.md`
- tokio 1.52.3 source (locked version per `Cargo.lock`): `src/io/split.rs:54-143` (mutex per
  poll in generic split), `src/net/tcp/stream.rs:1408` (`split()` efficiency note),
  `src/io/util/copy.rs` (`DEFAULT_BUF_SIZE` = 8 KiB)
- Repository sources at commit `beeb60c` (paths cited inline)
