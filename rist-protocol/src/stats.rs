use std::time::Duration;

/// RIST connection statistics.
#[derive(Debug, Clone, Default)]
pub struct RistStats {
    /// Total RTP packets sent.
    pub packets_sent: u64,
    /// Total RTP packets received.
    pub packets_received: u64,
    /// Total bytes sent (RTP payload only).
    pub bytes_sent: u64,
    /// Total bytes received (RTP payload only).
    pub bytes_received: u64,
    /// Total packets retransmitted by sender.
    pub packets_retransmitted: u64,
    /// Total NACK requests sent by receiver.
    pub nacks_sent: u64,
    /// Total NACK requests received by sender.
    pub nacks_received: u64,
    /// Total packets lost (not recovered).
    pub packets_lost: u64,
    /// Total packets recovered via retransmission.
    pub packets_recovered: u64,
    /// Current smoothed RTT estimate.
    pub rtt: Option<Duration>,
    /// Current packet loss fraction (0.0-1.0) over last reporting interval.
    pub loss_fraction: f64,
    /// Interarrival jitter (RFC 3550).
    pub jitter: f64,
    /// Number of bonding path switches (if bonding is active).
    pub bonding_switches: u64,
}
