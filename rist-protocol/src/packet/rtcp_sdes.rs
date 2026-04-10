//! RTCP Source Description (SDES) packet (RFC 3550 Section 6.5).
//!
//! RIST SDES packets contain exactly one chunk with one CNAME item.
//! See TR-06-1:2020 Section 5.2.5.

use bytes::{BufMut, BytesMut};

use crate::error::{RistError, Result};

/// RTCP packet type for SDES.
pub const RTCP_PT_SDES: u8 = 202;

/// SDES item type for CNAME.
const SDES_CNAME: u8 = 1;

/// RTCP SDES packet with a single CNAME item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sdes {
    /// SSRC of the source.
    pub ssrc: u32,
    /// CNAME string (e.g., IP address or "user@host").
    pub cname: String,
}

impl Sdes {
    /// Parse from buffer (after common RTCP header has been read).
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 6 {
            return Err(RistError::PacketTooShort {
                expected: 6,
                actual: buf.len(),
            });
        }

        let ssrc = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        let item_type = buf[4];
        if item_type != SDES_CNAME {
            return Err(RistError::Other(format!(
                "expected SDES CNAME (type 1), got type {item_type}"
            )));
        }
        let name_len = buf[5] as usize;
        if buf.len() < 6 + name_len {
            return Err(RistError::PacketTooShort {
                expected: 6 + name_len,
                actual: buf.len(),
            });
        }
        let cname = String::from_utf8_lossy(&buf[6..6 + name_len]).into_owned();

        Ok(Sdes { ssrc, cname })
    }

    /// Serialize the full SDES packet including the common RTCP header.
    /// Padded to a 32-bit boundary per TR-06-1:2020 Section 5.2.5.
    pub fn serialize(&self) -> BytesMut {
        let cname_bytes = self.cname.as_bytes();
        let name_len = cname_bytes.len();
        // SSRC (4) + CNAME type (1) + length (1) + name + zero terminator(s)
        let chunk_len = 4 + 2 + name_len;
        // Pad to 32-bit boundary: add 1-4 zero bytes
        let padding_needed = (4 - (chunk_len % 4)) % 4;
        let padding_needed = if padding_needed == 0 { 4 } else { padding_needed }; // At least 1 zero byte
        let total_chunk_len = chunk_len + padding_needed;
        // length field = total packet size in 32-bit words minus 1
        let length_words = (4 + total_chunk_len) / 4 - 1;

        let mut buf = BytesMut::with_capacity(4 + total_chunk_len);

        // Common header: V=2, P=0, SC=1, PT=202
        buf.put_u8(0x81); // V=2, P=0, SC=1
        buf.put_u8(RTCP_PT_SDES);
        buf.put_u16(length_words as u16);

        // Chunk
        buf.put_u32(self.ssrc);
        buf.put_u8(SDES_CNAME);
        buf.put_u8(name_len as u8);
        buf.put_slice(cname_bytes);
        // Zero padding
        for _ in 0..padding_needed {
            buf.put_u8(0);
        }

        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sdes_roundtrip() {
        let sdes = Sdes {
            ssrc: 0x12345678,
            cname: "192.168.1.100".to_string(),
        };
        let bytes = sdes.serialize();
        // Verify 32-bit alignment
        assert_eq!(bytes.len() % 4, 0);

        let parsed = Sdes::parse(&bytes[4..]).unwrap();
        assert_eq!(parsed, sdes);
    }

    #[test]
    fn test_sdes_short_cname() {
        let sdes = Sdes {
            ssrc: 1,
            cname: "a".to_string(),
        };
        let bytes = sdes.serialize();
        assert_eq!(bytes.len() % 4, 0);
        let parsed = Sdes::parse(&bytes[4..]).unwrap();
        assert_eq!(parsed, sdes);
    }

    #[test]
    fn test_sdes_alignment() {
        // Test various CNAME lengths to ensure proper padding
        for len in 0..20 {
            let cname: String = (0..len).map(|_| 'x').collect();
            let sdes = Sdes { ssrc: 1, cname };
            let bytes = sdes.serialize();
            assert_eq!(bytes.len() % 4, 0, "Misaligned for CNAME length {len}");
        }
    }
}
