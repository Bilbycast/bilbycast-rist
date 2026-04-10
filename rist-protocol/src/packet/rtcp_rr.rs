//! RTCP Receiver Report (RR) packet (RFC 3550 Section 6.4.2).
//!
//! RIST RR packets have RC=1 (one report block) and length=7.
//! An empty RR (RC=0, length=1) may be sent by senders instead of SR.
//! See TR-06-1:2020 Sections 5.2.3 and 5.2.4.

use bytes::{Buf, BufMut, BytesMut};

use crate::error::{RistError, Result};

/// RTCP packet type for Receiver Report.
pub const RTCP_PT_RR: u8 = 201;

/// A single reception report block (24 bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportBlock {
    /// SSRC of the source being reported on.
    pub ssrc: u32,
    /// Fraction of packets lost since last RR (8 bits, 0-255 = 0%-100%).
    pub fraction_lost: u8,
    /// Cumulative number of packets lost (24 bits, signed).
    pub cumulative_lost: i32,
    /// Extended highest sequence number received.
    pub extended_highest_seq: u32,
    /// Interarrival jitter estimate.
    pub jitter: u32,
    /// Last SR timestamp (compact NTP, middle 32 bits).
    pub last_sr: u32,
    /// Delay since last SR in 1/65536 seconds.
    pub delay_since_last_sr: u32,
}

impl ReportBlock {
    pub const SIZE: usize = 24;

    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < Self::SIZE {
            return Err(RistError::PacketTooShort {
                expected: Self::SIZE,
                actual: buf.len(),
            });
        }
        let mut r = &buf[..];
        let ssrc = r.get_u32();
        let lost_word = r.get_u32();
        let fraction_lost = (lost_word >> 24) as u8;
        // Cumulative lost is 24-bit signed
        let cumulative_raw = (lost_word & 0x00FFFFFF) as i32;
        let cumulative_lost = if cumulative_raw & 0x800000 != 0 {
            cumulative_raw | !0x00FFFFFF_i32 // sign-extend
        } else {
            cumulative_raw
        };
        let extended_highest_seq = r.get_u32();
        let jitter = r.get_u32();
        let last_sr = r.get_u32();
        let delay_since_last_sr = r.get_u32();

        Ok(ReportBlock {
            ssrc,
            fraction_lost,
            cumulative_lost,
            extended_highest_seq,
            jitter,
            last_sr,
            delay_since_last_sr,
        })
    }

    pub fn serialize(&self, buf: &mut BytesMut) {
        buf.put_u32(self.ssrc);
        let lost_word =
            ((self.fraction_lost as u32) << 24) | ((self.cumulative_lost as u32) & 0x00FFFFFF);
        buf.put_u32(lost_word);
        buf.put_u32(self.extended_highest_seq);
        buf.put_u32(self.jitter);
        buf.put_u32(self.last_sr);
        buf.put_u32(self.delay_since_last_sr);
    }
}

/// RTCP Receiver Report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverReport {
    /// SSRC of the packet sender (the receiver generating this report).
    pub ssrc: u32,
    /// Reception report blocks (RIST uses exactly one, or zero for empty RR).
    pub reports: Vec<ReportBlock>,
}

impl ReceiverReport {
    /// Parse from buffer (after common RTCP header has been read).
    /// `rc` is the reception report count from the common header.
    pub fn parse(buf: &[u8], rc: u8) -> Result<Self> {
        if buf.len() < 4 {
            return Err(RistError::PacketTooShort {
                expected: 4,
                actual: buf.len(),
            });
        }
        let mut r = &buf[..];
        let ssrc = r.get_u32();

        let mut reports = Vec::with_capacity(rc as usize);
        let mut offset = 4;
        for _ in 0..rc {
            let block = ReportBlock::parse(&buf[offset..])?;
            offset += ReportBlock::SIZE;
            reports.push(block);
        }

        Ok(ReceiverReport { ssrc, reports })
    }

    /// Serialize the full RR packet including the common RTCP header.
    pub fn serialize(&self) -> BytesMut {
        let rc = self.reports.len() as u8;
        // length in 32-bit words minus 1: header(1) + SSRC(1) + reports(6 each)
        let length = 1 + (rc as u16) * 6;
        let total_size = 4 + 4 + (rc as usize) * ReportBlock::SIZE;
        let mut buf = BytesMut::with_capacity(total_size);

        // Common header: V=2, P=0, RC=rc, PT=201
        buf.put_u8(0x80 | (rc & 0x1F));
        buf.put_u8(RTCP_PT_RR);
        buf.put_u16(length);
        buf.put_u32(self.ssrc);

        for report in &self.reports {
            report.serialize(&mut buf);
        }

        buf
    }

    /// Create an empty RR (RC=0, used by senders as keepalive per TR-06-1 Section 5.2.3).
    pub fn empty(ssrc: u32) -> Self {
        ReceiverReport {
            ssrc,
            reports: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rr_with_report_roundtrip() {
        let rr = ReceiverReport {
            ssrc: 0xAABBCCDD,
            reports: vec![ReportBlock {
                ssrc: 0x12345678,
                fraction_lost: 25,
                cumulative_lost: 100,
                extended_highest_seq: 50000,
                jitter: 300,
                last_sr: 0xABCDEF12,
                delay_since_last_sr: 65536,
            }],
        };
        let bytes = rr.serialize();
        // length field should be 7 (1 + 6)
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 7);

        let rc = bytes[0] & 0x1F;
        let parsed = ReceiverReport::parse(&bytes[4..], rc).unwrap();
        assert_eq!(parsed, rr);
    }

    #[test]
    fn test_empty_rr() {
        let rr = ReceiverReport::empty(0xDEADBEEF);
        let bytes = rr.serialize();
        // length field should be 1
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 1);
        assert_eq!(bytes.len(), 8);
    }

    #[test]
    fn test_negative_cumulative_lost() {
        let block = ReportBlock {
            ssrc: 1,
            fraction_lost: 0,
            cumulative_lost: -5, // Can be negative if packets duplicated
            extended_highest_seq: 100,
            jitter: 0,
            last_sr: 0,
            delay_since_last_sr: 0,
        };
        let mut buf = BytesMut::new();
        block.serialize(&mut buf);
        let parsed = ReportBlock::parse(&buf).unwrap();
        assert_eq!(parsed.cumulative_lost, -5);
    }
}
