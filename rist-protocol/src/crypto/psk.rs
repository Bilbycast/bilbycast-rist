//! Pre-Shared Key (PSK) encryption (TR-06-2:2024 Section 7).
//!
//! Stubbed for Phase 3 — types only.
//!
//! Uses AES-128 or AES-256 in CTR mode (RFC 3686).
//! Key is derived from a passphrase via EAP SRP-SHA256 (Annex D).

/// PSK encryption configuration.
#[derive(Debug, Clone)]
pub struct PskConfig {
    /// AES key length: 128 or 256 bits.
    pub key_length: PskKeyLength,
    /// Passphrase for key derivation.
    pub passphrase: String,
}

/// AES key length for PSK encryption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PskKeyLength {
    Aes128,
    Aes256,
}
