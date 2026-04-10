//! NACK-based retransmission tracking.
//!
//! Sender side: maintains a ring buffer of recently sent packets for retransmission.
//! Receiver side: detects gaps in sequence numbers and schedules NACKs.
//!
//! Both use O(1) flat ring buffers indexed by `seq % capacity` — no heap
//! allocation on the hot path.

use std::time::{Duration, Instant};

use bytes::Bytes;

// ---------------------------------------------------------------------------
// Sender-side retransmit buffer
// ---------------------------------------------------------------------------

/// Slot in the retransmit ring buffer.
struct RetransmitSlot {
    /// Sequence number that occupies this slot (used to detect stale entries).
    seq: u16,
    /// The full RTP packet (header + payload) stored as a reference-counted buffer.
    data: Bytes,
}

/// Sender-side retransmit buffer — O(1) insert and lookup.
///
/// Fixed-size flat array indexed by `seq % capacity`. Each slot stores the
/// packet data and the sequence number that wrote it. Lookup verifies the
/// sequence number matches to detect evicted entries.
pub struct RetransmitBuffer {
    slots: Vec<Option<RetransmitSlot>>,
    capacity: usize,
}

impl RetransmitBuffer {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.next_power_of_two();
        let mut slots = Vec::with_capacity(capacity);
        slots.resize_with(capacity, || None);
        Self { slots, capacity }
    }

    /// Store a packet for potential retransmission. O(1).
    #[inline]
    pub fn insert(&mut self, seq: u16, data: Bytes) {
        let idx = seq as usize & (self.capacity - 1);
        self.slots[idx] = Some(RetransmitSlot { seq, data });
    }

    /// Look up a packet by sequence number. O(1).
    /// Returns `None` if the slot has been overwritten by a newer packet.
    #[inline]
    pub fn get(&self, seq: u16) -> Option<&Bytes> {
        let idx = seq as usize & (self.capacity - 1);
        match &self.slots[idx] {
            Some(slot) if slot.seq == seq => Some(&slot.data),
            _ => None,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

impl std::fmt::Debug for RetransmitBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RetransmitBuffer")
            .field("capacity", &self.capacity)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Receiver-side NACK scheduler
// ---------------------------------------------------------------------------

/// State of a pending NACK for a single lost sequence number.
#[derive(Clone, Copy)]
struct NackSlot {
    /// The sequence number this slot tracks. Used to detect stale entries.
    seq: u16,
    /// Whether this slot is active (has a pending NACK).
    active: bool,
    /// When the next NACK should be sent.
    next_nack_at: Instant,
    /// Number of NACKs sent for this entry.
    nack_count: u32,
}

/// Receiver-side NACK scheduler — O(1) gap detection and NACK tracking.
///
/// Uses a flat ring buffer indexed by `seq % capacity`. Gap detection and
/// recovery marking are both O(1) per packet.
///
/// `get_pending_nacks()` scans the active window, which is bounded by the
/// number of outstanding gaps (typically small, < 100 even under heavy loss).
pub struct NackScheduler {
    slots: Vec<NackSlot>,
    capacity: usize,
    /// Maximum number of NACK retries per lost packet.
    max_retries: u32,
    /// Base delay before first NACK (overridden by RTT if available).
    base_delay: Duration,
    /// Expected next sequence number.
    expected_seq: Option<u16>,
    /// Tracks the range of active slots for efficient scanning.
    /// Oldest unrecovered gap sequence number.
    scan_lo: u16,
    /// Number of active (pending) gaps.
    active_count: u32,
}

impl NackScheduler {
    pub fn new(max_retries: u32, base_delay: Duration) -> Self {
        let capacity = 4096usize; // covers ~0.6s at 7000 pps
        let default_slot = NackSlot {
            seq: 0,
            active: false,
            next_nack_at: Instant::now(),
            nack_count: 0,
        };
        Self {
            slots: vec![default_slot; capacity],
            capacity,
            max_retries,
            base_delay,
            expected_seq: None,
            scan_lo: 0,
            active_count: 0,
        }
    }

    /// Process a received RTP packet. Returns the list of newly detected lost
    /// sequence numbers (gaps between expected and received).
    pub fn on_packet_received(&mut self, seq: u16, now: Instant) -> Vec<u16> {
        let mut new_gaps = Vec::new();

        match self.expected_seq {
            None => {
                self.expected_seq = Some(seq.wrapping_add(1));
                self.scan_lo = seq;
            }
            Some(expected) => {
                let diff = seq.wrapping_sub(expected) as i16;
                if diff > 0 {
                    // Gap detected: missing expected through seq-1
                    let gap_size = (diff as u16).min(1000); // cap to prevent flooding
                    for i in 0..gap_size {
                        let missing = expected.wrapping_add(i);
                        let idx = missing as usize & (self.capacity - 1);
                        let slot = &mut self.slots[idx];
                        // Only create if this slot isn't already tracking this seq
                        if !slot.active || slot.seq != missing {
                            *slot = NackSlot {
                                seq: missing,
                                active: true,
                                next_nack_at: now + self.base_delay,
                                nack_count: 0,
                            };
                            self.active_count += 1;
                            new_gaps.push(missing);
                        }
                    }
                    self.expected_seq = Some(seq.wrapping_add(1));
                } else if diff == 0 {
                    self.expected_seq = Some(seq.wrapping_add(1));
                } else {
                    // Out-of-order or retransmitted — deactivate if pending
                    self.deactivate(seq);
                }
            }
        }

        new_gaps
    }

    /// Mark a sequence number as recovered. O(1).
    #[inline]
    pub fn on_packet_recovered(&mut self, seq: u16) {
        self.deactivate(seq);
    }

    #[inline]
    fn deactivate(&mut self, seq: u16) {
        let idx = seq as usize & (self.capacity - 1);
        let slot = &mut self.slots[idx];
        if slot.active && slot.seq == seq {
            slot.active = false;
            self.active_count = self.active_count.saturating_sub(1);
        }
    }

    /// Get the list of sequence numbers that need NACKing now.
    /// Scans only active slots. Returns sorted sequence numbers.
    pub fn get_pending_nacks(&mut self, now: Instant, rtt: Option<Duration>) -> Vec<u16> {
        if self.active_count == 0 {
            return Vec::new();
        }

        let retry_delay = rtt.unwrap_or(self.base_delay);
        let mut to_nack = Vec::with_capacity(self.active_count as usize);

        // Scan the full ring — active_count bounds how many we'll find
        for i in 0..self.capacity {
            let slot = &mut self.slots[i];
            if !slot.active {
                continue;
            }
            if now >= slot.next_nack_at {
                if slot.nack_count >= self.max_retries {
                    slot.active = false;
                    self.active_count = self.active_count.saturating_sub(1);
                } else {
                    to_nack.push(slot.seq);
                    slot.nack_count += 1;
                    slot.next_nack_at = now + retry_delay;
                }
            }
        }

        to_nack.sort_unstable();
        to_nack
    }

    /// Number of pending (unrecovered) gaps.
    pub fn pending_count(&self) -> usize {
        self.active_count as usize
    }

    pub fn update_base_delay(&mut self, delay: Duration) {
        self.base_delay = delay;
    }
}

impl std::fmt::Debug for NackScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NackScheduler")
            .field("capacity", &self.capacity)
            .field("active_count", &self.active_count)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retransmit_buffer_o1() {
        let mut buf = RetransmitBuffer::new(8); // rounds up to 8
        assert_eq!(buf.capacity(), 8);

        for i in 0..8u16 {
            buf.insert(i, Bytes::from(vec![i as u8]));
        }
        assert_eq!(buf.get(0).unwrap().as_ref(), &[0]);
        assert_eq!(buf.get(7).unwrap().as_ref(), &[7]);

        // Overwrite slot 0 (seq 8 maps to same index)
        buf.insert(8, Bytes::from(vec![8]));
        assert!(buf.get(0).is_none()); // evicted
        assert_eq!(buf.get(8).unwrap().as_ref(), &[8]);
    }

    #[test]
    fn test_retransmit_buffer_stale() {
        let mut buf = RetransmitBuffer::new(4);
        buf.insert(0, Bytes::from_static(b"a"));
        buf.insert(4, Bytes::from_static(b"b")); // overwrites slot 0
        assert!(buf.get(0).is_none());
        assert_eq!(buf.get(4).unwrap().as_ref(), b"b");
    }

    #[test]
    fn test_nack_scheduler_gap_detection() {
        let mut sched = NackScheduler::new(10, Duration::from_millis(50));
        let now = Instant::now();

        let gaps = sched.on_packet_received(0, now);
        assert!(gaps.is_empty());

        let gaps = sched.on_packet_received(3, now);
        assert_eq!(gaps, vec![1, 2]);
        assert_eq!(sched.pending_count(), 2);
    }

    #[test]
    fn test_nack_scheduler_recovery() {
        let mut sched = NackScheduler::new(10, Duration::from_millis(50));
        let now = Instant::now();

        sched.on_packet_received(0, now);
        sched.on_packet_received(3, now);

        sched.on_packet_recovered(1);
        assert_eq!(sched.pending_count(), 1);
    }

    #[test]
    fn test_nack_scheduler_timing() {
        let mut sched = NackScheduler::new(2, Duration::from_millis(50));
        let now = Instant::now();

        sched.on_packet_received(0, now);
        sched.on_packet_received(3, now);

        // Not yet time to NACK
        let nacks = sched.get_pending_nacks(now, None);
        assert!(nacks.is_empty());

        // After delay
        let later = now + Duration::from_millis(60);
        let nacks = sched.get_pending_nacks(later, None);
        assert_eq!(nacks, vec![1, 2]);

        // Second retry
        let much_later = later + Duration::from_millis(60);
        let nacks = sched.get_pending_nacks(much_later, None);
        assert_eq!(nacks, vec![1, 2]);

        // Exceeded max retries — given up
        let even_later = much_later + Duration::from_millis(60);
        let nacks = sched.get_pending_nacks(even_later, None);
        assert!(nacks.is_empty());
        assert_eq!(sched.pending_count(), 0);
    }

    #[test]
    fn test_nack_scheduler_out_of_order() {
        let mut sched = NackScheduler::new(10, Duration::from_millis(50));
        let now = Instant::now();

        sched.on_packet_received(0, now);
        sched.on_packet_received(3, now);
        sched.on_packet_received(1, now); // out-of-order
        assert_eq!(sched.pending_count(), 1); // only 2 remains
    }

    #[test]
    fn test_retransmit_buffer_wraparound() {
        let mut buf = RetransmitBuffer::new(2048);
        // Insert around the u16 wraparound
        for seq in 65530..=65535u16 {
            buf.insert(seq, Bytes::from(vec![seq as u8]));
        }
        for seq in 0..5u16 {
            buf.insert(seq, Bytes::from(vec![seq as u8]));
        }
        // All should be retrievable
        assert!(buf.get(65535).is_some());
        assert!(buf.get(0).is_some());
        assert!(buf.get(4).is_some());
    }
}
