//! GRE tunnel state machine (TR-06-2:2024 Section 5.6).
//!
//! Stubbed for Phase 2 — types only.

use std::time::Duration;

/// GRE tunnel connection state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelState {
    /// Not connected. Client needs to send keep-alives to establish.
    Disconnected,
    /// Sending initial keep-alive burst (3-10 messages).
    Establishing,
    /// Tunnel is active. Keep-alives exchanged periodically.
    Connected,
}

/// GRE tunnel configuration.
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    /// Keep-alive interval (1-10 seconds per spec).
    pub keepalive_interval: Duration,
    /// Tunnel timeout (default 60 seconds per spec).
    pub tunnel_timeout: Duration,
    /// Number of initial keep-alives to send (3-10 per spec).
    pub initial_keepalive_count: u8,
    /// Use Reduced Overhead mode (vs Full Datagram mode).
    pub reduced_overhead: bool,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            keepalive_interval: Duration::from_secs(5),
            tunnel_timeout: Duration::from_secs(60),
            initial_keepalive_count: 5,
            reduced_overhead: true,
        }
    }
}
