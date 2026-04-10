//! RIST socket configuration.

use std::net::SocketAddr;
use std::time::Duration;

/// Configuration for a RIST socket (sender or receiver).
#[derive(Debug, Clone)]
pub struct RistSocketConfig {
    /// Local address to bind (RTP port, must be even).
    pub local_addr: SocketAddr,
    /// Remote address (for sender: receiver's RTP port; for receiver: optional sender filter).
    pub remote_addr: Option<SocketAddr>,
    /// Receiver buffer size (how long to wait for retransmissions).
    pub buffer_size: Duration,
    /// Maximum NACK retransmission attempts.
    pub max_nack_retries: u32,
    /// RTCP emission interval (≤100ms per TR-06-1).
    pub rtcp_interval: Duration,
    /// CNAME for SDES packets.
    pub cname: Option<String>,
    /// Retransmit buffer capacity (sender side).
    pub retransmit_buffer_capacity: usize,
    /// Enable RTT echo request/response.
    pub rtt_echo_enabled: bool,
}

impl Default for RistSocketConfig {
    fn default() -> Self {
        Self {
            local_addr: "0.0.0.0:5000".parse().unwrap(),
            remote_addr: None,
            buffer_size: Duration::from_millis(1000),
            max_nack_retries: 10,
            rtcp_interval: Duration::from_millis(100),
            cname: None,
            retransmit_buffer_capacity: 2048,
            rtt_echo_enabled: true,
        }
    }
}
