//! RTCP Sender Report (SR) packet (RFC 3550 Section 6.4.1).
//!
//! RIST SR packets have RC=0 (no reception report blocks) and length=6.
//! See TR-06-1:2020 Section 5.2.2.

use bytes::{Buf, BufMut, BytesMut};

use crate::error::{RistError, Result};

/// RTCP packet type for Sender Report.
pub const RTCP_PT_SR: u8 = 200;

/// RIST Sender Report. Always has RC=0, length=6.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SenderReport {
    /// SSRC of the sender.
    pub ssrc: u32,
    /// NTP timestamp, most significant 32 bits (seconds since 1900-01-01).
    pub ntp_msw: u32,
    /// NTP timestamp, least significant 32 bits (fractional seconds).
    pub ntp_lsw: u32,
    /// RTP timestamp corresponding to the NTP timestamp.
    pub rtp_timestamp: u32,
    /// Total RTP data packets sent.
    pub sender_packet_count: u32,
    /// Total RTP payload bytes sent.
    pub sender_octet_count: u32,
}

impl SenderReport {
    /// Size of SR in bytes (4-byte common header + 24 bytes sender info).
    pub const SIZE: usize = 28;

    /// Parse from buffer (after common RTCP header has been read).
    /// `buf` should start at the SSRC field (past the 4-byte common header).
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 24 {
            return Err(RistError::PacketTooShort {
                expected: 24,
                actual: buf.len(),
            });
        }
        let mut r = &buf[..];
        let ssrc = r.get_u32();
        let ntp_msw = r.get_u32();
        let ntp_lsw = r.get_u32();
        let rtp_timestamp = r.get_u32();
        let sender_packet_count = r.get_u32();
        let sender_octet_count = r.get_u32();

        Ok(SenderReport {
            ssrc,
            ntp_msw,
            ntp_lsw,
            rtp_timestamp,
            sender_packet_count,
            sender_octet_count,
        })
    }

    /// Serialize the full SR packet including the common RTCP header.
    pub fn serialize(&self) -> BytesMut {
        let mut buf = BytesMut::with_capacity(Self::SIZE);
        // Common header: V=2, P=0, RC=0, PT=200, length=6
        buf.put_u8(0x80); // V=2, P=0, RC=0
        buf.put_u8(RTCP_PT_SR);
        buf.put_u16(6); // length in 32-bit words minus 1
        buf.put_u32(self.ssrc);
        buf.put_u32(self.ntp_msw);
        buf.put_u32(self.ntp_lsw);
        buf.put_u32(self.rtp_timestamp);
        buf.put_u32(self.sender_packet_count);
        buf.put_u32(self.sender_octet_count);
        buf
    }

    /// Get the compact NTP timestamp (middle 32 bits) for use in RR's LSR field.
    pub fn compact_ntp(&self) -> u32 {
        ((self.ntp_msw & 0xFFFF) << 16) | ((self.ntp_lsw >> 16) & 0xFFFF)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sr_roundtrip() {
        let sr = SenderReport {
            ssrc: 0x12345678,
            ntp_msw: 3_800_000_000,
            ntp_lsw: 0x80000000,
            rtp_timestamp: 90000,
            sender_packet_count: 1000,
            sender_octet_count: 1_316_000,
        };
        let bytes = sr.serialize();
        assert_eq!(bytes.len(), SenderReport::SIZE);

        // Skip common header (4 bytes)
        let parsed = SenderReport::parse(&bytes[4..]).unwrap();
        assert_eq!(parsed, sr);
    }

    #[test]
    fn test_compact_ntp() {
        let sr = SenderReport {
            ssrc: 0,
            ntp_msw: 0x0000ABCD,
            ntp_lsw: 0xEF120000,
            rtp_timestamp: 0,
            sender_packet_count: 0,
            sender_octet_count: 0,
        };
        assert_eq!(sr.compact_ntp(), 0xABCDEF12);
    }
}
