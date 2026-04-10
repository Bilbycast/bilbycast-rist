//! RTP packet header parsing and serialization (RFC 3550).
//!
//! RIST uses standard RTP for media transport. The baseline protocol is
//! SMPTE-2022-1/2 for MPEG-2 TS over RTP.

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::error::{RistError, Result};

/// Minimum RTP header size (fixed part, no CSRC or extensions).
pub const RTP_HEADER_SIZE: usize = 12;

/// RTP header (RFC 3550 Section 5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpHeader {
    /// RTP version, always 2.
    pub version: u8,
    /// Padding flag.
    pub padding: bool,
    /// Extension flag — indicates an extension header follows the fixed header.
    pub extension: bool,
    /// Number of CSRC identifiers following the fixed header.
    pub csrc_count: u8,
    /// Marker bit (application-specific, e.g., frame boundary).
    pub marker: bool,
    /// Payload type (7 bits).
    pub payload_type: u8,
    /// Sequence number (16 bits, wraps around).
    pub sequence_number: u16,
    /// RTP timestamp (32 bits, clock rate depends on payload type).
    pub timestamp: u32,
    /// Synchronization source identifier.
    pub ssrc: u32,
}

impl RtpHeader {
    /// Parse an RTP header from the given buffer.
    /// Returns the header and the number of bytes consumed (including CSRC entries).
    pub fn parse(buf: &[u8]) -> Result<(Self, usize)> {
        if buf.len() < RTP_HEADER_SIZE {
            return Err(RistError::PacketTooShort {
                expected: RTP_HEADER_SIZE,
                actual: buf.len(),
            });
        }

        let mut r = &buf[..];

        let first = r.get_u8();
        let version = (first >> 6) & 0x03;
        if version != 2 {
            return Err(RistError::InvalidRtpVersion(version));
        }
        let padding = (first >> 5) & 0x01 != 0;
        let extension = (first >> 4) & 0x01 != 0;
        let csrc_count = first & 0x0F;

        let second = r.get_u8();
        let marker = (second >> 7) & 0x01 != 0;
        let payload_type = second & 0x7F;

        let sequence_number = r.get_u16();
        let timestamp = r.get_u32();
        let ssrc = r.get_u32();

        let header_size = RTP_HEADER_SIZE + (csrc_count as usize) * 4;
        if buf.len() < header_size {
            return Err(RistError::PacketTooShort {
                expected: header_size,
                actual: buf.len(),
            });
        }

        Ok((
            RtpHeader {
                version,
                padding,
                extension,
                csrc_count,
                marker,
                payload_type,
                sequence_number,
                timestamp,
                ssrc,
            },
            header_size,
        ))
    }

    /// Serialize the RTP header into a new buffer.
    /// Does not include CSRC entries or extensions.
    pub fn serialize(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(RTP_HEADER_SIZE);

        let first = (self.version << 6)
            | ((self.padding as u8) << 5)
            | ((self.extension as u8) << 4)
            | (self.csrc_count & 0x0F);
        buf.put_u8(first);

        let second = ((self.marker as u8) << 7) | (self.payload_type & 0x7F);
        buf.put_u8(second);

        buf.put_u16(self.sequence_number);
        buf.put_u32(self.timestamp);
        buf.put_u32(self.ssrc);

        buf.freeze()
    }

    /// Create a new RTP header for MPEG-2 TS (payload type 33, per SMPTE 2022-1/2).
    pub fn new_ts(ssrc: u32, seq: u16, timestamp: u32) -> Self {
        RtpHeader {
            version: 2,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker: false,
            payload_type: 33, // MPEG-2 TS
            sequence_number: seq,
            timestamp,
            ssrc,
        }
    }
}

/// Full RTP packet: header + payload.
#[derive(Debug, Clone)]
pub struct RtpPacket {
    pub header: RtpHeader,
    pub payload: Bytes,
}

impl RtpPacket {
    /// Parse an RTP packet from a buffer.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let (header, header_size) = RtpHeader::parse(buf)?;
        // Skip CSRC entries (already accounted for in header_size)
        // Skip extension header if present
        let mut offset = header_size;
        if header.extension {
            if buf.len() < offset + 4 {
                return Err(RistError::PacketTooShort {
                    expected: offset + 4,
                    actual: buf.len(),
                });
            }
            let ext_len_words = u16::from_be_bytes([buf[offset + 2], buf[offset + 3]]) as usize;
            let ext_total = 4 + ext_len_words * 4;
            if buf.len() < offset + ext_total {
                return Err(RistError::PacketTooShort {
                    expected: offset + ext_total,
                    actual: buf.len(),
                });
            }
            offset += ext_total;
        }
        let payload = Bytes::copy_from_slice(&buf[offset..]);
        Ok(RtpPacket { header, payload })
    }

    /// Serialize the full packet (header + payload).
    pub fn serialize(&self) -> Bytes {
        let header_bytes = self.header.serialize();
        let mut buf = BytesMut::with_capacity(header_bytes.len() + self.payload.len());
        buf.extend_from_slice(&header_bytes);
        buf.extend_from_slice(&self.payload);
        buf.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rtp_header_roundtrip() {
        let header = RtpHeader::new_ts(0x12345678, 42, 1000);
        let bytes = header.serialize();
        let (parsed, size) = RtpHeader::parse(&bytes).unwrap();
        assert_eq!(size, RTP_HEADER_SIZE);
        assert_eq!(parsed, header);
    }

    #[test]
    fn test_rtp_packet_roundtrip() {
        let pkt = RtpPacket {
            header: RtpHeader::new_ts(0xAABBCCDD, 100, 90000),
            payload: Bytes::from_static(&[0x47, 0x00, 0x00, 0x10]),
        };
        let bytes = pkt.serialize();
        let parsed = RtpPacket::parse(&bytes).unwrap();
        assert_eq!(parsed.header, pkt.header);
        assert_eq!(parsed.payload, pkt.payload);
    }

    #[test]
    fn test_invalid_version() {
        let mut buf = [0u8; 12];
        buf[0] = 0b11_0_0_0000; // version 3
        assert!(matches!(
            RtpHeader::parse(&buf),
            Err(RistError::InvalidRtpVersion(3))
        ));
    }

    #[test]
    fn test_too_short() {
        let buf = [0u8; 4];
        assert!(matches!(
            RtpHeader::parse(&buf),
            Err(RistError::PacketTooShort { .. })
        ));
    }

    #[test]
    fn test_marker_and_payload_type() {
        let header = RtpHeader {
            version: 2,
            padding: false,
            extension: false,
            csrc_count: 0,
            marker: true,
            payload_type: 96,
            sequence_number: 0,
            timestamp: 0,
            ssrc: 0,
        };
        let bytes = header.serialize();
        let (parsed, _) = RtpHeader::parse(&bytes).unwrap();
        assert!(parsed.marker);
        assert_eq!(parsed.payload_type, 96);
    }
}
