use std::time::Duration;

/// RIST profile level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RistProfile {
    /// TR-06-1: Basic interoperability and packet loss recovery.
    Simple,
    /// TR-06-2: Adds tunneling, encryption, null packet deletion.
    Main,
}

/// Configuration for a RIST sender or receiver.
#[derive(Debug, Clone)]
pub struct RistConfig {
    /// RIST profile to use.
    pub profile: RistProfile,

    /// Receiver buffer size. Determines how long the receiver waits before
    /// delivering packets, allowing time for retransmissions.
    pub buffer_size: Duration,

    /// Maximum number of NACK retransmission attempts per lost packet.
    pub max_nack_retries: u32,

    /// RTCP compound packet emission interval.
    /// TR-06-1 requires ≤100ms.
    pub rtcp_interval: Duration,

    /// Maximum RTCP bandwidth as a fraction of media bandwidth.
    /// TR-06-1 specifies 5% (0.05).
    pub rtcp_bandwidth_fraction: f64,

    /// CNAME for SDES packets. If None, uses a generated value.
    pub cname: Option<String>,

    /// Retransmit buffer capacity (number of packets kept for retransmission).
    pub retransmit_buffer_capacity: usize,

    /// Enable RTT echo request/response (optional per TR-06-1:2020 Section 5.2.6).
    pub rtt_echo_enabled: bool,

    /// Enable null packet deletion (Main Profile, Section 8).
    pub null_packet_deletion: bool,
}

impl Default for RistConfig {
    fn default() -> Self {
        Self {
            profile: RistProfile::Simple,
            buffer_size: Duration::from_millis(1000),
            max_nack_retries: 10,
            rtcp_interval: Duration::from_millis(100),
            rtcp_bandwidth_fraction: 0.05,
            cname: None,
            retransmit_buffer_capacity: 2048,
            rtt_echo_enabled: true,
            null_packet_deletion: false,
        }
    }
}
