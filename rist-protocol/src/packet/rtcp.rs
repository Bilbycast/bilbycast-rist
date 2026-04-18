//! RTCP compound packet parsing and building.
//!
//! Per RFC 3550 and TR-06-1:2020 Section 5.2.1, RTCP packets are always sent
//! as compound packets — multiple RTCP sub-packets concatenated in a single
//! UDP datagram with no separators.

use bytes::{Buf, BytesMut};

use crate::error::{RistError, Result};

use super::rtcp_app::{RistApp, RTCP_PT_APP};
use super::rtcp_nack::{NackPacket, RTCP_PT_RTPFB};
use super::rtcp_rr::{ReceiverReport, RTCP_PT_RR};
use super::rtcp_sdes::{Sdes, RTCP_PT_SDES};
use super::rtcp_sr::{SenderReport, RTCP_PT_SR};

/// Common RTCP header (4 bytes).
#[derive(Debug, Clone, Copy)]
pub struct RtcpCommonHeader {
    /// Version (2 bits, always 2).
    pub version: u8,
    /// Padding flag.
    pub padding: bool,
    /// Count/subtype field (5 bits). Meaning depends on packet type:
    /// - SR/RR: reception report count (RC)
    /// - SDES: source count (SC)
    /// - APP: subtype
    /// - RTPFB: FMT
    pub count: u8,
    /// Packet type.
    pub packet_type: u8,
    /// Length in 32-bit words minus 1.
    pub length: u16,
}

impl RtcpCommonHeader {
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < 4 {
            return Err(RistError::PacketTooShort {
                expected: 4,
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
        let count = first & 0x1F;
        let packet_type = r.get_u8();
        let length = r.get_u16();

        Ok(RtcpCommonHeader {
            version,
            padding,
            count,
            packet_type,
            length,
        })
    }

    /// Total size of this RTCP packet in bytes (including the 4-byte common header).
    pub fn packet_size(&self) -> usize {
        (self.length as usize + 1) * 4
    }
}

/// A parsed RTCP packet.
#[derive(Debug, Clone)]
pub enum RtcpPacket {
    SenderReport(SenderReport),
    ReceiverReport(ReceiverReport),
    Sdes(Sdes),
    Nack(NackPacket),
    App(RistApp),
    /// Unknown packet type — preserved but not parsed.
    Unknown {
        packet_type: u8,
        data: Vec<u8>,
    },
}

/// A compound RTCP packet containing multiple sub-packets.
#[derive(Debug, Clone)]
pub struct RtcpCompound {
    pub packets: Vec<RtcpPacket>,
}

impl RtcpCompound {
    /// Parse a compound RTCP packet from a UDP datagram.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        let mut packets = Vec::new();
        let mut offset = 0;

        while offset + 4 <= buf.len() {
            let header = RtcpCommonHeader::parse(&buf[offset..])?;
            let pkt_size = header.packet_size();
            if offset + pkt_size > buf.len() {
                // Truncated packet — stop parsing
                break;
            }

            let payload = &buf[offset + 4..offset + pkt_size];

            // Parse sub-packets lazily and leniently: any per-packet parse
            // error demotes the sub-packet to `Unknown` instead of tearing
            // the whole compound down. librist interop relies on this —
            // peer extensions (XR echo PT=77 pre-0.2.8, new APP subtypes,
            // etc.) must not block the NACK that followed in the same
            // datagram.
            let pkt = match header.packet_type {
                RTCP_PT_SR => SenderReport::parse(payload)
                    .map(RtcpPacket::SenderReport)
                    .unwrap_or_else(|_| RtcpPacket::Unknown {
                        packet_type: header.packet_type,
                        data: payload.to_vec(),
                    }),
                RTCP_PT_RR => ReceiverReport::parse(payload, header.count)
                    .map(RtcpPacket::ReceiverReport)
                    .unwrap_or_else(|_| RtcpPacket::Unknown {
                        packet_type: header.packet_type,
                        data: payload.to_vec(),
                    }),
                RTCP_PT_SDES => Sdes::parse(payload)
                    .map(RtcpPacket::Sdes)
                    .unwrap_or_else(|_| RtcpPacket::Unknown {
                        packet_type: header.packet_type,
                        data: payload.to_vec(),
                    }),
                RTCP_PT_RTPFB => NackPacket::parse(payload, header.count, payload.len())
                    .map(RtcpPacket::Nack)
                    .unwrap_or_else(|_| RtcpPacket::Unknown {
                        packet_type: header.packet_type,
                        data: payload.to_vec(),
                    }),
                RTCP_PT_APP => RistApp::parse(payload, header.count)
                    .map(RtcpPacket::App)
                    .unwrap_or_else(|_| RtcpPacket::Unknown {
                        packet_type: header.packet_type,
                        data: payload.to_vec(),
                    }),
                pt => RtcpPacket::Unknown {
                    packet_type: pt,
                    data: payload.to_vec(),
                },
            };

            packets.push(pkt);
            offset += pkt_size;
        }

        Ok(RtcpCompound { packets })
    }

    /// Serialize all sub-packets into a single compound packet buffer.
    pub fn serialize(&self) -> BytesMut {
        let mut buf = BytesMut::new();
        for pkt in &self.packets {
            match pkt {
                RtcpPacket::SenderReport(sr) => buf.extend_from_slice(&sr.serialize()),
                RtcpPacket::ReceiverReport(rr) => buf.extend_from_slice(&rr.serialize()),
                RtcpPacket::Sdes(sdes) => buf.extend_from_slice(&sdes.serialize()),
                RtcpPacket::Nack(nack) => buf.extend_from_slice(&nack.serialize()),
                RtcpPacket::App(app) => buf.extend_from_slice(&app.serialize()),
                RtcpPacket::Unknown { .. } => {} // Skip unknown packets on serialize
            }
        }
        buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::rtcp_nack::{BitmaskNack, NackEntries};
    use crate::packet::rtcp_rr::ReportBlock;

    #[test]
    fn test_compound_sr_sdes() {
        // Build a compound packet: SR + SDES (sender's compound per TR-06-1)
        let compound = RtcpCompound {
            packets: vec![
                RtcpPacket::SenderReport(SenderReport {
                    ssrc: 0x11111111,
                    ntp_msw: 1000,
                    ntp_lsw: 2000,
                    rtp_timestamp: 90000,
                    sender_packet_count: 500,
                    sender_octet_count: 658000,
                }),
                RtcpPacket::Sdes(Sdes {
                    ssrc: 0x11111111,
                    cname: "10.0.0.1".to_string(),
                }),
            ],
        };

        let bytes = compound.serialize();
        let parsed = RtcpCompound::parse(&bytes).unwrap();
        assert_eq!(parsed.packets.len(), 2);

        match &parsed.packets[0] {
            RtcpPacket::SenderReport(sr) => assert_eq!(sr.ssrc, 0x11111111),
            _ => panic!("expected SR"),
        }
        match &parsed.packets[1] {
            RtcpPacket::Sdes(sdes) => assert_eq!(sdes.cname, "10.0.0.1"),
            _ => panic!("expected SDES"),
        }
    }

    #[test]
    fn test_compound_rr_sdes_nack() {
        // Build a receiver's compound packet: RR + SDES + NACK
        let compound = RtcpCompound {
            packets: vec![
                RtcpPacket::ReceiverReport(ReceiverReport {
                    ssrc: 0x22222222,
                    reports: vec![ReportBlock {
                        ssrc: 0x11111111,
                        fraction_lost: 10,
                        cumulative_lost: 5,
                        extended_highest_seq: 1000,
                        jitter: 100,
                        last_sr: 0,
                        delay_since_last_sr: 0,
                    }],
                }),
                RtcpPacket::Sdes(Sdes {
                    ssrc: 0x22222222,
                    cname: "10.0.0.2".to_string(),
                }),
                RtcpPacket::Nack(NackPacket {
                    sender_ssrc: 0x22222222,
                    media_ssrc: 0x11111111,
                    entries: NackEntries::Bitmask(vec![BitmaskNack { pid: 990, blp: 0 }]),
                }),
            ],
        };

        let bytes = compound.serialize();
        let parsed = RtcpCompound::parse(&bytes).unwrap();
        assert_eq!(parsed.packets.len(), 3);
    }
}
