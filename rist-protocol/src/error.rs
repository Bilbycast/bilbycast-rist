use thiserror::Error;

#[derive(Debug, Error)]
pub enum RistError {
    #[error("packet too short: expected at least {expected} bytes, got {actual}")]
    PacketTooShort { expected: usize, actual: usize },

    #[error("invalid RTP version: expected 2, got {0}")]
    InvalidRtpVersion(u8),

    #[error("invalid RTCP packet type: {0}")]
    InvalidRtcpType(u8),

    #[error("invalid RTCP length: header says {header_len} words but buffer has {actual} bytes")]
    InvalidRtcpLength { header_len: u16, actual: usize },

    #[error("invalid NACK format type: {0}")]
    InvalidNackFormat(u8),

    #[error("invalid port number: {0} (must be even for RTP)")]
    InvalidPort(u16),

    #[error("buffer overflow: retransmit buffer full")]
    BufferOverflow,

    #[error("tunnel error: {0}")]
    TunnelError(String),

    #[error("crypto error: {0}")]
    CryptoError(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, RistError>;
