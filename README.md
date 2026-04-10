# bilbycast-rist

Pure Rust implementation of the [RIST](https://www.rist.tv/) (Reliable Internet Stream Transport) protocol for professional broadcast media transport.

Zero C/C++ dependencies. Wire-compatible with [librist](https://code.videolan.org/rist/librist) 0.2.11, FFmpeg `rist://`, and GStreamer `ristenc`/`ristdec`.

## What is RIST?

RIST is a standardised protocol for reliable media transport over lossy IP networks (internet, satellite, cellular). It wraps standard RTP/RTCP with automatic retransmission (ARQ) to recover lost packets without the latency penalty of FEC-only approaches.

Key properties:
- **NACK-based ARQ** -- receiver detects gaps and requests retransmission
- **RTT-aware timing** -- NACK scheduling adapts to measured round-trip time
- **Dual-port RTP/RTCP** -- standard even/odd port pair (RFC 3550)
- **SMPTE 2022-7 bonding** -- hitless failover across redundant network paths
- **Low, bounded latency** -- configurable receiver buffer (typically 100-2000 ms)

## Crate structure

| Crate | Description |
|-------|-------------|
| [`rist-protocol`](rist-protocol/) | Pure protocol logic: packet parsing/serialisation, RTCP state machines, NACK tracking, RTT estimation, bonding merger. No I/O dependencies. |
| [`rist-transport`](rist-transport/) | Async networking layer: tokio-based sender/receiver tasks, dual-port UDP channels, public `RistSocket` API. |

## Quick start

### As a library

```rust
use rist_transport::{RistSocket, RistSocketConfig};
use bytes::Bytes;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // --- Sender ---
    let config = RistSocketConfig {
        local_addr: "0.0.0.0:6010".parse()?,
        ..Default::default()
    };
    let sender = RistSocket::sender(config, "10.0.0.2:6000".parse()?).await?;
    sender.send(Bytes::from_static(&[0x47, 0x00, 0x00, 0x10])).await?;

    // --- Receiver ---
    let config = RistSocketConfig {
        local_addr: "0.0.0.0:6000".parse()?,
        ..Default::default()
    };
    let mut receiver = RistSocket::receiver(config).await?;
    while let Some(payload) = receiver.recv().await {
        // payload is the raw MPEG-TS data (no RTP header)
        println!("received {} bytes", payload.len());
    }
    Ok(())
}
```

### Interop examples

The `rist-transport/examples/` directory contains ready-to-use interop test programs:

```bash
# Build
cargo build --examples

# Our sender -> librist ristreceiver
ristreceiver -i "rist://@:6000?buffer=1000" -o "udp://127.0.0.1:7000" -p 0
cargo run --example interop_sender -- --input udp://0.0.0.0:5000 --output 127.0.0.1:6000
# Feed MPEG-TS into UDP port 5000

# librist ristsender -> our receiver
cargo run --example interop_receiver -- --bind 0.0.0.0:6000 --output udp://127.0.0.1:7000
ristsender -i "udp://0.0.0.0:5000" -o "rist://127.0.0.1:6000?buffer=1000" -p 0
# Feed MPEG-TS into UDP port 5000
```

## Build

```bash
cargo build            # debug
cargo build --release  # optimised
cargo test             # 51 unit tests
```

No C compiler, CMake, or system libraries required.

## Interoperability

Tested against librist 0.2.11 (Simple Profile, `-p 0`):

| Direction | Rate | Delivery |
|-----------|------|----------|
| librist ristsender -> bilbycast receiver | 5 Mbps | 100% |
| librist ristsender -> bilbycast receiver | 50 Mbps | 100% |
| bilbycast sender -> librist ristreceiver | 5 Mbps | 100% (stats-verified) |
| bilbycast sender -> librist ristreceiver | 50 Mbps | 99.996% |

## Configuration

`RistSocketConfig` controls sender and receiver behaviour:

| Parameter | Default | Description |
|-----------|---------|-------------|
| `local_addr` | `0.0.0.0:5000` | Local RTP bind address (must be even port) |
| `buffer_size` | 1000 ms | Receiver buffer for retransmission recovery |
| `max_nack_retries` | 10 | Max NACK attempts per lost packet before giving up |
| `rtcp_interval` | 100 ms | RTCP compound packet emission interval (TR-06-1 limit) |
| `retransmit_buffer_capacity` | 2048 | Sender retransmit ring buffer size (packets) |
| `rtt_echo_enabled` | true | Enable RTT measurement via RTCP APP echo |
| `cname` | auto | SDES CNAME string (auto-generated from local address) |

## Specifications

- **TR-06-1:2020** -- RIST Simple Profile (implemented)
- **TR-06-2:2024** -- RIST Main Profile (types stubbed)

## Implementation status

| Feature | Status |
|---------|--------|
| RTP parsing/serialisation (RFC 3550) | Done |
| RTCP SR/RR/SDES/NACK/APP | Done |
| RTCP compound packets | Done |
| 16-bit sequence arithmetic | Done |
| NACK-based retransmission (bitmask + range) | Done |
| RTT estimation (EWMA) | Done |
| NTP-aligned RTP timestamps | Done |
| SMPTE 2022-7 bonding | Done |
| Async sender/receiver (tokio) | Done |
| Dual-port UDP channel (SO_REUSEADDR, 2 MB buffers) | Done |
| librist 0.2.11 interop (Simple Profile) | Done |
| GRE-over-UDP tunnelling (Main Profile) | Stubbed |
| PSK encryption (AES-CTR) | Stubbed |
| DTLS 1.2 encryption | Stubbed |
| Null packet deletion | Stubbed |

## Architecture

```
Application
    |
    v
RistSocket::send(payload)
    |
    v
Sender task (tokio)
    |-- wraps in RTP (V=2, PT=33, NTP-aligned timestamps)
    |-- sends via UDP to remote RTP port
    |-- buffers in retransmit ring for NACK recovery
    |-- emits RTCP SR + SDES every 100 ms
    |-- responds to NACKs with retransmissions
    |-- responds to RTT echo requests
    v
UDP (even port P) -----> Remote receiver
RTCP (port P+1)  <-----> Remote receiver

Remote sender -----> UDP (even port P)
                     RTCP (port P+1) <----->
                         |
                         v
                    Receiver task (tokio)
                         |-- parses RTP, detects gaps
                         |-- sends NACKs for missing packets
                         |-- emits RTCP RR + SDES every 100 ms
                         |-- sends RTT echo requests
                         v
                    RistSocket::recv() -> payload
                         |
                         v
                    Application
```

## License

This project is licensed under the [Mozilla Public License 2.0](LICENSE).
