//! GRE header with RIST extensions (TR-06-2:2024 Section 5.1-5.2).
//!
//! Stubbed for Phase 2 — types only.

/// VSF EtherType for RIST Main Profile (recommended, TR-06-2:2022+).
pub const VSF_ETHERTYPE: u16 = 0xCCE0;

/// Legacy EtherType for Reduced Overhead (deprecated, TR-06-2:2020-2021).
pub const LEGACY_REDUCED_ETHERTYPE: u16 = 0x88B6;

/// Legacy EtherType for Keep-Alive (deprecated, TR-06-2:2020-2021).
pub const LEGACY_KEEPALIVE_ETHERTYPE: u16 = 0x88B5;

/// RIST version field (3 bits in GRE Reserved0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RistVersion {
    /// RV=000: TR-06-2:2020
    V2020 = 0,
    /// RV=001: TR-06-2:2021
    V2021 = 1,
    /// RV=010: TR-06-2:2022+
    V2022 = 2,
}

/// GRE header for RIST (RFC 8086 with RIST extensions).
#[derive(Debug, Clone)]
pub struct GreHeader {
    /// Checksum present flag.
    pub c_flag: bool,
    /// Key present flag (set to 1 for PSK mode).
    pub k_flag: bool,
    /// Sequence number present flag.
    pub s_flag: bool,
    /// AES key length hint (H bit): 0 = 128-bit, 1 = 256-bit.
    pub h_bit: bool,
    /// RIST version (3 bits).
    pub rist_version: RistVersion,
    /// Protocol type (EtherType).
    pub protocol_type: u16,
    /// Optional key field (present if k_flag is set).
    pub key: Option<u32>,
    /// Optional sequence number (present if s_flag is set).
    pub sequence: Option<u32>,
}

/// VSF packet header (after GRE header when protocol_type = 0xCCE0).
#[derive(Debug, Clone)]
pub struct VsfPacketHeader {
    /// VSF Protocol Type (0x0000 = RIST Packet).
    pub protocol_type: u16,
    /// VSF Protocol Subtype.
    /// Data: 0x0000 = Reduced Overhead.
    /// Control: 0x8000 = Keep-Alive, 0x8001 = Future Nonce Announcement.
    pub protocol_subtype: u16,
}
