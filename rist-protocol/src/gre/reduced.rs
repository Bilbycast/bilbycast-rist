//! Reduced Overhead mode for GRE tunneling (TR-06-2:2024 Section 5.3.2).
//!
//! Stubbed for Phase 2 — types only.
//!
//! In this mode, only the UDP source and destination ports are prepended
//! to the payload (4 bytes), rather than a full IP+UDP header.

/// Reduced overhead header: just UDP port pair.
#[derive(Debug, Clone, Copy)]
pub struct ReducedUdpHeader {
    pub source_port: u16,
    pub dest_port: u16,
}
