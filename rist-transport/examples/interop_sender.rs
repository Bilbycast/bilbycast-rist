//! RIST interop test: sender
//!
//! Reads MPEG-TS from a UDP input and sends it via RIST to a receiver.
//! Designed to test against librist's `ristreceiver`.
//!
//! Test chain:
//!   ffmpeg → UDP → interop_sender → RIST → ristreceiver → UDP → analysis
//!
//! Usage:
//!   # Terminal 1: RIST receiver (librist)
//!   ristreceiver -i "rist://0.0.0.0:6000?buffer=1000" -o "udp://127.0.0.1:7000" -p 0 -v 6
//!
//!   # Terminal 2: This sender
//!   cargo run --example interop_sender -- --input udp://0.0.0.0:5000 --output 127.0.0.1:6000
//!
//!   # Terminal 3: Generate test stream
//!   ffmpeg -re -f lavfi -i testsrc=size=320x240:rate=25 -f lavfi -i sine=frequency=1000 \
//!     -c:v libx264 -preset ultrafast -b:v 1M -c:a aac -f mpegts udp://127.0.0.1:5000?pkt_size=1316

use bytes::Bytes;
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use rist_transport::{RistSocket, RistSocketConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    let udp_input: SocketAddr = args.iter()
        .position(|a| a == "--input")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.strip_prefix("udp://"))
        .unwrap_or("0.0.0.0:5000")
        .parse()?;

    let rist_output: SocketAddr = args.iter()
        .position(|a| a == "--output")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"127.0.0.1:6000".to_string())
        .parse()?;

    // Bind UDP input to receive MPEG-TS from ffmpeg
    let udp_rx = UdpSocket::bind(udp_input).await?;
    println!("Listening for MPEG-TS on UDP {udp_input}");

    // Local port for RIST sender (even number)
    let rist_local: SocketAddr = "0.0.0.0:6010".parse()?;
    let config = RistSocketConfig {
        local_addr: rist_local,
        ..Default::default()
    };

    let rist = RistSocket::sender(config, rist_output).await?;
    println!("RIST sender: {rist_local} → {rist_output}");

    let mut buf = vec![0u8; 2048];
    let mut packets = 0u64;

    loop {
        let (len, _from) = udp_rx.recv_from(&mut buf).await?;
        rist.send(Bytes::copy_from_slice(&buf[..len])).await?;
        packets += 1;
        if packets % 5000 == 0 {
            println!("Forwarded {packets} packets to RIST");
        }
    }
}
