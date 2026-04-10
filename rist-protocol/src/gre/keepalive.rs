//! Keep-alive message format for GRE tunnel establishment (TR-06-2:2024 Section 5.6.3-5.6.4).
//!
//! Stubbed for Phase 2 — types only.

/// Keep-alive message payload (JSON-encoded capabilities).
#[derive(Debug, Clone)]
pub struct KeepAlivePayload {
    /// RIST protocol version string.
    pub version: String,
    /// Supported profile.
    pub profile: String,
}
