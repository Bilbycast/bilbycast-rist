//! Dual-port UDP channel for RIST Simple Profile.
//!
//! RIST uses adjacent port pairs: RTP on even port P, RTCP on P+1.
//! This module manages both sockets.

use std::net::SocketAddr;

use tokio::net::UdpSocket;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("RTP port must be even, got {0}")]
    OddPort(u16),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// A pair of UDP sockets for RTP (even port) and RTCP (even port + 1).
pub struct RistChannel {
    /// RTP data socket (even port).
    pub rtp: UdpSocket,
    /// RTCP control socket (even port + 1).
    pub rtcp: UdpSocket,
    /// Local RTP address.
    pub local_rtp_addr: SocketAddr,
    /// Local RTCP address.
    pub local_rtcp_addr: SocketAddr,
}

impl RistChannel {
    /// Bind a RIST channel on the given RTP address.
    /// The port must be even; RTCP will bind on port + 1.
    pub async fn bind(rtp_addr: SocketAddr) -> Result<Self, ChannelError> {
        if rtp_addr.port() % 2 != 0 {
            return Err(ChannelError::OddPort(rtp_addr.port()));
        }

        let rtcp_addr = SocketAddr::new(rtp_addr.ip(), rtp_addr.port() + 1);

        let rtp = UdpSocket::bind(rtp_addr).await?;
        let rtcp = UdpSocket::bind(rtcp_addr).await?;

        let local_rtp_addr = rtp.local_addr()?;
        let local_rtcp_addr = rtcp.local_addr()?;

        Ok(RistChannel {
            rtp,
            rtcp,
            local_rtp_addr,
            local_rtcp_addr,
        })
    }

    /// Get the remote RTCP address given a remote RTP address.
    pub fn rtcp_addr_for(rtp_addr: SocketAddr) -> SocketAddr {
        SocketAddr::new(rtp_addr.ip(), rtp_addr.port() + 1)
    }
}
