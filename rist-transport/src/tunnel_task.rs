//! GRE tunnel transport task (TR-06-2:2024 Main Profile).
//!
//! Stubbed for Phase 2.
//!
//! When implemented, this will multiplex RTP+RTCP onto a single UDP port
//! via GRE-over-UDP encapsulation.

/// Placeholder for GRE tunnel task configuration.
#[derive(Debug, Clone)]
pub struct TunnelTaskConfig {
    /// Tunnel UDP port.
    pub tunnel_port: u16,
    /// Use Reduced Overhead mode.
    pub reduced_overhead: bool,
}
