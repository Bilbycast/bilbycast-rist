# Integration Guide

How to integrate bilbycast-rist into your application.

## Adding the dependency

In your `Cargo.toml`:

```toml
[dependencies]
rist-transport = { path = "../bilbycast-rist/rist-transport" }
# or, if using only protocol types (no networking):
rist-protocol = { path = "../bilbycast-rist/rist-protocol" }
```

## Sender

A RIST sender wraps application data (typically MPEG-TS) in RTP packets and sends them to a remote receiver, handling retransmission requests automatically.

```rust
use rist_transport::{RistSocket, RistSocketConfig};
use bytes::Bytes;
use std::net::SocketAddr;

async fn run_sender() -> anyhow::Result<()> {
    let config = RistSocketConfig {
        local_addr: "0.0.0.0:6010".parse()?,  // must be even port
        retransmit_buffer_capacity: 4096,       // packets kept for NACK recovery
        rtt_echo_enabled: true,                 // measure RTT for optimal NACK timing
        ..Default::default()
    };

    let remote: SocketAddr = "10.0.0.2:6000".parse()?;
    let sender = RistSocket::sender(config, remote).await?;

    // Send MPEG-TS payloads (typically 1316 bytes = 7 x 188-byte TS packets)
    loop {
        let ts_data: Bytes = receive_from_encoder().await;
        sender.send(ts_data).await?;
    }
}
```

### Sender behaviour

- RTP packets are sent to the remote's even port (e.g., 6000)
- RTCP (SR + SDES) is sent to the remote's odd port (e.g., 6001) every 100 ms
- Incoming NACKs trigger retransmission from the ring buffer
- RTT echo requests from the receiver are answered automatically
- The internal channel has capacity 256; if the application sends faster than the network can drain, `send()` will await

## Receiver

A RIST receiver listens for RTP data, detects packet loss via sequence gaps, and requests retransmission via NACK.

```rust
use rist_transport::{RistSocket, RistSocketConfig};

async fn run_receiver() -> anyhow::Result<()> {
    let config = RistSocketConfig {
        local_addr: "0.0.0.0:6000".parse()?,  // must be even port
        buffer_size: std::time::Duration::from_millis(1000), // 1s recovery window
        max_nack_retries: 10,
        ..Default::default()
    };

    let mut receiver = RistSocket::receiver(config).await?;

    // Receive payloads (RTP header stripped, just the TS data)
    while let Some(payload) = receiver.recv().await {
        // payload is Bytes, typically 1316 bytes of MPEG-TS
        forward_to_decoder(&payload).await;
    }
    Ok(())
}
```

### Receiver behaviour

- Binds to even port P (RTP) and P+1 (RTCP)
- Learns the sender's address from the first received RTP packet
- Detects gaps in the sequence number stream
- Sends NACKs after a 50 ms delay (or RTT/2 if RTT is known)
- Retries up to `max_nack_retries` times per lost packet
- RTCP (RR + SDES + NACKs) emitted every 100 ms
- The internal delivery channel has capacity 1024; slow consumers cause packet drops (logged as warnings)

## Shutdown

```rust
// Graceful shutdown -- cancels all internal tasks
sender.close();
// or
receiver.close();  // also available via drop
```

## Configuration reference

```rust
pub struct RistSocketConfig {
    /// Local address to bind (RTP port, must be even).
    pub local_addr: SocketAddr,

    /// Remote address (for sender: receiver's RTP port).
    pub remote_addr: Option<SocketAddr>,

    /// Receiver buffer size (how long to wait for retransmissions).
    /// Higher values tolerate more loss but add latency.
    /// Typical: 100-2000 ms for broadcast, 50-200 ms for low-latency.
    pub buffer_size: Duration,

    /// Maximum NACK retransmission attempts per lost packet.
    /// After this many attempts, the packet is considered permanently lost.
    pub max_nack_retries: u32,

    /// RTCP compound packet emission interval.
    /// TR-06-1 requires <= 100 ms. Lower values improve loss recovery
    /// speed but increase control overhead.
    pub rtcp_interval: Duration,

    /// CNAME for SDES packets.
    /// If None, auto-generated from the local socket address.
    pub cname: Option<String>,

    /// Sender retransmit buffer capacity (number of packets).
    /// Must cover: max_rtt * packet_rate. For 500 ms RTT at 5 Mbps
    /// (approx 500 pps): 500 * 0.5 = 250 packets minimum.
    /// Default 2048 covers ~4 seconds at 5 Mbps.
    pub retransmit_buffer_capacity: usize,

    /// Enable RTT echo request/response (optional per TR-06-1).
    /// Improves NACK timing when RTT varies. Disable for minimal overhead.
    pub rtt_echo_enabled: bool,
}
```

## Using the protocol crate directly

If you need to parse/serialise RIST packets without the transport layer (e.g., for a custom networking stack), use `rist-protocol` directly:

```rust
use rist_protocol::packet::rtp::{RtpHeader, RtpPacket};
use rist_protocol::packet::rtcp::{RtcpCompound, RtcpPacket};
use rist_protocol::packet::rtcp_sr::SenderReport;
use rist_protocol::protocol::nack_tracker::{NackScheduler, RetransmitBuffer};
use rist_protocol::protocol::rtt::RttEstimator;

// Parse an incoming RTP packet
let (header, header_size) = RtpHeader::parse(&udp_data)?;
let payload = &udp_data[header_size..];

// Parse a compound RTCP packet
let compound = RtcpCompound::parse(&rtcp_data)?;
for pkt in &compound.packets {
    match pkt {
        RtcpPacket::SenderReport(sr) => { /* process SR */ }
        RtcpPacket::Nack(nack) => { /* retransmit requested packets */ }
        _ => {}
    }
}

// Build a compound RTCP packet
let compound = RtcpCompound {
    packets: vec![
        RtcpPacket::SenderReport(sr),
        RtcpPacket::Sdes(sdes),
    ],
};
let wire_bytes = compound.serialize();
```

## Sizing the retransmit buffer

The retransmit buffer must hold enough packets to cover the maximum expected round-trip time:

```
capacity >= max_rtt_seconds * packets_per_second
```

| Bitrate | Packet rate (1316 B) | RTT 100 ms | RTT 500 ms | RTT 1000 ms |
|---------|---------------------|------------|------------|-------------|
| 5 Mbps  | ~475 pps            | 48         | 238        | 475         |
| 20 Mbps | ~1900 pps           | 190        | 950        | 1900        |
| 50 Mbps | ~4750 pps           | 475        | 2375       | 4750        |
| 100 Mbps| ~9500 pps           | 950        | 4750       | 9500        |

The default 2048 covers up to ~50 Mbps at 400 ms RTT. For higher bitrates or longer RTT, increase `retransmit_buffer_capacity`.

## Logging

bilbycast-rist uses the `log` crate. Configure with any compatible logger:

```rust
env_logger::init(); // RUST_LOG=info,rist_transport=debug
```

Key log targets:
- `rist_transport::sender` -- sender task lifecycle, RTCP emission
- `rist_transport::receiver` -- receiver task lifecycle, sender detection
- `rist_transport::channel` -- socket binding, buffer sizes

## Thread safety

`RistSocket` is `Send` but not `Sync`. The `send()` method takes `&self` (uses internal `mpsc::Sender` which is `Send + Sync`). The `recv()` method takes `&mut self`. For multi-threaded access to the receiver, wrap in a `Mutex` or use a dedicated task.
