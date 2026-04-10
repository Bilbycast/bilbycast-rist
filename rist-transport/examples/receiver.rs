//! Simple RIST receiver example.
//!
//! Listens for RIST media and prints stats.
//! Usage: cargo run --example receiver -- [bind_addr:port]

use rist_transport::{RistSocket, RistSocketConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let bind: std::net::SocketAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:5000".to_string())
        .parse()?;

    let config = RistSocketConfig {
        local_addr: bind,
        ..Default::default()
    };

    let mut socket = RistSocket::receiver(config).await?;
    println!("RIST receiver listening on {bind}");

    let mut count = 0u64;
    while let Some(data) = socket.recv().await {
        count += 1;
        if count % 1000 == 0 {
            println!("Received {count} packets ({} bytes last)", data.len());
        }
    }

    Ok(())
}
