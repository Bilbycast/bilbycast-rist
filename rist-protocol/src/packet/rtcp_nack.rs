//! RTCP NACK packets for retransmission requests (RFC 4585).
//!
//! RIST supports two NACK formats:
//! - Bitmask-based (Generic NACK, PT=205, FMT=1) — RFC 4585 Section 6.2.1
//! - Range-based (APP NACK, PT=205, FMT=15) — TR-06-1:2020 Section 5.3.2.2

use bytes::{Buf, BufMut, BytesMut};

use crate::error::{RistError, Result};

/// RTCP packet type for Transport-layer Feedback (RTPFB).
pub const RTCP_PT_RTPFB: u8 = 205;
/// FMT for Generic NACK (bitmask-based).
pub const NACK_FMT_GENERIC: u8 = 1;
/// FMT for Range-based NACK (RIST-specific, uses FMT=15 APP).
pub const NACK_FMT_RANGE: u8 = 15;

/// A single bitmask NACK entry (RFC 4585 Section 6.2.1).
/// PID identifies one lost packet; BLP is a bitmask covering the next 16 packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitmaskNack {
    /// Packet ID — the sequence number of a lost packet.
    pub pid: u16,
    /// Bitmask of following lost packets. Bit 0 = pid+1, bit 1 = pid+2, etc.
    pub blp: u16,
}

impl BitmaskNack {
    /// Returns an iterator over all lost sequence numbers in this NACK.
    pub fn lost_seqs(&self) -> impl Iterator<Item = u16> + '_ {
        std::iter::once(self.pid).chain((0..16).filter_map(|i| {
            if self.blp & (1 << i) != 0 {
                Some(self.pid.wrapping_add(i + 1))
            } else {
                None
            }
        }))
    }
}

/// A single range NACK entry (TR-06-1:2020 Section 5.3.2.2).
/// Identifies a contiguous range of lost packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeNack {
    /// First lost sequence number.
    pub start: u16,
    /// Number of additional lost packets beyond `start`.
    /// Total lost = extra + 1.
    pub extra: u16,
}

impl RangeNack {
    /// Returns an iterator over all lost sequence numbers in this range.
    pub fn lost_seqs(&self) -> impl Iterator<Item = u16> {
        let start = self.start;
        let count = self.extra as u32 + 1;
        (0..count).map(move |i| start.wrapping_add(i as u16))
    }
}

/// A NACK packet containing one or more NACK entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NackPacket {
    /// SSRC of the packet sender (the receiver requesting retransmission).
    pub sender_ssrc: u32,
    /// SSRC of the media source whose packets are missing.
    pub media_ssrc: u32,
    /// NACK entries (either all bitmask or all range).
    pub entries: NackEntries,
}

/// NACK entry format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NackEntries {
    Bitmask(Vec<BitmaskNack>),
    Range(Vec<RangeNack>),
}

impl NackPacket {
    /// Parse from buffer (after common RTCP header has been read).
    /// `fmt` is the FMT field from the common header.
    /// `payload_len` is the number of bytes after the common header.
    pub fn parse(buf: &[u8], fmt: u8, payload_len: usize) -> Result<Self> {
        if buf.len() < 8 {
            return Err(RistError::PacketTooShort {
                expected: 8,
                actual: buf.len(),
            });
        }
        let mut r = &buf[..];
        let sender_ssrc = r.get_u32();
        let media_ssrc = r.get_u32();

        let fci_len = payload_len.saturating_sub(8);
        let num_entries = fci_len / 4;

        let entries = match fmt {
            NACK_FMT_GENERIC => {
                let mut nacks = Vec::with_capacity(num_entries);
                for _ in 0..num_entries {
                    let pid = r.get_u16();
                    let blp = r.get_u16();
                    nacks.push(BitmaskNack { pid, blp });
                }
                NackEntries::Bitmask(nacks)
            }
            NACK_FMT_RANGE => {
                let mut nacks = Vec::with_capacity(num_entries);
                for _ in 0..num_entries {
                    let start = r.get_u16();
                    let extra = r.get_u16();
                    nacks.push(RangeNack { start, extra });
                }
                NackEntries::Range(nacks)
            }
            _ => return Err(RistError::InvalidNackFormat(fmt)),
        };

        Ok(NackPacket {
            sender_ssrc,
            media_ssrc,
            entries,
        })
    }

    /// Serialize the full NACK packet including the common RTCP header.
    pub fn serialize(&self) -> BytesMut {
        let (fmt, entry_count) = match &self.entries {
            NackEntries::Bitmask(v) => (NACK_FMT_GENERIC, v.len()),
            NackEntries::Range(v) => (NACK_FMT_RANGE, v.len()),
        };
        // length = SSRCs(2 words) + FCIs(1 word each)
        let length_words = 2 + entry_count;
        let total_size = 4 + (length_words * 4);

        let mut buf = BytesMut::with_capacity(total_size);

        // Common header: V=2, P=0, FMT
        buf.put_u8(0x80 | (fmt & 0x1F));
        buf.put_u8(RTCP_PT_RTPFB);
        buf.put_u16(length_words as u16);
        buf.put_u32(self.sender_ssrc);
        buf.put_u32(self.media_ssrc);

        match &self.entries {
            NackEntries::Bitmask(nacks) => {
                for nack in nacks {
                    buf.put_u16(nack.pid);
                    buf.put_u16(nack.blp);
                }
            }
            NackEntries::Range(nacks) => {
                for nack in nacks {
                    buf.put_u16(nack.start);
                    buf.put_u16(nack.extra);
                }
            }
        }

        buf
    }
}

/// Builder that packs a list of lost sequence numbers into efficient NACK packets.
pub struct NackListBuilder {
    sender_ssrc: u32,
    media_ssrc: u32,
}

impl NackListBuilder {
    pub fn new(sender_ssrc: u32, media_ssrc: u32) -> Self {
        Self {
            sender_ssrc,
            media_ssrc,
        }
    }

    /// Build bitmask NACKs from a sorted list of lost sequence numbers.
    pub fn build_bitmask(&self, lost_seqs: &[u16]) -> NackPacket {
        let mut nacks = Vec::new();
        let mut i = 0;
        while i < lost_seqs.len() {
            let pid = lost_seqs[i];
            let mut blp: u16 = 0;
            i += 1;
            while i < lost_seqs.len() {
                let diff = lost_seqs[i].wrapping_sub(pid);
                if diff >= 1 && diff <= 16 {
                    blp |= 1 << (diff - 1);
                    i += 1;
                } else {
                    break;
                }
            }
            nacks.push(BitmaskNack { pid, blp });
        }
        NackPacket {
            sender_ssrc: self.sender_ssrc,
            media_ssrc: self.media_ssrc,
            entries: NackEntries::Bitmask(nacks),
        }
    }

    /// Build range NACKs from a sorted list of lost sequence numbers.
    pub fn build_range(&self, lost_seqs: &[u16]) -> NackPacket {
        let mut nacks = Vec::new();
        let mut i = 0;
        while i < lost_seqs.len() {
            let start = lost_seqs[i];
            let mut extra: u16 = 0;
            i += 1;
            while i < lost_seqs.len() {
                if lost_seqs[i] == start.wrapping_add(extra + 1) {
                    extra += 1;
                    i += 1;
                } else {
                    break;
                }
            }
            nacks.push(RangeNack { start, extra });
        }
        NackPacket {
            sender_ssrc: self.sender_ssrc,
            media_ssrc: self.media_ssrc,
            entries: NackEntries::Range(nacks),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitmask_nack_roundtrip() {
        let pkt = NackPacket {
            sender_ssrc: 0xAAAA,
            media_ssrc: 0xBBBB,
            entries: NackEntries::Bitmask(vec![
                BitmaskNack {
                    pid: 100,
                    blp: 0b0000_0000_0000_0101,
                },
                BitmaskNack { pid: 200, blp: 0 },
            ]),
        };
        let bytes = pkt.serialize();
        let payload_len = bytes.len() - 4;
        let parsed = NackPacket::parse(&bytes[4..], NACK_FMT_GENERIC, payload_len).unwrap();
        assert_eq!(parsed, pkt);
    }

    #[test]
    fn test_range_nack_roundtrip() {
        let pkt = NackPacket {
            sender_ssrc: 1,
            media_ssrc: 2,
            entries: NackEntries::Range(vec![
                RangeNack {
                    start: 500,
                    extra: 9,
                },
                RangeNack {
                    start: 520,
                    extra: 0,
                },
            ]),
        };
        let bytes = pkt.serialize();
        let payload_len = bytes.len() - 4;
        let parsed = NackPacket::parse(&bytes[4..], NACK_FMT_RANGE, payload_len).unwrap();
        assert_eq!(parsed, pkt);
    }

    #[test]
    fn test_bitmask_nack_lost_seqs() {
        let nack = BitmaskNack {
            pid: 100,
            blp: 0b0000_0000_0000_0101,
        };
        let seqs: Vec<u16> = nack.lost_seqs().collect();
        assert_eq!(seqs, vec![100, 101, 103]);
    }

    #[test]
    fn test_range_nack_lost_seqs() {
        let nack = RangeNack {
            start: 500,
            extra: 4,
        };
        let seqs: Vec<u16> = nack.lost_seqs().collect();
        assert_eq!(seqs, vec![500, 501, 502, 503, 504]);
    }

    #[test]
    fn test_nack_builder_bitmask() {
        let builder = NackListBuilder::new(1, 2);
        let pkt = builder.build_bitmask(&[100, 101, 103, 200]);
        match &pkt.entries {
            NackEntries::Bitmask(nacks) => {
                assert_eq!(nacks.len(), 2);
                assert_eq!(nacks[0].pid, 100);
                assert_eq!(nacks[0].blp, 0b0000_0000_0000_0101);
                assert_eq!(nacks[1].pid, 200);
                assert_eq!(nacks[1].blp, 0);
            }
            _ => panic!("expected bitmask"),
        }
    }

    #[test]
    fn test_nack_builder_range() {
        let builder = NackListBuilder::new(1, 2);
        let pkt = builder.build_range(&[100, 101, 102, 200, 201]);
        match &pkt.entries {
            NackEntries::Range(nacks) => {
                assert_eq!(nacks.len(), 2);
                assert_eq!(nacks[0], RangeNack { start: 100, extra: 2 });
                assert_eq!(nacks[1], RangeNack { start: 200, extra: 1 });
            }
            _ => panic!("expected range"),
        }
    }
}
