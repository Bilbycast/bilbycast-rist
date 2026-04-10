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
| RTCP NACK (bitmask + range) | Done |
| RTCP RTT Echo Request/Response | Done |
| RTCP compound packets | Done |
| 16-bit sequence arithmetic | Done |
| RTCP sender/receiver state | Done |
| NACK-based retransmission | Done |
| RTT estimation (EWMA) | Done |
| SMPTE 2022-7 bonding | Done |
| Async sender/receiver tasks | Done |
| Dual-port UDP channel | Done |
| GRE-over-UDP tunneling | Stubbed |
| PSK encryption (AES-CTR) | Stubbed |
| DTLS 1.2 encryption | Stubbed |
| Null packet deletion | Stubbed |
| bilbycast-edge integration | Not started |

## Inter-Project Dependencies

```
bilbycast-edge
  └── compiles against: bilbycast-rist (path dependency, future)
```
