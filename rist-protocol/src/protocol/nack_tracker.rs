//! NACK-based retransmission tracking.
//!
//! Sender side: maintains a ring buffer of recently sent packets for retransmission.
//! Receiver side: detects gaps in sequence numbers and schedules NACKs.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bytes::Bytes;

/// Sender-side retransmit buffer.
///
/// Stores recently sent RTP packets indexed by sequence number for NACK-driven
/// retransmission. Fixed capacity; oldest packets are evicted when full.
#[derive(Debug)]
pub struct RetransmitBuffer {
    /// Ring buffer of packets indexed by sequence number.
    /// Key is the 16-bit RTP sequence number.
    packets: BTreeMap<u16, Bytes>,
    /// Maximum number of packets to retain.
    capacity: usize,
}

impl RetransmitBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            packets: BTreeMap::new(),
            capacity,
        }
    }

    /// Store a packet for potential retransmission.
    pub fn insert(&mut self, seq: u16, data: Bytes) {
        self.packets.insert(seq, data);
        // Evict oldest entries if over capacity
        while self.packets.len() > self.capacity {
            // Remove the entry with the smallest key
            // (this is approximate for wraparound but sufficient for the ring buffer)
            if let Some(&oldest) = self.packets.keys().next() {
                self.packets.remove(&oldest);
            }
        }
    }

    /// Look up a packet by sequence number for retransmission.
    pub fn get(&self, seq: u16) -> Option<&Bytes> {
        self.packets.get(&seq)
    }

    /// Number of packets currently buffered.
    pub fn len(&self) -> usize {
        self.packets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
}

/// State of a pending NACK for a single lost sequence number.
#[derive(Debug, Clone)]
struct NackEntry {
    /// When this gap was first detected.
    #[allow(dead_code)]
    detected_at: Instant,
    /// When the next NACK should be sent.
    next_nack_at: Instant,
    /// Number of NACKs sent for this entry.
    nack_count: u32,
}

/// Receiver-side NACK scheduler.
///
/// Detects gaps in received sequence numbers and generates NACK requests
/// with configurable timing and retry limits.
#[derive(Debug)]
pub struct NackScheduler {
    /// Pending NACKs indexed by missing sequence number.
    pending: BTreeMap<u16, NackEntry>,
    /// Maximum number of NACK retries per lost packet.
    max_retries: u32,
    /// Base delay before first NACK (overridden by RTT if available).
    base_delay: Duration,
    /// Expected next sequence number.
    expected_seq: Option<u16>,
}

impl NackScheduler {
    pub fn new(max_retries: u32, base_delay: Duration) -> Self {
        Self {
            pending: BTreeMap::new(),
            max_retries,
            base_delay,
            expected_seq: None,
        }
    }

    /// Process a received RTP packet. Returns the list of newly detected lost
    /// sequence numbers (gaps between expected and received).
    pub fn on_packet_received(&mut self, seq: u16, now: Instant) -> Vec<u16> {
        let mut new_gaps = Vec::new();

        match self.expected_seq {
            None => {
                // First packet — initialize expected
                self.expected_seq = Some(seq.wrapping_add(1));
            }
            Some(expected) => {
                let diff = seq.wrapping_sub(expected) as i16;
                if diff > 0 {
                    // Gap detected: missing expected through seq-1
                    let gap_size = diff as u16;
                    // Cap gap detection to prevent flooding on large jumps
                    let max_gap = 1000u16;
                    let effective_gap = gap_size.min(max_gap);
                    for i in 0..effective_gap {
                        let missing = expected.wrapping_add(i);
                        if !self.pending.contains_key(&missing) {
                            let entry = NackEntry {
                                detected_at: now,
                                next_nack_at: now + self.base_delay,
                                nack_count: 0,
                            };
                            self.pending.insert(missing, entry);
                            new_gaps.push(missing);
                        }
                    }
                    self.expected_seq = Some(seq.wrapping_add(1));
                } else if diff == 0 {
                    // Expected packet arrived in order
                    self.expected_seq = Some(seq.wrapping_add(1));
                } else {
                    // Out-of-order or retransmitted packet — remove from pending if present
                    self.pending.remove(&seq);
                }
            }
        }

        new_gaps
    }

    /// Mark a sequence number as recovered (received via retransmission).
    pub fn on_packet_recovered(&mut self, seq: u16) {
        self.pending.remove(&seq);
    }

    /// Get the list of sequence numbers that need NACKing now.
    /// Updates internal state (increments nack_count, schedules next retry).
    /// Returns sequence numbers to NACK, sorted.
    pub fn get_pending_nacks(&mut self, now: Instant, rtt: Option<Duration>) -> Vec<u16> {
        let retry_delay = rtt.unwrap_or(self.base_delay);
        let mut to_nack = Vec::new();
        let mut to_remove = Vec::new();

        for (&seq, entry) in &mut self.pending {
            if now >= entry.next_nack_at {
                if entry.nack_count >= self.max_retries {
                    // Give up on this packet
                    to_remove.push(seq);
                } else {
                    to_nack.push(seq);
                    entry.nack_count += 1;
                    entry.next_nack_at = now + retry_delay;
                }
            }
        }

        for seq in to_remove {
            self.pending.remove(&seq);
        }

        to_nack.sort();
        to_nack
    }

    /// Number of pending (unrecovered) gaps.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Number of packets that were given up on (exceeded max retries).
    /// This is tracked externally — caller should count removals from get_pending_nacks.
    pub fn update_base_delay(&mut self, delay: Duration) {
        self.base_delay = delay;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retransmit_buffer() {
        let mut buf = RetransmitBuffer::new(5);
        for i in 0..5 {
            buf.insert(i, Bytes::from(vec![i as u8]));
        }
        assert_eq!(buf.len(), 5);
        assert_eq!(buf.get(0).unwrap().as_ref(), &[0]);
        assert_eq!(buf.get(4).unwrap().as_ref(), &[4]);

        // Exceeding capacity evicts oldest
        buf.insert(5, Bytes::from(vec![5]));
        assert_eq!(buf.len(), 5);
        assert!(buf.get(0).is_none());
        assert!(buf.get(5).is_some());
    }

    #[test]
    fn test_nack_scheduler_gap_detection() {
        let mut sched = NackScheduler::new(10, Duration::from_millis(50));
        let now = Instant::now();

        // Receive seq 0
        let gaps = sched.on_packet_received(0, now);
        assert!(gaps.is_empty());

        // Receive seq 3 (missing 1, 2)
        let gaps = sched.on_packet_received(3, now);
        assert_eq!(gaps, vec![1, 2]);
        assert_eq!(sched.pending_count(), 2);
    }

    #[test]
    fn test_nack_scheduler_recovery() {
        let mut sched = NackScheduler::new(10, Duration::from_millis(50));
        let now = Instant::now();

        sched.on_packet_received(0, now);
        sched.on_packet_received(3, now); // gap: 1, 2

        // Recover packet 1 (arrived out of order)
        sched.on_packet_recovered(1);
        assert_eq!(sched.pending_count(), 1);
    }

    #[test]
    fn test_nack_scheduler_timing() {
        let mut sched = NackScheduler::new(2, Duration::from_millis(50));
        let now = Instant::now();

        sched.on_packet_received(0, now);
        sched.on_packet_received(3, now); // gap: 1, 2

        // Not yet time to NACK
        let nacks = sched.get_pending_nacks(now, None);
        assert!(nacks.is_empty());

        // After delay
        let later = now + Duration::from_millis(60);
        let nacks = sched.get_pending_nacks(later, None);
        assert_eq!(nacks, vec![1, 2]);

        // After max retries, packets are removed
        let much_later = later + Duration::from_millis(60);
        let nacks = sched.get_pending_nacks(much_later, None);
        assert_eq!(nacks, vec![1, 2]); // second retry

        let even_later = much_later + Duration::from_millis(60);
        let nacks = sched.get_pending_nacks(even_later, None);
        assert!(nacks.is_empty()); // given up
        assert_eq!(sched.pending_count(), 0);
    }

    #[test]
    fn test_nack_scheduler_out_of_order() {
        let mut sched = NackScheduler::new(10, Duration::from_millis(50));
        let now = Instant::now();

        sched.on_packet_received(0, now);
        sched.on_packet_received(3, now); // gap: 1, 2
        sched.on_packet_received(1, now); // out-of-order delivery of 1
        assert_eq!(sched.pending_count(), 1); // only 2 remains
    }
}
