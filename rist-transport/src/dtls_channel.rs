//! DTLS-encrypted channel wrapper (TR-06-2:2024 Section 6).
//!
//! Stubbed for Phase 3.
//!
//! When implemented, this will wrap the GRE tunnel UDP socket with
//! DTLS 1.2 encryption using the required cipher suites.

/// Placeholder for DTLS channel configuration.
#[derive(Debug, Clone)]
pub struct DtlsChannelConfig {
    /// Enable DTLS encryption.
    pub enabled: bool,
}
