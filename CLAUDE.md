# CLAUDE.md — bilbycast-rist

Pure Rust implementation of the RIST (Reliable Internet Stream Transport) protocol.

## What Is bilbycast-rist

A standalone Rust library implementing the VSF RIST protocol for reliable media transport over lossy networks. Zero C/C++ dependencies — pure Rust throughout.

## Specifications

- **TR-06-1:2020** — RIST Simple Profile (implemented)
- **TR-06-2:2024** — RIST Main Profile (types stubbed, implementation pending)

## Crate Structure

| Crate | Role |
|-------|------|
| **rist-protocol** | Pure protocol logic: packet parsing/serialization, RTCP state machines, NACK tracking, RTT estimation, bonding. No I/O dependencies. |
| **rist-transport** | Async networking layer: tokio-based sender/receiver tasks, UDP channel management, public `RistSocket` API. |

## Architecture

### Protocol Layer (rist-protocol)

- `packet/` — Wire format types for RTP (RFC 3550), RTCP SR/RR/SDES/NACK/APP, 16-bit sequence arithmetic
- `protocol/` — State machines: RTCP sender/receiver state, NACK-based retransmission (sender buffer + receiver gap detection), RTT estimation (EWMA), SMPTE 2022-7 bonding merger
- `gre/` — GRE-over-UDP header types (stubbed for Main Profile)
- `crypto/` — PSK (AES-CTR) and DTLS config types (stubbed for Phase 3)

### Transport Layer (rist-transport)

- `channel.rs` — Dual-port UDP binding (RTP on even port P, RTCP on P+1)
- `sender.rs` — Sender task: RTP out, NACK handling, retransmit from buffer, periodic RTCP SR+SDES
- `receiver.rs` — Receiver task: RTP in, gap detection, NACK generation, periodic RTCP RR+SDES
- `socket.rs` — Public API: `RistSocket::sender()` and `RistSocket::receiver()`

### Data Flow

```
Application → RistSocket::send(payload)
  → sender task wraps in RTP, sends via UDP, buffers for retransmit
  → periodic RTCP SR + SDES to receiver

Receiver UDP → receiver task parses RTP, detects gaps
  → sends NACKs for lost packets
  → delivers payload to application via RistSocket::recv()
  → periodic RTCP RR + SDES + NACKs to sender
```

## Build & Test

```bash
cargo build          # Build both crates
cargo test           # Run all tests (48 unit tests in rist-protocol)
cargo build --release
```

## Key Design Decisions

1. **No traits for transport abstraction** — follows bilbycast-srt pattern: concrete types + enum dispatch
2. **Protocol/transport separation** — rist-protocol has zero async/I/O deps, fully testable
3. **Lock-free data path** — sender/receiver tasks own all mutable state, communicate via channels
4. **RIST carries native RTP** — unlike SRT, packets are standard RTP, simplifying bilbycast-edge integration

## Implementation Status

| Feature | Status |
|---------|--------|
| RTP parsing/serialization | Done |
| RTCP SR/RR/SDES | Done |
| RTCP NACK — PT=205 RTPFB bitmask (FMT=1) | Send + parse |
| RTCP NACK — PT=205 RTPFB range (FMT=15) | Parse |
| RTCP NACK — PT=204 APP "RIST" range (librist default, subtype 0) | **Parse — verified against librist 0.2.11** |
| RIST retransmit flag (RTP SSRC LSB=1) | Send + parse (authoritative `retransmits_received` stat) |
| RTCP RTT Echo Request/Response (APP "RIST" subtypes 2/3) | Done |
| RTCP XR PT=77 (librist RRT extension) | Tolerated (lenient compound parse) |
| RTCP compound packets (lenient parse) | Done — unknown sub-packets preserved, don't abort compound |
| 16-bit sequence arithmetic | Done |
| RTCP sender/receiver state | Done |
| Receiver-side reorder / jitter buffer (`protocol::reorder::ReorderBuffer`) | Done |
| Fast NACK pump (10 ms tick, RTT-scaled retry delay) | Done |
| NACK-based retransmission | Done |
| RTT estimation (EWMA) | Done |
| SMPTE 2022-7 bonding | Done (protocol layer) |
| Async sender/receiver tasks | Done |
| Dual-port UDP channel | Done |
| Shared stats handle (`RistConnStats`) | Done (Arc<AtomicU64>, lock-free) |
| GRE-over-UDP tunneling (Main Profile) | Stubbed — separate sprint |
| Main Profile peer multiplexing | Stubbed — separate sprint |
| DTLS 1.2 encryption (Main Profile) | Stubbed — separate sprint |
| PSK encryption (AES-CTR, Main Profile) | Stubbed — separate sprint |
| Null-packet deletion | Stubbed |
| bilbycast-edge integration | **Done** — wire-verified with librist 0.2.11 ARQ matrix, both directions |

## Interop Status (Simple Profile)

**100 % bidirectional interoperability with librist 0.2.11 Simple Profile has been achieved** under adverse-network conditions (10 % loss / 200 ms delay / 50 ms jitter on both RTP and RTCP paths). See `testbed/RIST_ARQ_TEST.md` for the full test matrix and post-run stats cross-checks.

Remaining gaps are **Main Profile + DTLS + AES-CTR + null-packet deletion**, all tracked as dedicated follow-up sprints. The Simple Profile data-plane is stable, stats are authoritative, and the retransmit-flag accounting matches librist's expectations.

## Inter-Project Dependencies

```
bilbycast-edge
  └── compiles against: bilbycast-rist (path dependency, future)
```
