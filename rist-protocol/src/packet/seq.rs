//! 16-bit RTP sequence number with wraparound arithmetic.

use std::fmt;

/// A 16-bit RTP sequence number with wraparound-aware arithmetic.
///
/// Comparison uses the half-space algorithm: two sequence numbers are compared
/// by looking at the signed difference in the 16-bit space. This correctly
/// handles wraparound (e.g., 65535 < 0 when 0 is "after" 65535).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SeqNo(pub u16);

impl SeqNo {
    /// Returns the signed distance from `self` to `other`.
    /// Positive means `other` is ahead of `self`.
    #[inline]
    pub fn diff(self, other: SeqNo) -> i32 {
        let d = other.0.wrapping_sub(self.0) as i16;
        d as i32
    }

    /// Returns true if `self` comes before `other` in sequence space.
    #[inline]
    pub fn precedes(self, other: SeqNo) -> bool {
        self.diff(other) > 0
    }

    /// Increment by one (wraps around).
    #[inline]
    pub fn next(self) -> SeqNo {
        SeqNo(self.0.wrapping_add(1))
    }

    /// Add an offset (wraps around).
    #[inline]
    pub fn add(self, n: u16) -> SeqNo {
        SeqNo(self.0.wrapping_add(n))
    }
}

impl PartialOrd for SeqNo {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SeqNo {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let d = self.diff(*other);
        d.cmp(&0)
    }
}

impl fmt::Debug for SeqNo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SeqNo({})", self.0)
    }
}

impl fmt::Display for SeqNo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u16> for SeqNo {
    fn from(v: u16) -> Self {
        SeqNo(v)
    }
}

impl From<SeqNo> for u16 {
    fn from(s: SeqNo) -> Self {
        s.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_ordering() {
        assert!(SeqNo(0).precedes(SeqNo(1)));
        assert!(SeqNo(100).precedes(SeqNo(200)));
        assert!(!SeqNo(200).precedes(SeqNo(100)));
    }

    #[test]
    fn test_wraparound() {
        // 65535 should precede 0 (0 is "after" 65535)
        assert!(SeqNo(65535).precedes(SeqNo(0)));
        assert!(SeqNo(65534).precedes(SeqNo(0)));
        // But 0 should not precede 65535
        assert!(!SeqNo(0).precedes(SeqNo(65535)));
    }

    #[test]
    fn test_diff() {
        assert_eq!(SeqNo(0).diff(SeqNo(1)), 1);
        assert_eq!(SeqNo(1).diff(SeqNo(0)), -1);
        assert_eq!(SeqNo(65535).diff(SeqNo(0)), 1);
        assert_eq!(SeqNo(0).diff(SeqNo(65535)), -1);
    }

    #[test]
    fn test_next() {
        assert_eq!(SeqNo(0).next(), SeqNo(1));
        assert_eq!(SeqNo(65535).next(), SeqNo(0));
    }

    #[test]
    fn test_add() {
        assert_eq!(SeqNo(65530).add(10), SeqNo(4));
    }

    #[test]
    fn test_half_space_boundary() {
        // At exactly half-space (32768), the direction is ambiguous.
        // By convention, diff >= 32768 is treated as "behind".
        assert_eq!(SeqNo(0).diff(SeqNo(32768)), -32768);
        assert_eq!(SeqNo(0).diff(SeqNo(32767)), 32767);
    }
}
