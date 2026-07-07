# RFC 1928 / RFC 1929 Compliance Audit

**Date:** 2026-07-07
**Scope:** `src/protocol/`, `src/server/`, `src/auth.rs`, `src/error.rs` audited against
[RFC 1928](https://www.rfc-editor.org/rfc/rfc1928) (SOCKS Protocol Version 5) and
[RFC 1929](https://www.rfc-editor.org/rfc/rfc1929) (Username/Password Authentication for SOCKS V5).
Every RFC claim cites the RFC section; every code claim cites `file:line` (at commit `beeb60c`).

**Legend:** ✅ compliant · ⚠️ deviation (explained) · ❌ not implemented · ➖ not applicable (client-side requirement)

## Summary

The implementation is compliant with every wire-format and behavioral **MUST** that applies to a
SOCKS5 *server*, with two exceptions that are deliberate scope decisions rather than bugs:
**GSSAPI** (a formal MUST in RFC 1928 §3) and **BIND** are not implemented. BIND is refused with
the exact reply code the RFC defines for that case (`0x07`), and the GSSAPI omission is shared by
virtually every deployed SOCKS5 implementation. All remaining findings are lenient-parsing notes or
deliberate security hardening that the RFC permits.

## RFC 1928 §3 — Procedure for TCP-based clients (handshake)

| Requirement (RFC 1928 §3) | Verdict | Evidence |
| --- | --- | --- |
| Greeting is `VER NMETHODS METHODS...`, VER = X'05' | ✅ | `src/protocol/handshake.rs:29-37` rejects any `VER != 0x05` (`BadVersion`) |
| Server replies with method selection `VER METHOD` | ✅ | `method_reply` `src/protocol/handshake.rs:58-60`; sent at `src/server/connection.rs:125` |
| Reply X'FF' when no offered method is acceptable | ✅ | `select_method` returns `0xFF` `src/protocol/handshake.rs:44-55`; server sends it, then closes `src/server/connection.rs:128-130` (§3 puts the close-MUST on the *client*; the server closing too is standard practice) |
| Method X'00' NO AUTHENTICATION REQUIRED | ✅ | `METHOD_NO_AUTH` `src/protocol/handshake.rs:7`; selected when auth is not required `src/protocol/handshake.rs:44-49` |
| Method X'02' USERNAME/PASSWORD (SHOULD support) | ✅ | `METHOD_USERPASS` `src/protocol/handshake.rs:9`; full RFC 1929 subnegotiation `src/server/connection.rs:132-162` |
| Method X'01' GSSAPI (**MUST** support) | ❌ | Not implemented anywhere in `src/protocol/handshake.rs`. Formal violation of §3's MUST; see "Deviations" below. |
| Method-specific subnegotiation entered after selection | ✅ | `src/server/connection.rs:132-162` (userpass) — no subnegotiation for no-auth, as specified |

Notes:

- The server accepts exactly one method per configuration (`no-auth` **or** `userpass`, never both —
  `select_method` `src/protocol/handshake.rs:44-55`). §3 says the server "selects from one of the
  methods given"; declining everything but the configured method is permitted.
- A greeting with a wrong version byte closes the connection without a reply
  (`src/server/connection.rs:116-117`); the RFC defines no reply for this case.

## RFC 1929 — Username/Password subnegotiation

| Requirement | Verdict | Evidence |
| --- | --- | --- |
| Request `VER ULEN UNAME PLEN PASSWD`, VER = X'01' (§2) | ✅ | `parse_userpass` `src/protocol/handshake.rs:64-83` rejects `VER != 0x01` |
| Response `VER STATUS`, X'00' = success (§2) | ✅ | `userpass_reply` `src/protocol/handshake.rs:87-89`: `[0x01, 0x00]` / `[0x01, 0x01]` |
| On failure status, server **MUST** close the connection (§2) | ✅ | `src/server/connection.rs:159-161`: after sending the failure reply, `!ok` returns `None`, closing the stream. A complete-but-malformed auth message also gets a failure reply before close (`src/server/connection.rs:146-149`; regression test `tests/reproductions.rs:520`) |
| ULEN/PLEN fields are 1 to 255 (§2) | ⚠️ | Parser accepts zero-length UNAME/PASSWD (`src/protocol/handshake.rs:69-75`). Lenient-accept only; such credentials still fail verification unless configured. Harmless. |
| UNAME/PASSWD octet content | ⚠️ | §2 defines no encoding ("username as known to the source operating system"); the parser requires valid UTF-8 (`src/protocol/handshake.rs:76-81`), so a client sending non-UTF-8 credentials gets an auth-failure reply instead of a comparison. Cannot affect UTF-8 (i.e. all realistic) credentials. |

Beyond the RFC: credential verification is constant-time across all configured users
(`src/auth.rs:14-37`), addressing the timing side-channel the RFC never considers.

## RFC 1928 §4 — Requests

| Requirement | Verdict | Evidence |
| --- | --- | --- |
| Request `VER CMD RSV ATYP DST.ADDR DST.PORT`, VER = X'05' | ✅ | `parse_request` `src/protocol/request.rs:42-59` |
| CMD X'01' CONNECT | ✅ | `src/protocol/request.rs:51`; handled in `src/server/connect.rs:26-141` |
| CMD X'02' BIND | ❌ | Parsed (`src/protocol/request.rs:52`) but deliberately refused with REP X'07' *command not supported* — the reply §6 defines for exactly this — `src/server/connection.rs:80-87`. The RFC defines the command but does not state that servers MUST implement every command. |
| CMD X'03' UDP ASSOCIATE | ✅ | `src/protocol/request.rs:53`; handled in `src/server/udp.rs:37-251` |
| Unknown CMD | ✅ | REP X'07' via `BadCommand` → `CommandNotSupported` `src/server/connection.rs:177-179` |
| RSV field is X'00' (§6: "Fields marked RESERVED (RSV) must be set to X'00'") | ⚠️ | The server ignores the RSV byte rather than enforcing it (`src/protocol/request.rs:56`). The MUST binds the sender; lenient acceptance is a deliberate robustness choice. |

## RFC 1928 §5 — Addressing

| Requirement | Verdict | Evidence |
| --- | --- | --- |
| ATYP X'01': 4-octet IPv4 | ✅ | `src/protocol/address.rs:36-44` |
| ATYP X'03': length-prefixed FQDN, no terminating NUL | ✅ | `src/protocol/address.rs:54-66`; max-length (255) round-trip tested `src/protocol/address.rs:143-153` |
| ATYP X'04': 16-octet IPv6 | ✅ | `src/protocol/address.rs:45-53` |
| Unknown ATYP | ✅ | `BadAtyp` → REP X'08' *address type not supported* `src/server/connection.rs:181-183` |
| Domain octet content | ⚠️ | §5 only says "fully-qualified domain name"; the decoder additionally requires UTF-8 (`src/protocol/address.rs:61-63`), mapping violations to REP X'01'. DNS hostnames are ASCII, so no real-world impact. |

## RFC 1928 §6 — Replies

| Requirement | Verdict | Evidence |
| --- | --- | --- |
| Reply `VER REP RSV ATYP BND.ADDR BND.PORT`, VER = X'05', RSV = X'00' | ✅ | `encode_reply` `src/protocol/reply.rs:12-17` |
| All REP codes X'00'–X'08' representable | ✅ | `Socks5Error::reply_code` `src/error.rs` maps X'01'–X'08' exactly per §6; success is `REP_SUCCEEDED` `src/protocol/reply.rs:6` |
| CONNECT success: BND.ADDR/BND.PORT = server's outbound socket address | ✅ | `upstream.local_addr()` used as BND `src/server/connect.rs:94-100` |
| Reply-code semantics on failure paths | ✅/⚠️ | Resolve failure → X'04' host unreachable (`src/server/connect.rs:41-52`); egress-blocked → X'02' connection not allowed by ruleset (`src/server/connect.rs:57-65`); dial errors mapped via `io::ErrorKind` (`src/error.rs`, `from_io`). ⚠️ One interpretive stretch: a *dial timeout* replies X'06' "TTL expired" (`src/server/connect.rs:80-89`) — §6 defines X'06' without elaboration and implementations vary (X'01'/X'04' are also seen); harmless either way. |
| **Reply Processing:** on failure the server MUST terminate the TCP connection ≤ 10 s after sending the reply | ✅ | Every failure path returns immediately after `write_all`, dropping the stream: `src/server/connection.rs:215-226`, `src/server/connect.rs:41-90`, `src/server/udp.rs:56-64` |
| BIND two-reply sequence | ➖ | BIND not implemented (see §4) |
| UDP ASSOCIATE reply: BND.ADDR/BND.PORT where the client MUST send datagrams | ✅ | Relay socket bound on the control connection's local IP (client-reachable by construction) `src/server/udp.rs:45-53`; advertised BND is that socket's real port + either the bound IP or the configured `[udp].advertise` IP for NAT/Docker `src/server/udp.rs:70-79`. An unspecified advertise IP (`0.0.0.0`/`::`) is never advertised `src/server/udp.rs:306-313` |
| UDP association terminates when the control TCP connection terminates | ✅ | Control-stream EOF/error breaks the relay loop `src/server/udp.rs:228-235` |
| UDP ASSOCIATE request DST.ADDR/DST.PORT (server MAY use to limit access; all-zeros when client doesn't know) | ✅ | The MAY is exercised differently: the request's DST is ignored, and access is limited by locking the client's actual UDP source on first datagram, pre-filtered by control-connection IP `src/server/udp.rs:91-145`. Stricter than using the (frequently zeroed) request hint. |

## RFC 1928 §7 — Procedure for UDP-based clients

| Requirement | Verdict | Evidence |
| --- | --- | --- |
| UDP request header `RSV FRAG ATYP DST.ADDR DST.PORT DATA` | ✅ | `decap` `src/protocol/udp.rs:32-46` |
| RSV = X'0000' | ⚠️ | Ignored rather than enforced (`src/protocol/udp.rs:36`) — same lenient-acceptance stance as the TCP request RSV. |
| Relay/drop decisions are silent (no notification to the client) | ✅ | All drop paths are bare `continue` `src/server/udp.rs:146-177` |
| Reply datagrams MUST be re-encapsulated with the UDP request header | ✅ | `encap` with the target's address `src/server/udp.rs:207-219` |
| Server MUST drop datagrams from any source IP other than the recorded client | ✅ | First-contact filter by control-connection IP, then exact `ip:port` lock; unknown sources are dropped `src/server/udp.rs:133-140`, `221-223` |
| Fragmentation optional; if unsupported, MUST drop FRAG != X'00' | ✅ | Fragmentation unimplemented; `FRAG != 0` dropped `src/server/udp.rs:152-155` |
| Reassembly queue/timer (only if fragmentation implemented) | ➖ | Fragmentation not implemented, so not required |
| Client-side API buffer-space reporting | ➖ | Requirement on the client programming interface, not the server |

Note (stricter than spec, deliberate): inbound *reply* datagrams are forwarded only when their
source exactly matches a previously contacted target (`known_targets`,
`src/server/udp.rs:196-207` + `207-219`) — port-restricted-cone behavior with an LRU cap
(`udp_max_targets`), plus an optional per-association rate cap
(`src/server/udp.rs:110-167`). The RFC does not require forwarding from arbitrary sources, so
this anti-injection hardening is compliant.

## Deviations & gaps, prioritized

**Deliberate scope decisions (recommend: document, won't fix)**

1. **GSSAPI not implemented** — the only true MUST violation (RFC 1928 §3). Shared by nearly every
   deployed SOCKS5 server/client; implementing it would pull in a Kerberos stack for a method with
   almost no client demand. Recommend stating the deviation in README/docs explicitly.
2. **BIND not implemented** — refused with the spec's own "command not supported" reply
   (`src/server/connection.rs:80-87`). Legitimate; already noted as intentional in code comments.
3. **UDP fragmentation not implemented** — explicitly optional (§7); the required drop behavior is
   in place.

**Lenient parsing (recommend: leave as-is)**

4. RSV bytes (TCP request, UDP header) accepted with any value; the MUSTs bind the sender.
5. RFC 1929 zero-length UNAME/PASSWD accepted by the parser (spec says 1–255); cannot authenticate
   unless such a user is configured.

**Interpretive choices (recommend: leave as-is, optionally revisit)**

6. Dial timeout → REP X'06' (TTL expired) rather than X'04'/X'01' — spec is silent, clients treat
   all non-zero REP as failure.
7. UTF-8 required for domains and credentials where the RFC specifies raw octets — no effect on
   real-world traffic.

**No action required beyond the above.** All server-side MUSTs of RFC 1928 §§4–7 and RFC 1929 §2
are met, several with hardening the RFCs don't ask for (single handshake deadline
`src/server/connection.rs:46-58`, egress SSRF filter `src/server/connect.rs:54-65`, constant-time
auth `src/auth.rs`, UDP source locking and rate/target caps `src/server/udp.rs`).

## Sources

- RFC 1928, "SOCKS Protocol Version 5", March 1996 — https://www.rfc-editor.org/rfc/rfc1928
- RFC 1929, "Username/Password Authentication for SOCKS V5", March 1996 — https://www.rfc-editor.org/rfc/rfc1929
- Repository sources at commit `beeb60c` (paths cited inline)
