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

### A. Global registry `Mutex` on the per-chunk byte-count path — MEASURED 2026-07-07

`CLAUDE.md` described metrics as "atomic counters (hot path, per-byte)", but `add_up`/`add_down`
also take the **global** `registry: Mutex<HashMap>` on every call
(`src/metrics.rs:159-173`), once per relayed chunk from both relay directions
(`src/server/connect.rs:298-299`) and per UDP datagram (`src/server/udp.rs:205`, `:218`).

**Experiment 1 — contention micro-benchmark (10-core Apple M-series, rustc 1.93, `-O`).**
Threads hammer `add_up` on distinct connection ids, current design (atomics + `Mutex<HashMap>`
update) vs. the "obvious fix" (per-connection `Arc<AtomicU64>` + shared global atomic):

| threads | current (Mops/s) | per-conn atomics (Mops/s) |
| --- | --- | --- |
| 1 | 71.9 | 457.5 |
| 4 | 21.3 | 26.5 |
| 8 | 26.2 | 17.4 |
| 16 | 26.2 | 18.1 |

**Verdict: REFUTED as a throughput bottleneck.** The mutex does serialize (negative scaling
1→4 threads), but even fully contended it sustains ~26 M ops/s, while the relay at 2 GB/s in
16 KiB chunks generates only ~128 **k** ops/s — a ~200× margin. Two further lessons:
(a) the per-chunk work between calls (a read+write syscall pair) makes real contention far
lower than the zero-work loop above; (b) the "obvious fix" is *worse* under heavy contention —
the shared global `AtomicU64` cache-line ping-pong costs more than parked mutex waiters. Any
future fix must shard the global counters too, and only after a workload shows ≥ Mops/s rates
(e.g. tiny-chunk relays), which syscall cost makes unlikely.

**Experiment 2 — the risk that IS real: `connections()` clone-under-lock.** The TUI/admin
observer clones the whole registry under the same mutex every 250 ms
(`src/metrics.rs:212-214`, sampled by the 250 ms tick). Measured clone time
(same host, `String` fields populated):

| registry size | clone under lock |
| --- | --- |
| 1k conns | 0.07 ms |
| 10k conns | 0.72 ms |
| 100k conns | 8.7 ms |
| 250k conns | 25.8 ms |

At the 250k-connection scale `docs/PERFORMANCE.md` reports as stable, an attached TUI/admin
holds the lock ~26 ms, 4×/s. `add_up` uses a **blocking** `std::sync::Mutex` on tokio worker
threads, so during each clone every relay task that touches metrics parks its worker thread —
with enough relays this stalls the whole runtime for the clone duration (a periodic p99 latency
spike, observer-induced). Headless with no admin client attached, nothing calls
`connections()`, so there is no effect.

- **Fix direction (revised):** per-connection `Arc<(AtomicU64, AtomicU64)>` still helps, but
  the justification is removing relays from the observer's lock (not throughput): `add_up`
  becomes lock-free, and `connections()` can clone without stalling relays. Keep the global
  counters as single atomics (fine at realistic call rates per Experiment 1).
- **Priority: low-medium** — only manifests with an observer attached at ≥ ~50k connections.

### B. `tokio::io::split` locks on every poll; `TcpStream::split` is free

The relay splits both streams with the generic `tokio::io::split`
(`src/server/connect.rs:214-215`). In tokio 1.52.3 the generic split wraps the stream in a
`Mutex` and acquires it on **every** `poll_read`/`poll_write`/`poll_flush`
(`tokio-1.52.3/src/io/split.rs:54-143`). `TcpStream::split()` is the specialized, borrow-based
split tokio documents as "more efficient than `into_split`"
(`tokio-1.52.3/src/net/tcp/stream.rs:1408`) — it needs no lock because the halves borrow the
stream. Cost today: 2 streams × 2 lock ops per chunk on top of finding A.

- **Status: applied 2026-07-07** — swapped to `client.split()` / `upstream.split()` (drop-in
  for this code shape); full test suite passes. Uncontended mutexes are cheap, so the win is
  small single-digit % at most — but it was free to take.

### C. `TCP_NODELAY` is never set

No `set_nodelay` call exists anywhere in `src/`. Both the client-facing socket and the upstream
dial are Nagle-enabled, so small request/response exchanges relayed through the proxy can pick
up Nagle/delayed-ACK stalls (classically up to ~40 ms per stall on WAN paths; invisible on
loopback where RTT ≈ 0). Every mainstream proxy (HAProxy, Envoy, nginx stream) sets NODELAY on
relay sockets.

- **Status: applied 2026-07-07** — `set_nodelay(true)` on the accepted socket
  (`src/server/mod.rs`, post-accept) and the upstream (`src/server/connect.rs`, post-connect);
  full test suite passes. The latency win is only measurable across a real (non-loopback) RTT
  path — validating it belongs to the two-host bench run.
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

**Status: measured & applied 2026-07-07** — the sweep (8/16/64/256 KiB, `-mode thr`) found
64 KiB ~15–25% faster than 16 KiB at 8 and 64 streams, with 256 KiB regressing at low
concurrency; the default is now 64 KiB (`src/server/connect.rs`). The memory fear did not
materialize: `-mode hold` at 8k idle connections showed ~22 KB RSS/conn — buffer pages become
resident only when traffic writes them. Numbers and caveats in `docs/PERFORMANCE.md`
("Relay buffer size").

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

- **Status: partially measured 2026-07-07** — `tests/scripts/socks5_udp.go` now exists and the
  first numbers are in `docs/PERFORMANCE.md`: lossless relay capacity ~60k datagrams/s on a
  10-core loopback host, **identical at 64 B and 1400 B payloads** — direct evidence that the
  ceiling is per-datagram overhead (findings 1 and 3), not bandwidth. Past the knee goodput
  collapses (replies compete with the client flood for the same relay-socket queue).
- **Fixes applied 2026-07-07:** finding 2 — per-association DNS TTL cache (30 s, 256-entry
  cap, `DnsCache` in `src/server/udp.rs`): domain-target capacity went from ~5k pps
  (resolver-bound, RTT p50 4.7 s at the IP knee's load) to ~60k pps matching IP literals —
  a 12× improvement, numbers in `docs/PERFORMANCE.md`. Finding 1 — the reply-path encap
  scratch buffer is now reused across datagrams and `decap_ref` borrows the payload instead
  of copying (`src/protocol/udp.rs`); IP literals also skip the per-datagram resolve timer
  entirely. Remaining: alloc-rate profiling to confirm nothing per-datagram is left.

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
| 1 | ~~UDP bench tool + first-ever UDP numbers~~ **done 2026-07-07** (`socks5_udp.go`; ~60k pps lossless knee, per-datagram-overhead-bound — see `docs/PERFORMANCE.md`) | harness | unlocks E entirely | — |
| 2 | ~~Metrics mutex as throughput bottleneck~~ **refuted by experiment**; remaining issue is observer clone-under-lock stalls (A, Experiment 2) | code, if TUI/admin used at ≥50k conns | low-medium | S |
| 3 | ~~Buffer-size sweep + concurrent-capacity ramp (D + §7.5)~~ **done 2026-07-07** (64 KiB default, +15–25%; `-mode thr`/`-mode hold` added) | harness + tuning | medium | — |
| 4 | ~~`TCP_NODELAY` on both relay sockets (C)~~ **done 2026-07-07** (WAN validation pending) | code | high for WAN request/response latency | — |
| 5 | ~~`TcpStream::split` instead of `tokio::io::split` (B)~~ **done 2026-07-07** | code | small, free | — |
| 6 | ~~UDP DNS cache + encap buffer reuse (E)~~ **done 2026-07-07** (12× domain-target pps; see `docs/PERFORMANCE.md`) | code | high for domain-target UDP | — |
| 7 | Latency-under-load mode (§7.4) | harness | measurement fidelity | S |
| 8 | `SO_REUSEPORT` multi-accept (F) | code | unknown until two-host test | M |

## Linux verification (2026-07-07)

All findings above were tuned on macOS and re-verified on a Debian 13 x86_64 VM
(4 vCPU) running the shipped `x86_64-unknown-linux-musl` static binary:

- **Correctness:** all 134 tests (unit + integration + `reproductions.rs`
  security suite) pass on musl — the cross-compiled release artifact behaves
  identically to the dev build.
- **TCP throughput:** 3.3 GB/s at c=8 on Linux loopback (vs 1.78 GB/s on the
  macOS host), confirming the 64 KiB buffer + `TCP_NODELAY` changes carry over.
- **DNS cache (finding E) is platform-sensitive:** the 12× headline is a
  macOS-slow-resolver artifact. On Linux with `/etc/hosts` the cache buys ~1.8×
  throughput and ~100× tail latency at saturation (36 ms → 0.4 ms p50). The
  real win tracks resolver latency, so network-resolved domains benefit most.
  Full A/B table in `docs/PERFORMANCE.md`.

## Sources

- RFC 9411, "Benchmarking Methodology for Network Security Device Performance", §7.1–7.5 —
  https://www.rfc-editor.org/rfc/rfc9411 (fetched 2026-07-07)
- RFC 2647 (terminology), RFC 3511 (obsoleted by 9411) — already cited in `docs/PERFORMANCE.md`
- tokio 1.52.3 source (locked version per `Cargo.lock`): `src/io/split.rs:54-143` (mutex per
  poll in generic split), `src/net/tcp/stream.rs:1408` (`split()` efficiency note),
  `src/io/util/copy.rs` (`DEFAULT_BUF_SIZE` = 8 KiB)
- Repository sources at commit `beeb60c` (paths cited inline)
