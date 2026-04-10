//! DTLS 1.2 encryption (TR-06-2:2024 Section 6).
//!
//! Stubbed for Phase 3 — types only.
//!
//! DTLS is applied to the GRE tunnel UDP port. Required cipher suites:
//! - TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256
//! - TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256

/// DTLS configuration.
#[derive(Debug, Clone)]
pub struct DtlsConfig {
    /// Authentication mode.
    pub auth_mode: DtlsAuthMode,
}

/// DTLS authentication mode.
#[derive(Debug, Clone)]
pub enum DtlsAuthMode {
    /// Certificate-based authentication.
    Certificate {
        cert_path: String,
        key_path: String,
    },
    /// Pre-shared key via TLS-SRP (Section 6.4).
    Srp {
        username: String,
        password: String,
    },
}
