//! TS null packet deletion and restoration (TR-06-2:2024 Section 8).
//!
//! Stubbed for Phase 2 — types only.
//!
//! When enabled, the sender strips MPEG-TS null packets (PID 0x1FFF) from
//! the stream to save bandwidth. The receiver restores them using the
//! extended sequence number carried in an RTP header extension (RFC 8285).

/// TS null packet PID.
pub const TS_NULL_PID: u16 = 0x1FFF;

/// Sender-side null packet deletion state.
/// Tracks extended sequence numbers and strips null packets.
#[derive(Debug)]
pub struct NullDeleteSender {
    /// 32-bit extended sequence counter (includes deleted packets).
    pub extended_seq: u32,
}

/// Receiver-side null packet restoration state.
/// Inserts null packets at positions indicated by sequence gaps.
#[derive(Debug)]
pub struct NullDeleteReceiver {
    /// Last extended sequence number received.
    pub last_extended_seq: u32,
}
