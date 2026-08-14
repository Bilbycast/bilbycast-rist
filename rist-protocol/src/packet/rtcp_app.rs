//! RTCP APP packets used by RIST (PT=204).
//!
//! Multiple RIST messages share PT=204 with an ASCII name of `"RIST"`. The
//! subtype field (5 bits in the count position of the common RTCP header)
//! selects the payload:
//!
//! | Subtype | Meaning                                 | Size |
//! |--------:|-----------------------------------------|------|
//! | 0       | Range NACK (librist default, interop)   | 12 + 4·N bytes |
//! | 2       | RTT Echo Request (TR-06-1:2020 §5.2.6)  | 16 bytes        |
//! | 3       | RTT Echo Response                       | 20 bytes        |
//!
//! `RistApp::parse` accepts any subtype and returns a best-effort variant so
//! a peer's extension doesn't tear down the whole RTCP compound.

use bytes::{Buf, BufMut, BytesMut};

use crate::error::{RistError, Result};

/// RTCP packet type for APP.
pub const RTCP_PT_APP: u8 = 204;

/// Subtype for librist-style Range NACK (PT=204 APP "RIST"). Default NACK
/// format emitted by librist 0.2.11 unless the application explicitly opts
/// into the RFC 4585 PT=205 bitmask form.
pub const RIST_APP_RANGE_NACK: u8 = 0;
/// Subtype for RTT Echo Request.
pub const RTT_ECHO_REQUEST: u8 = 2;
/// Subtype for RTT Echo Response.
pub const RTT_ECHO_RESPONSE: u8 = 3;

/// ASCII name field for RIST APP packets.
const RIST_APP_NAME: [u8; 4] = *b"RIST";

/// Hard ceiling on the number of `(start, extra)` range entries kept from one
/// APP Range NACK. Entries past this are discarded, not rejected — a lenient
/// parse is what keeps librist extensions from tearing a compound down.
///
/// **Why 512 cannot reject a NACK librist legitimately sends.** Two
/// independent ceilings sit below it:
///
/// 1. librist 0.2.11 caps itself at **200 entries per NACK packet**. Its
///    writer logs `"nack max counter per packet (%d) exceeded. Skipping the
///    rest"` with the literal `200` (verified in the shipped
///    `librist.so.4.4.0` — the log call site loads `$0xc8` into the vararg
///    register feeding that `%d`).
/// 2. A RIST RTCP datagram is read into a 2048-byte buffer
///    (`rist_transport::receiver::MAX_UDP_RECV`, and the sender's own RTCP
///    buffer). After the 4-byte common header + SSRC + `"RIST"` name, at most
///    `(2048 - 12) / 4 = 509` entries can physically arrive in one datagram.
///
/// So 512 is above both the peer's self-imposed limit and the wire's physical
/// limit: no NACK that can reach this parser intact is ever truncated. What it
/// does bound is a hostile caller handing `parse` a large buffer — the entry
/// `Vec` is then capped instead of scaling with the input.
pub const MAX_RANGE_NACK_ENTRIES: usize = 512;

/// RTT Echo Request packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RttEchoRequest {
    /// SSRC of the media source.
    pub ssrc: u32,
    /// Timestamp (64-bit, sender's local time). Opaque — echoed back unchanged.
    pub timestamp_msw: u32,
    pub timestamp_lsw: u32,
}

/// RTT Echo Response packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RttEchoResponse {
    /// SSRC of the media source.
    pub ssrc: u32,
    /// Echoed timestamp from the request.
    pub timestamp_msw: u32,
    pub timestamp_lsw: u32,
    /// Processing delay in microseconds (time between receiving request and sending response).
    pub processing_delay_us: u32,
}

/// librist Range NACK carried inside the APP "RIST" envelope.
///
/// Wire layout (after the 4-byte common RTCP header, per
/// `rist_rtcp_nack_range` in libRIST `src/proto/rtp.h`):
///
/// ```text
///  0                   1                   2                   3
///  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
/// +---------------------------------------------------------------+
/// |                      SSRC (media source)                      |
/// +---------------------------------------------------------------+
/// |                         "RIST" ASCII                          |
/// +---------------------------------------------------------------+
/// |        start #1               |          extra #1             |
/// +---------------------------------------------------------------+
/// |        start #N               |          extra #N             |
/// +---------------------------------------------------------------+
/// ```
///
/// `extra = N` means `N` additional consecutive seqs are lost after `start`,
/// so the total run is `N + 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeNack {
    /// SSRC of the media source whose packets are missing.
    pub media_ssrc: u32,
    /// Range entries, each `(start, extra)`.
    pub entries: Vec<(u16, u16)>,
}

/// Parsed RIST APP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RistApp {
    RttEchoRequest(RttEchoRequest),
    RttEchoResponse(RttEchoResponse),
    /// librist Range NACK carried as PT=204 APP "RIST" subtype 0.
    RangeNack(RangeNack),
    /// Any other PT=204 APP "RIST" subtype — preserved to keep the
    /// compound-level parse lenient. The `data` is the raw payload
    /// AFTER the common header (i.e., starting with the SSRC field).
    Unknown { subtype: u8, data: Vec<u8> },
}

impl RistApp {
    /// Parse from buffer (after common RTCP header has been read).
    /// `subtype` is from the common header (5 bits where RC normally goes).
    ///
    /// Unknown subtypes are preserved as [`RistApp::Unknown`] rather than
    /// returning `Err` — a stray librist extension inside an RTCP compound
    /// must not cause the peer's NACKs (in an earlier or later sub-packet)
    /// to be rejected.
    pub fn parse(buf: &[u8], subtype: u8) -> Result<Self> {
        // Minimum APP body: SSRC(4) + name(4) = 8 bytes. Everything shorter
        // is malformed and a hard parse error.
        if buf.len() < 8 {
            return Err(RistError::PacketTooShort {
                expected: 8,
                actual: buf.len(),
            });
        }
        let mut r = &buf[..];
        let ssrc = r.get_u32();
        let mut name = [0u8; 4];
        r.copy_to_slice(&mut name);
        if name != RIST_APP_NAME {
            // Foreign APP packet inside the compound — treat as unknown
            // rather than tearing the compound down.
            return Ok(RistApp::Unknown {
                subtype,
                data: buf.to_vec(),
            });
        }

        match subtype {
            RIST_APP_RANGE_NACK => {
                // librist Range NACK: after SSRC(4) + "RIST"(4), the rest is
                // an array of (u16 start, u16 extra) entries, capped at
                // MAX_RANGE_NACK_ENTRIES (see the constant for why that
                // ceiling is unreachable for real librist traffic).
                let mut entries =
                    Vec::with_capacity((r.remaining() / 4).min(MAX_RANGE_NACK_ENTRIES));
                while r.remaining() >= 4 && entries.len() < MAX_RANGE_NACK_ENTRIES {
                    let start = r.get_u16();
                    let extra = r.get_u16();
                    entries.push((start, extra));
                }
                Ok(RistApp::RangeNack(RangeNack {
                    media_ssrc: ssrc,
                    entries,
                }))
            }
            RTT_ECHO_REQUEST => {
                if r.remaining() < 8 {
                    return Err(RistError::PacketTooShort {
                        expected: 16,
                        actual: buf.len(),
                    });
                }
                let timestamp_msw = r.get_u32();
                let timestamp_lsw = r.get_u32();
                Ok(RistApp::RttEchoRequest(RttEchoRequest {
                    ssrc,
                    timestamp_msw,
                    timestamp_lsw,
                }))
            }
            RTT_ECHO_RESPONSE => {
                if r.remaining() < 12 {
                    return Err(RistError::PacketTooShort {
                        expected: 20,
                        actual: buf.len(),
                    });
                }
                let timestamp_msw = r.get_u32();
                let timestamp_lsw = r.get_u32();
                let processing_delay_us = r.get_u32();
                Ok(RistApp::RttEchoResponse(RttEchoResponse {
                    ssrc,
                    timestamp_msw,
                    timestamp_lsw,
                    processing_delay_us,
                }))
            }
            _ => Ok(RistApp::Unknown {
                subtype,
                data: buf.to_vec(),
            }),
        }
    }

    /// Serialize the full APP packet including common RTCP header.
    pub fn serialize(&self) -> BytesMut {
        match self {
            RistApp::RttEchoRequest(req) => {
                // Header(4) + SSRC(4) + name(4) + timestamp(8) = 20 bytes
                // length = 4 (words minus 1)
                let mut buf = BytesMut::with_capacity(20);
                buf.put_u8(0x80 | RTT_ECHO_REQUEST);
                buf.put_u8(RTCP_PT_APP);
                buf.put_u16(4); // length in words - 1
                buf.put_u32(req.ssrc);
                buf.put_slice(&RIST_APP_NAME);
                buf.put_u32(req.timestamp_msw);
                buf.put_u32(req.timestamp_lsw);
                buf
            }
            RistApp::RttEchoResponse(resp) => {
                // Header(4) + SSRC(4) + name(4) + timestamp(8) + delay(4) = 24 bytes
                // length = 5 (words minus 1)
                let mut buf = BytesMut::with_capacity(24);
                buf.put_u8(0x80 | RTT_ECHO_RESPONSE);
                buf.put_u8(RTCP_PT_APP);
                buf.put_u16(5); // length in words - 1
                buf.put_u32(resp.ssrc);
                buf.put_slice(&RIST_APP_NAME);
                buf.put_u32(resp.timestamp_msw);
                buf.put_u32(resp.timestamp_lsw);
                buf.put_u32(resp.processing_delay_us);
                buf
            }
            RistApp::RangeNack(nack) => {
                // Header(4) + SSRC(4) + name(4) + N * range(4)
                let total = 12 + 4 * nack.entries.len();
                let length_words = (total / 4) - 1;
                let mut buf = BytesMut::with_capacity(total);
                buf.put_u8(0x80 | RIST_APP_RANGE_NACK);
                buf.put_u8(RTCP_PT_APP);
                buf.put_u16(length_words as u16);
                buf.put_u32(nack.media_ssrc);
                buf.put_slice(&RIST_APP_NAME);
                for (start, extra) in &nack.entries {
                    buf.put_u16(*start);
                    buf.put_u16(*extra);
                }
                buf
            }
            RistApp::Unknown { subtype, data } => {
                let total = 4 + data.len();
                let length_words = (total / 4).saturating_sub(1);
                let mut buf = BytesMut::with_capacity(total);
                buf.put_u8(0x80 | (*subtype & 0x1F));
                buf.put_u8(RTCP_PT_APP);
                buf.put_u16(length_words as u16);
                buf.put_slice(data);
                buf
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rtt_echo_request_roundtrip() {
        let req = RistApp::RttEchoRequest(RttEchoRequest {
            ssrc: 0x12345678,
            timestamp_msw: 1000,
            timestamp_lsw: 2000,
        });
        let bytes = req.serialize();
        assert_eq!(bytes.len(), 20);

        let subtype = bytes[0] & 0x1F;
        let parsed = RistApp::parse(&bytes[4..], subtype).unwrap();
        assert_eq!(parsed, req);
    }

    /// A 2048-byte RTCP datagram can carry 509 Range NACK entries, and each
    /// entry can name a 65 536-seq run — the sender expands every one of them
    /// into retransmit work. The parsed entry array must therefore be capped
    /// rather than scaling with whatever the peer declared.
    ///
    /// The cap sits above both librist's own 200-entries-per-packet limit and
    /// the 509 that physically fit in a datagram, so this can only bind on a
    /// buffer no real peer can deliver.
    #[test]
    fn range_nack_entry_array_is_capped() {
        // Body: SSRC(4) + "RIST"(4) + 4 bytes per entry.
        let over = MAX_RANGE_NACK_ENTRIES + 250;
        let mut body = Vec::with_capacity(8 + 4 * over);
        body.extend_from_slice(&0u32.to_be_bytes());
        body.extend_from_slice(b"RIST");
        for _ in 0..over {
            body.extend_from_slice(&[0x00, 0x00, 0xFF, 0xFF]);
        }

        match RistApp::parse(&body, RIST_APP_RANGE_NACK).unwrap() {
            RistApp::RangeNack(n) => {
                assert_eq!(n.entries.len(), MAX_RANGE_NACK_ENTRIES);
                assert_eq!(n.entries[0], (0, 0xFFFF));
            }
            other => panic!("expected RangeNack, got {other:?}"),
        }
    }

    /// The cap must not touch a NACK that fits the wire: 509 entries is the
    /// most a 2048-byte RTCP datagram can hold, and librist itself stops at
    /// 200, so both must round-trip whole.
    #[test]
    fn range_nack_keeps_every_entry_a_real_peer_can_send() {
        for count in [200usize, 509] {
            let entries: Vec<(u16, u16)> =
                (0..count).map(|i| (i as u16, (i % 7) as u16)).collect();
            let nack = RistApp::RangeNack(RangeNack {
                media_ssrc: 0x1234_5678,
                entries: entries.clone(),
            });
            let bytes = nack.serialize();
            assert!(bytes.len() <= 2048, "{count} entries must fit a datagram");
            let parsed = RistApp::parse(&bytes[4..], bytes[0] & 0x1F).unwrap();
            match parsed {
                RistApp::RangeNack(n) => assert_eq!(n.entries, entries),
                other => panic!("expected RangeNack, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_rtt_echo_response_roundtrip() {
        let resp = RistApp::RttEchoResponse(RttEchoResponse {
            ssrc: 0xAABBCCDD,
            timestamp_msw: 3000,
            timestamp_lsw: 4000,
            processing_delay_us: 150,
        });
        let bytes = resp.serialize();
        assert_eq!(bytes.len(), 24);

        let subtype = bytes[0] & 0x1F;
        let parsed = RistApp::parse(&bytes[4..], subtype).unwrap();
        assert_eq!(parsed, resp);
    }
}
