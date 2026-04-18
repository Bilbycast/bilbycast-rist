//! Receiver-side reorder / jitter buffer.
//!
//! Holds incoming RTP packets for `buffer_time` so NACK-driven retransmits
//! can fill gaps before delivery. Delivers packets to the application in
//! RTP sequence order. Missing packets that don't arrive in time are
//! reported as lost so downstream stats stay accurate.
//!
//! O(1) insert and amortised O(1) drain via a flat ring buffer indexed by
//! `seq & (capacity - 1)`. 16-bit sequence arithmetic is handled using
//! wrapping semantics throughout.

use std::time::{Duration, Instant};

use bytes::Bytes;

/// Default ring-buffer size. 8192 slots at ~8000 pps = ~1 s of headroom;
/// combined with `buffer_time` gating, this comfortably covers a 1 s jitter
/// budget at typical broadcast bitrates.
const DEFAULT_CAPACITY: usize = 8192;

/// A slot in the reorder ring.
#[derive(Clone)]
struct Slot {
    /// Sequence number this slot tracks. Used to detect stale entries after
    /// wraparound (when a newer seq has claimed the same ring index).
    seq: u16,
    state: SlotState,
}

#[derive(Clone)]
enum SlotState {
    /// Never written — no packet seen with this seq yet, and no later
    /// packet has caused us to mark it as a gap.
    Empty,
    /// We noticed this sequence was missing at `first_noticed` (when a
    /// higher seq arrived). Used to time-out gaps that never get filled.
    Gap { first_noticed: Instant },
    /// Packet is available for delivery.
    Filled { data: Bytes, arrival: Instant },
}

/// Result of `insert`.
#[derive(Debug, Clone, Copy, Default)]
pub struct InsertOutcome {
    /// Seq was strictly below the current base (already delivered / timed out).
    pub stale: bool,
    /// Seq was already filled (duplicate arrival).
    pub duplicate: bool,
    /// Seq filled a slot that we had flagged as a gap — retransmit recovered it.
    pub recovered: bool,
    /// Number of sequence slots newly flagged as gaps because this packet
    /// pushed `highest_seq` forward across empty positions.
    pub new_gaps: u32,
}

/// Item produced by `drain_ready`.
pub enum DrainItem {
    /// Packet payload (header stripped).
    Delivered(Bytes),
    /// Gap timed out — no retransmit arrived within `buffer_time`.
    Lost,
}

/// Receiver-side reorder / jitter buffer.
pub struct ReorderBuffer {
    slots: Vec<Slot>,
    capacity: usize,
    mask: usize,
    buffer_time: Duration,
    /// Next sequence number scheduled for delivery.
    base_seq: Option<u16>,
    /// Highest sequence number inserted so far (via a fresh arrival, not
    /// gap marking). Used to size the "forward" gap-fill region on insert.
    highest_seq: Option<u16>,
}

impl ReorderBuffer {
    pub fn new(buffer_time: Duration) -> Self {
        Self::with_capacity(buffer_time, DEFAULT_CAPACITY)
    }

    pub fn with_capacity(buffer_time: Duration, capacity: usize) -> Self {
        let capacity = capacity.next_power_of_two().max(64);
        let empty_slot = Slot {
            seq: 0,
            state: SlotState::Empty,
        };
        Self {
            slots: vec![empty_slot; capacity],
            capacity,
            mask: capacity - 1,
            buffer_time,
            base_seq: None,
            highest_seq: None,
        }
    }

    /// Configured hold time.
    #[inline]
    pub fn buffer_time(&self) -> Duration {
        self.buffer_time
    }

    /// Number of ring slots.
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Insert an incoming packet. Returns an outcome describing whether the
    /// packet was new, duplicate, stale (before the base), or a recovery.
    pub fn insert(&mut self, seq: u16, data: Bytes, now: Instant) -> InsertOutcome {
        let mut outcome = InsertOutcome::default();

        // First packet initialises base_seq and highest_seq to the arrival seq.
        let (base, highest) = match (self.base_seq, self.highest_seq) {
            (Some(b), Some(h)) => (b, h),
            _ => {
                let idx = (seq as usize) & self.mask;
                self.slots[idx] = Slot {
                    seq,
                    state: SlotState::Filled { data, arrival: now },
                };
                self.base_seq = Some(seq);
                self.highest_seq = Some(seq);
                return outcome;
            }
        };

        // Reject packets older than `base` — they were already delivered or
        // timed out. Use signed 16-bit arithmetic so wraparound works.
        let from_base = seq.wrapping_sub(base) as i16;
        if from_base < 0 {
            outcome.stale = true;
            return outcome;
        }

        // If the packet is farther ahead than the ring can hold, drop it —
        // accepting would corrupt slots in use for lower seqs. This keeps
        // the invariant that every index in the active window maps uniquely.
        let ahead_capacity = self.capacity as u16 - 1;
        if (from_base as u16) > ahead_capacity {
            outcome.stale = true;
            return outcome;
        }

        let idx = (seq as usize) & self.mask;

        // If the slot already holds our seq, it's a duplicate (or the retransmit
        // of a filled packet); keep the first arrival.
        if self.slots[idx].seq == seq {
            match self.slots[idx].state {
                SlotState::Filled { .. } => {
                    outcome.duplicate = true;
                    return outcome;
                }
                SlotState::Gap { .. } => {
                    outcome.recovered = true;
                    self.slots[idx].state = SlotState::Filled { data, arrival: now };
                }
                SlotState::Empty => {
                    self.slots[idx].state = SlotState::Filled { data, arrival: now };
                }
            }
        } else {
            // Different seq occupies this slot — a stale entry from an older
            // cycle the drain path hasn't cleared yet. Overwrite.
            self.slots[idx] = Slot {
                seq,
                state: SlotState::Filled { data, arrival: now },
            };
        }

        // Extend highest_seq forward and mark any newly exposed slots as gaps.
        let diff = seq.wrapping_sub(highest) as i16;
        if diff > 0 {
            for i in 1..=(diff as u16) {
                let gap_seq = highest.wrapping_add(i);
                if gap_seq == seq {
                    continue;
                }
                let gidx = (gap_seq as usize) & self.mask;
                let slot = &mut self.slots[gidx];
                let replace = matches!(slot.state, SlotState::Empty) || slot.seq != gap_seq;
                if replace {
                    *slot = Slot {
                        seq: gap_seq,
                        state: SlotState::Gap { first_noticed: now },
                    };
                    outcome.new_gaps += 1;
                }
            }
            self.highest_seq = Some(seq);
        }

        outcome
    }

    /// Drain packets that are ready for delivery: either their own hold time
    /// has elapsed, or their sequence is a gap that has aged past the budget.
    pub fn drain_ready(&mut self, now: Instant, out: &mut Vec<DrainItem>) {
        loop {
            let base = match self.base_seq {
                Some(b) => b,
                None => return,
            };
            // Never drain past the highest sequence we've ever seen: beyond
            // that point slot state is uninitialised and can't be
            // distinguished from wrap-past victims without a spurious Lost.
            match self.highest_seq {
                Some(h) if (base.wrapping_sub(h) as i16) > 0 => return,
                None => return,
                _ => {}
            }
            let idx = (base as usize) & self.mask;
            let slot = &mut self.slots[idx];
            if slot.seq != base {
                // Ring has already wrapped past `base` — treat as lost to
                // resync without producing bogus payloads.
                *slot = Slot {
                    seq: 0,
                    state: SlotState::Empty,
                };
                out.push(DrainItem::Lost);
                self.base_seq = Some(base.wrapping_add(1));
                continue;
            }
            match &slot.state {
                SlotState::Filled { arrival, .. } => {
                    if now.saturating_duration_since(*arrival) >= self.buffer_time {
                        let payload = match std::mem::replace(
                            &mut slot.state,
                            SlotState::Empty,
                        ) {
                            SlotState::Filled { data, .. } => data,
                            _ => unreachable!(),
                        };
                        out.push(DrainItem::Delivered(payload));
                        self.base_seq = Some(base.wrapping_add(1));
                        continue;
                    }
                    return;
                }
                SlotState::Gap { first_noticed } => {
                    if now.saturating_duration_since(*first_noticed) >= self.buffer_time {
                        slot.state = SlotState::Empty;
                        out.push(DrainItem::Lost);
                        self.base_seq = Some(base.wrapping_add(1));
                        continue;
                    }
                    return;
                }
                SlotState::Empty => {
                    // Haven't seen this seq, and no later packet has noticed
                    // it as a gap — the sender simply hasn't transmitted yet.
                    return;
                }
            }
        }
    }

    /// Earliest monotonic time at which `drain_ready` will produce output,
    /// if such a deadline exists. Used to schedule the drain timer without
    /// waking on a hard fast tick.
    pub fn next_drain_time(&self) -> Option<Instant> {
        let base = self.base_seq?;
        let idx = (base as usize) & self.mask;
        let slot = &self.slots[idx];
        if slot.seq != base {
            // Stale slot ahead — drain ASAP.
            return Some(Instant::now());
        }
        match &slot.state {
            SlotState::Filled { arrival, .. } => Some(*arrival + self.buffer_time),
            SlotState::Gap { first_noticed } => Some(*first_noticed + self.buffer_time),
            SlotState::Empty => None,
        }
    }
}

impl std::fmt::Debug for ReorderBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReorderBuffer")
            .field("capacity", &self.capacity)
            .field("buffer_time", &self.buffer_time)
            .field("base_seq", &self.base_seq)
            .field("highest_seq", &self.highest_seq)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(n: u8) -> Bytes {
        Bytes::from(vec![n])
    }

    fn collect(buf: &mut ReorderBuffer, now: Instant) -> Vec<DrainItem> {
        let mut v = Vec::new();
        buf.drain_ready(now, &mut v);
        v
    }

    fn assert_delivered(items: &[DrainItem], expected: &[u8]) {
        let got: Vec<u8> = items
            .iter()
            .filter_map(|i| match i {
                DrainItem::Delivered(b) => Some(b[0]),
                DrainItem::Lost => None,
            })
            .collect();
        assert_eq!(&got, expected);
    }

    #[test]
    fn in_order_delivery_after_hold() {
        let mut buf = ReorderBuffer::new(Duration::from_millis(50));
        let t0 = Instant::now();
        buf.insert(10, b(1), t0);
        buf.insert(11, b(2), t0);

        assert!(collect(&mut buf, t0).is_empty());

        let items = collect(&mut buf, t0 + Duration::from_millis(50));
        assert_delivered(&items, &[1, 2]);
    }

    #[test]
    fn gap_filled_by_retransmit_before_timeout() {
        let mut buf = ReorderBuffer::new(Duration::from_millis(100));
        let t0 = Instant::now();
        buf.insert(10, b(1), t0);
        buf.insert(12, b(3), t0); // seq 11 is a gap

        // Nothing ready yet — seq 11 still has time
        assert!(collect(&mut buf, t0 + Duration::from_millis(40)).is_empty());

        // Retransmit arrives
        let outcome = buf.insert(11, b(2), t0 + Duration::from_millis(60));
        assert!(outcome.recovered);

        // After hold time elapses, deliver in order
        let items = collect(&mut buf, t0 + Duration::from_millis(200));
        assert_delivered(&items, &[1, 2, 3]);
    }

    #[test]
    fn gap_times_out_when_retransmit_never_arrives() {
        let mut buf = ReorderBuffer::new(Duration::from_millis(80));
        let t0 = Instant::now();
        buf.insert(10, b(1), t0);
        buf.insert(12, b(3), t0);

        let items = collect(&mut buf, t0 + Duration::from_millis(200));
        let mut iter = items.iter();
        match iter.next() {
            Some(DrainItem::Delivered(b)) if b[0] == 1 => {}
            _ => panic!("expected delivery of seq 10"),
        }
        match iter.next() {
            Some(DrainItem::Lost) => {}
            _ => panic!("expected lost marker for seq 11"),
        }
        match iter.next() {
            Some(DrainItem::Delivered(b)) if b[0] == 3 => {}
            _ => panic!("expected delivery of seq 12"),
        }
    }

    #[test]
    fn duplicate_arrivals_are_ignored() {
        let mut buf = ReorderBuffer::new(Duration::from_millis(10));
        let t0 = Instant::now();
        buf.insert(10, b(1), t0);
        let dup = buf.insert(10, b(99), t0);
        assert!(dup.duplicate);

        let items = collect(&mut buf, t0 + Duration::from_millis(20));
        assert_delivered(&items, &[1]);
    }

    #[test]
    fn stale_arrivals_are_rejected() {
        let mut buf = ReorderBuffer::new(Duration::from_millis(20));
        let t0 = Instant::now();
        buf.insert(10, b(1), t0);
        buf.insert(11, b(2), t0);
        let drained = collect(&mut buf, t0 + Duration::from_millis(30));
        assert_delivered(&drained, &[1, 2]);

        let late = buf.insert(10, b(9), t0 + Duration::from_millis(40));
        assert!(late.stale);
    }

    #[test]
    fn sequence_wraparound() {
        let mut buf = ReorderBuffer::new(Duration::from_millis(5));
        let t0 = Instant::now();
        buf.insert(65534, b(1), t0);
        buf.insert(65535, b(2), t0);
        buf.insert(0, b(3), t0);
        buf.insert(1, b(4), t0);

        let items = collect(&mut buf, t0 + Duration::from_millis(10));
        assert_delivered(&items, &[1, 2, 3, 4]);
    }

    #[test]
    fn next_drain_time_is_monotonic_with_arrivals() {
        let mut buf = ReorderBuffer::new(Duration::from_millis(100));
        assert!(buf.next_drain_time().is_none());
        let t0 = Instant::now();
        buf.insert(10, b(1), t0);
        assert_eq!(buf.next_drain_time(), Some(t0 + Duration::from_millis(100)));
    }
}
