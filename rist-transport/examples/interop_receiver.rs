//! RIST interop test: receiver
//!
//! Receives RIST from a sender and outputs MPEG-TS to a UDP destination.
//! Designed to test against librist's `ristsender`.
//!
//! Test chain:
//!   ffmpeg → UDP → ristsender → RIST → interop_receiver → UDP → ffplay
//!
//! Usage:
//!   # Terminal 1: This receiver
//!   cargo run --example interop_receiver -- --bind 0.0.0.0:6000 --output udp://127.0.0.1:7000
//!
//!   # Terminal 2: RIST sender (librist)
//!   ristsender -i "udp://0.0.0.0:5000" -o "rist://127.0.0.1:6000?buffer=1000" -p 0 -v 6
//!
//!   # Terminal 3: Generate test stream
//!   ffmpeg -re -f lavfi -i testsrc=size=320x240:rate=25 -f lavfi -i sine=frequency=1000 \
//!     -c:v libx264 -preset ultrafast -b:v 1M -c:a aac -f mpegts udp://127.0.0.1:5000?pkt_size=1316
//!
//!   # Terminal 4: Watch the output
//!   ffplay udp://127.0.0.1:7000

use std::net::SocketAddr;
use tokio::net::UdpSocket;
use rist_transport::{RistSocket, RistSocketConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();
    let rist_bind: SocketAddr = args.iter()
        .position(|a| a == "--bind")
        .and_then(|i| args.get(i + 1))
        .unwrap_or(&"0.0.0.0:6000".to_string())
        .parse()?;

    let udp_output: SocketAddr = args.iter()
        .position(|a| a == "--output")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.strip_prefix("udp://"))
        .unwrap_or("127.0.0.1:7000")
        .parse()?;

    let config = RistSocketConfig {
        local_addr: rist_bind,
        ..Default::default()
    };

    let mut rist = RistSocket::receiver(config).await?;
    println!("RIST receiver listening on {rist_bind}");

    // UDP output socket for forwarding to ffplay
    let udp_tx = UdpSocket::bind("0.0.0.0:0").await?;
    udp_tx.connect(udp_output).await?;
    println!("Output to UDP {udp_output}");

    let mut packets = 0u64;
    while let Some(data) = rist.recv().await {
        let _ = udp_tx.send(&data).await;
        packets += 1;
        if packets % 5000 == 0 {
            println!("Received {packets} packets from RIST ({} bytes last)", data.len());
        }
    }

    Ok(())
}
