//! Simple RIST sender example.
//!
//! Sends a test pattern to a RIST receiver.
//! Usage: cargo run --example sender -- <receiver_addr:port>

use bytes::Bytes;
use std::net::SocketAddr;
use rist_transport::{RistSocket, RistSocketConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let remote: SocketAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:5000".to_string())
        .parse()?;

    let config = RistSocketConfig {
        local_addr: "0.0.0.0:6000".parse()?,
        ..Default::default()
    };

    let socket = RistSocket::sender(config, remote).await?;
    println!("RIST sender started, sending to {remote}");

    // Send test TS-like packets
    let mut counter = 0u32;
    loop {
        let mut payload = vec![0x47u8; 1316]; // 7 TS packets
        payload[1] = (counter >> 8) as u8;
        payload[2] = counter as u8;
        socket.send(Bytes::from(payload)).await?;
        counter += 1;
        tokio::time::sleep(std::time::Duration::from_micros(1428)).await; // ~5 Mbps
    }
}
