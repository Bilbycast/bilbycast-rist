//! SMPTE 2022-7 bonding: sequence-based deduplication across multiple paths.
//!
//! When bonding is active, the same RTP packets are sent over multiple network
//! paths. The receiver uses this merger to deduplicate and fill gaps.
//!
//! Algorithm matches bilbycast-edge's HitlessMerger but adapted for 16-bit
//! RTP sequence numbers.

use bytes::Bytes;

/// Sliding window size for deduplication (in sequence numbers).
const WINDOW_SIZE: usize = 1024;
/// Number of u64 bitmask words needed for the window.
const BITMAP_WORDS: usize = WINDOW_SIZE / 64;

/// Bonding merger for SMPTE 2022-7 hitless switching.
///
/// Accepts packets from multiple paths and emits each unique packet exactly once.
/// Out-of-order packets within the window are accepted; packets outside the
/// window (too old) are dropped.
pub struct BondingMerger {
    /// Bitmap tracking which sequence numbers have been emitted.
    bitmap: [u64; BITMAP_WORDS],
    /// The highest sequence number we've emitted.
    highest_seq: Option<u16>,
    /// Number of packets emitted.
    pub packets_emitted: u64,
    /// Number of duplicate packets dropped.
    pub duplicates_dropped: u64,
    /// Number of late packets dropped (outside window).
    pub late_dropped: u64,
}

impl BondingMerger {
    pub fn new() -> Self {
        Self {
            bitmap: [0u64; BITMAP_WORDS],
            highest_seq: None,
            packets_emitted: 0,
            duplicates_dropped: 0,
            late_dropped: 0,
        }
    }

    /// Process an incoming packet from any bonded path.
    ///
    /// Returns `Some(data)` if this packet should be emitted (first time seen),
    /// or `None` if it's a duplicate or too late.
    pub fn process(&mut self, seq: u16, data: Bytes) -> Option<Bytes> {
        match self.highest_seq {
            None => {
                // First packet ever
                self.highest_seq = Some(seq);
                self.clear_bitmap();
                self.mark_seq(seq);
                self.packets_emitted += 1;
                Some(data)
            }
            Some(highest) => {
                let diff = seq.wrapping_sub(highest) as i16;

                if diff > 0 {
                    // Packet is ahead of highest — advance window
                    let advance = diff as u16;
                    self.advance_window(advance);
                    self.highest_seq = Some(seq);
                    self.mark_seq(seq);
                    self.packets_emitted += 1;
                    Some(data)
                } else if diff == 0 {
                    // Exact duplicate of highest
                    self.duplicates_dropped += 1;
                    None
                } else {
                    // Packet is behind highest — check if within window
                    let behind = (-diff) as usize;
                    if behind >= WINDOW_SIZE {
                        self.late_dropped += 1;
                        None
                    } else if self.is_seq_marked(seq) {
                        self.duplicates_dropped += 1;
                        None
                    } else {
                        // Gap fill — this packet was lost on one path but arrived on another
                        self.mark_seq(seq);
                        self.packets_emitted += 1;
                        Some(data)
                    }
                }
            }
        }
    }

    /// Mark a sequence number as seen in the bitmap.
    fn mark_seq(&mut self, seq: u16) {
        let idx = (seq as usize) % WINDOW_SIZE;
        let word = idx / 64;
        let bit = idx % 64;
        self.bitmap[word] |= 1u64 << bit;
    }

    /// Check if a sequence number has been seen.
    fn is_seq_marked(&self, seq: u16) -> bool {
        let idx = (seq as usize) % WINDOW_SIZE;
        let word = idx / 64;
        let bit = idx % 64;
        self.bitmap[word] & (1u64 << bit) != 0
    }

    /// Advance the window by clearing bits for newly entered positions.
    fn advance_window(&mut self, advance: u16) {
        if advance as usize >= WINDOW_SIZE {
            // Complete window reset
            self.clear_bitmap();
        } else {
            // Clear bits that are being recycled
            let highest = self.highest_seq.unwrap_or(0);
            for i in 1..=advance {
                let seq = highest.wrapping_add(i);
                let idx = (seq as usize) % WINDOW_SIZE;
                let word = idx / 64;
                let bit = idx % 64;
                self.bitmap[word] &= !(1u64 << bit);
            }
        }
    }

    fn clear_bitmap(&mut self) {
        self.bitmap = [0u64; BITMAP_WORDS];
    }
}

impl Default for BondingMerger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_packet() {
        let mut merger = BondingMerger::new();
        assert!(merger.process(0, Bytes::from_static(b"a")).is_some());
        assert_eq!(merger.packets_emitted, 1);
    }

    #[test]
    fn test_duplicate_dropped() {
        let mut merger = BondingMerger::new();
        merger.process(0, Bytes::from_static(b"a"));
        assert!(merger.process(0, Bytes::from_static(b"a")).is_none());
        assert_eq!(merger.duplicates_dropped, 1);
    }

    #[test]
    fn test_in_order() {
        let mut merger = BondingMerger::new();
        for i in 0..100u16 {
            assert!(merger.process(i, Bytes::from(vec![i as u8])).is_some());
        }
        assert_eq!(merger.packets_emitted, 100);
        assert_eq!(merger.duplicates_dropped, 0);
    }

    #[test]
    fn test_bonding_dedup() {
        let mut merger = BondingMerger::new();
        // Simulate two paths sending the same packets
        for i in 0..50u16 {
            assert!(merger.process(i, Bytes::from(vec![i as u8])).is_some());
            assert!(merger.process(i, Bytes::from(vec![i as u8])).is_none());
        }
        assert_eq!(merger.packets_emitted, 50);
        assert_eq!(merger.duplicates_dropped, 50);
    }

    #[test]
    fn test_gap_fill() {
        let mut merger = BondingMerger::new();
        merger.process(0, Bytes::from_static(b"0"));
        merger.process(2, Bytes::from_static(b"2")); // skip 1
        // Now 1 arrives from the other path
        assert!(merger.process(1, Bytes::from_static(b"1")).is_some());
        assert_eq!(merger.packets_emitted, 3);
    }

    #[test]
    fn test_late_packet_dropped() {
        let mut merger = BondingMerger::new();
        merger.process(0, Bytes::from_static(b"0"));
        // Advance far ahead
        merger.process(2000, Bytes::from_static(b"2000"));
        // Original packet is too late
        assert!(merger.process(0, Bytes::from_static(b"0")).is_none());
        assert_eq!(merger.late_dropped, 1);
    }

    #[test]
    fn test_wraparound() {
        let mut merger = BondingMerger::new();
        merger.process(65534, Bytes::from_static(b"a"));
        merger.process(65535, Bytes::from_static(b"b"));
        assert!(merger.process(0, Bytes::from_static(b"c")).is_some());
        assert!(merger.process(1, Bytes::from_static(b"d")).is_some());
        assert_eq!(merger.packets_emitted, 4);
    }
}
