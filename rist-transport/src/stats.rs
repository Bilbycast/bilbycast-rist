//! Shared connection-level counters for a `RistSocket`.
//!
//! The sender and receiver tasks update these atomics on their hot paths;
//! consumers (the edge stats API, the Prometheus exporter) snapshot them
//! via `RistConnStats::snapshot()`. Lock-free throughout.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Role of the socket these stats belong to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RistRole {
    Sender,
    Receiver,
}

/// Lock-free counters populated by the sender / receiver tasks.
///
/// Stored behind an `Arc` so the async tasks and the stats snapshotter
/// share the same counters without locking.
#[derive(Debug, Default)]
pub struct RistConnStats {
    // Sender counters
    pub packets_sent: AtomicU64,
    pub bytes_sent: AtomicU64,
    pub packets_retransmitted: AtomicU64,
    pub nacks_received: AtomicU64,

    // Receiver counters
    pub packets_received: AtomicU64,
    pub bytes_received: AtomicU64,
    pub packets_lost: AtomicU64,
    pub packets_recovered: AtomicU64,
    pub nacks_sent: AtomicU64,
    pub duplicates: AtomicU64,
    pub reorder_drops: AtomicU64,
    /// RTP data packets whose SSRC carried the RIST retransmit flag
    /// (LSB=1). This is the authoritative ARQ-recovered-via-retransmit
    /// count — `packets_recovered` also fires on pure out-of-order
    /// arrivals that fill a gap without ever being requested.
    pub retransmits_received: AtomicU64,

    /// Smoothed RTT expressed in microseconds. `0` when no RTT sample yet.
    pub rtt_us: AtomicU64,
    /// Interarrival jitter (RFC 3550 A.8) scaled to microseconds.
    pub jitter_us: AtomicU64,
}

impl RistConnStats {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Capture a consistent-ish snapshot of all counters.
    pub fn snapshot(&self) -> RistConnStatsSnapshot {
        RistConnStatsSnapshot {
            packets_sent: self.packets_sent.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            packets_retransmitted: self.packets_retransmitted.load(Ordering::Relaxed),
            nacks_received: self.nacks_received.load(Ordering::Relaxed),
            packets_received: self.packets_received.load(Ordering::Relaxed),
            bytes_received: self.bytes_received.load(Ordering::Relaxed),
            packets_lost: self.packets_lost.load(Ordering::Relaxed),
            packets_recovered: self.packets_recovered.load(Ordering::Relaxed),
            nacks_sent: self.nacks_sent.load(Ordering::Relaxed),
            duplicates: self.duplicates.load(Ordering::Relaxed),
            reorder_drops: self.reorder_drops.load(Ordering::Relaxed),
            retransmits_received: self.retransmits_received.load(Ordering::Relaxed),
            rtt_us: self.rtt_us.load(Ordering::Relaxed),
            jitter_us: self.jitter_us.load(Ordering::Relaxed),
        }
    }
}

/// Plain-data snapshot for external consumers.
#[derive(Debug, Clone, Default)]
pub struct RistConnStatsSnapshot {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub packets_retransmitted: u64,
    pub nacks_received: u64,
    pub packets_received: u64,
    pub bytes_received: u64,
    pub packets_lost: u64,
    pub packets_recovered: u64,
    pub nacks_sent: u64,
    pub duplicates: u64,
    pub reorder_drops: u64,
    pub retransmits_received: u64,
    pub rtt_us: u64,
    pub jitter_us: u64,
}

impl RistConnStatsSnapshot {
    pub fn rtt_ms(&self) -> f64 {
        self.rtt_us as f64 / 1000.0
    }
}
