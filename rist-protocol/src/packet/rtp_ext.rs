//! RTP header extensions (RFC 8285) for RIST features.
//!
//! Used by Main Profile for null packet deletion (extended sequence numbers).
//! Stubbed for Phase 1 — types only.

/// RTP header extension for RIST extended sequence numbers.
/// Used when null packet deletion is enabled (TR-06-2 Section 8.3).
///
/// The 16-bit RTP sequence number is extended to 32 bits by carrying the
/// upper 16 bits in a one-byte header extension (RFC 8285).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtendedSequenceNumber {
    /// Upper 16 bits of the 32-bit extended sequence number.
    pub seq_hi: u16,
}

impl ExtendedSequenceNumber {
    /// Combine with the standard 16-bit RTP sequence number to form
    /// a full 32-bit extended sequence number.
    pub fn full_seq(&self, rtp_seq: u16) -> u32 {
        ((self.seq_hi as u32) << 16) | (rtp_seq as u32)
    }
}
