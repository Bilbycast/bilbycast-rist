//! Dual-port UDP channel for RIST Simple Profile.
//!
//! RIST uses adjacent port pairs: RTP on even port P, RTCP on P+1.
//! This module manages both sockets.

use std::net::SocketAddr;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChannelError {
    #[error("RTP port must be even, got {0}")]
    OddPort(u16),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Default UDP receive buffer size (2 MB) for high-bandwidth media.
const DEFAULT_RECV_BUFFER: usize = 2 * 1024 * 1024;
/// Default UDP send buffer size (2 MB).
const DEFAULT_SEND_BUFFER: usize = 2 * 1024 * 1024;

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

        let rtp = Self::create_udp_socket(rtp_addr)?;
        let rtcp = Self::create_udp_socket(rtcp_addr)?;

        let local_rtp_addr = rtp.local_addr()?;
        let local_rtcp_addr = rtcp.local_addr()?;

        log::info!(
            "RIST channel bound: RTP={local_rtp_addr} RTCP={local_rtcp_addr} \
             (recv_buf={}KB, send_buf={}KB)",
            DEFAULT_RECV_BUFFER / 1024,
            DEFAULT_SEND_BUFFER / 1024,
        );

        Ok(RistChannel {
            rtp,
            rtcp,
            local_rtp_addr,
            local_rtcp_addr,
        })
    }

    /// Create a UDP socket with SO_REUSEADDR and large buffers for media traffic.
    fn create_udp_socket(addr: SocketAddr) -> Result<UdpSocket, std::io::Error> {
        let domain = if addr.is_ipv4() {
            Domain::IPV4
        } else {
            Domain::IPV6
        };
        let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.set_nonblocking(true)?;

        // Set large buffers for high-bandwidth media — kernel may cap these
        // but we request the maximum we need
        let _ = socket.set_recv_buffer_size(DEFAULT_RECV_BUFFER);
        let _ = socket.set_send_buffer_size(DEFAULT_SEND_BUFFER);

        socket.bind(&addr.into())?;
        UdpSocket::from_std(socket.into())
    }

    /// Get the remote RTCP address given a remote RTP address.
    pub fn rtcp_addr_for(rtp_addr: SocketAddr) -> SocketAddr {
        SocketAddr::new(rtp_addr.ip(), rtp_addr.port() + 1)
    }
}
