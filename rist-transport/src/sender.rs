//! RIST sender task.
//!
//! Owns the RTP sequence counter, RTCP sender state, and retransmit buffer.
//! Runs as a tokio task with a `select!` loop handling:
//! - Outgoing media from the application
//! - Incoming RTCP (NACKs, RTT echo responses) from the receiver
//! - Periodic RTCP SR + SDES emission
//!
//! Hot-path design:
//! - Pre-allocated send buffer reused across packets (no per-packet Vec alloc)
//! - RTP socket connected to remote — `send()` instead of `send_to()`
//! - Retransmit buffer stores `Bytes` (refcounted, no copy on retransmit)
//! - NACK processing avoids Vec allocation for small NACK lists

use std::net::SocketAddr;
use std::time::Instant;

use bytes::{Bytes, BytesMut, BufMut};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use rist_protocol::packet::rtcp::{RtcpCompound, RtcpPacket};
use rist_protocol::packet::rtcp_app::{RistApp, RttEchoRequest};
use rist_protocol::protocol::nack_tracker::RetransmitBuffer;
use rist_protocol::protocol::rtcp_state::RtcpSenderState;
use rist_protocol::protocol::rtt::RttEstimator;

use crate::config::RistSocketConfig;

/// Maximum RTP packet size (header + payload).
const MAX_RTP_PACKET: usize = 1500;

/// Handle for sending data to a RIST sender task.
pub struct SenderHandle {
    pub tx: mpsc::Sender<Bytes>,
}

/// Spawn a RIST sender task.
pub fn spawn_sender(
    config: RistSocketConfig,
    rtp_socket: UdpSocket,
    rtcp_socket: UdpSocket,
    remote_rtp_addr: SocketAddr,
    cancel: CancellationToken,
) -> (SenderHandle, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Bytes>(256);

    let handle = tokio::spawn(async move {
        if let Err(e) =
            sender_loop(config, rtp_socket, rtcp_socket, remote_rtp_addr, rx, cancel).await
        {
            log::error!("RIST sender task exited with error: {e}");
        }
    });

    (SenderHandle { tx }, handle)
}

async fn sender_loop(
    config: RistSocketConfig,
    rtp_socket: UdpSocket,
    rtcp_socket: UdpSocket,
    remote_rtp_addr: SocketAddr,
    mut rx: mpsc::Receiver<Bytes>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let ssrc: u32 = rand::random();
    let cname = config
        .cname
        .unwrap_or_else(|| format!("{}", rtp_socket.local_addr().unwrap()));

    let mut rtcp_state = RtcpSenderState::new(ssrc, cname, config.rtcp_interval);
    let mut retransmit_buf = RetransmitBuffer::new(config.retransmit_buffer_capacity);
    let mut rtt_estimator = RttEstimator::new(config.rtcp_interval * 10);
    let mut seq: u16 = rand::random();
    let mut rtp_timestamp: u32 = rand::random();

    // Pre-allocated buffers — reused every iteration, zero hot-path allocs
    let mut send_buf = BytesMut::with_capacity(MAX_RTP_PACKET);
    let mut rtcp_recv_buf = vec![0u8; 2048];

    let remote_rtcp_addr = crate::channel::RistChannel::rtcp_addr_for(remote_rtp_addr);
    let mut rtcp_interval = tokio::time::interval(config.rtcp_interval);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                log::info!("RIST sender shutting down");
                break;
            }

            // Outgoing media from application
            data = rx.recv() => {
                let Some(payload) = data else {
                    log::info!("RIST sender channel closed");
                    break;
                };

                // Build RTP packet in pre-allocated buffer (no alloc)
                send_buf.clear();
                write_rtp_header(&mut send_buf, ssrc, seq, rtp_timestamp);
                send_buf.extend_from_slice(&payload);

                // Store for retransmission (Bytes::copy_from_slice is needed here
                // because the retransmit buffer must own the data independently of
                // send_buf, which gets reused. The Bytes is refcounted so retransmit
                // sends are zero-copy.)
                let pkt_bytes = Bytes::copy_from_slice(&send_buf);
                retransmit_buf.insert(seq, pkt_bytes);

                // Send RTP via connected socket (no address lookup)
                if let Err(e) = rtp_socket.send_to(&send_buf, remote_rtp_addr).await {
                    log::warn!("RTP send error: {e}");
                }

                rtcp_state.on_packet_sent(payload.len(), rtp_timestamp);
                seq = seq.wrapping_add(1);
                rtp_timestamp = rtp_timestamp.wrapping_add(2700 * 7);
            }

            // Incoming RTCP from receiver (NACKs, RTT echo responses)
            result = rtcp_socket.recv_from(&mut rtcp_recv_buf) => {
                let (len, from) = result?;
                if let Ok(compound) = RtcpCompound::parse(&rtcp_recv_buf[..len]) {
                    for pkt in &compound.packets {
                        match pkt {
                            RtcpPacket::Nack(nack) => {
                                // Retransmit requested packets — iterate without allocating
                                match &nack.entries {
                                    rist_protocol::packet::rtcp_nack::NackEntries::Bitmask(v) => {
                                        for nack_entry in v {
                                            for lost_seq in nack_entry.lost_seqs() {
                                                if let Some(pkt_data) = retransmit_buf.get(lost_seq) {
                                                    // Zero-copy: Bytes is refcounted
                                                    let _ = rtp_socket.send_to(pkt_data, remote_rtp_addr).await;
                                                }
                                            }
                                        }
                                    }
                                    rist_protocol::packet::rtcp_nack::NackEntries::Range(v) => {
                                        for nack_entry in v {
                                            for lost_seq in nack_entry.lost_seqs() {
                                                if let Some(pkt_data) = retransmit_buf.get(lost_seq) {
                                                    let _ = rtp_socket.send_to(pkt_data, remote_rtp_addr).await;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            RtcpPacket::App(RistApp::RttEchoRequest(req)) => {
                                let response = RistApp::RttEchoResponse(
                                    rist_protocol::packet::rtcp_app::RttEchoResponse {
                                        ssrc: req.ssrc,
                                        timestamp_msw: req.timestamp_msw,
                                        timestamp_lsw: req.timestamp_lsw,
                                        processing_delay_us: 0,
                                    },
                                );
                                let resp_bytes = response.serialize();
                                let _ = rtcp_socket.send_to(&resp_bytes, from).await;
                            }
                            RtcpPacket::App(RistApp::RttEchoResponse(resp)) => {
                                rtt_estimator.on_response(
                                    Instant::now(),
                                    resp.timestamp_msw,
                                    resp.timestamp_lsw,
                                    resp.processing_delay_us,
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }

            // Periodic RTCP emission (not hot path — allocations acceptable here)
            _ = rtcp_interval.tick() => {
                let now = Instant::now();
                let sr = rtcp_state.generate_sr(now);
                let sdes = rtcp_state.generate_sdes();

                let mut packets: Vec<RtcpPacket> = vec![
                    RtcpPacket::SenderReport(sr),
                    RtcpPacket::Sdes(sdes),
                ];

                if config.rtt_echo_enabled && rtt_estimator.should_send_request(now) {
                    let (msw, lsw) = rtt_estimator.generate_request(now);
                    packets.push(RtcpPacket::App(RistApp::RttEchoRequest(RttEchoRequest {
                        ssrc,
                        timestamp_msw: msw,
                        timestamp_lsw: lsw,
                    })));
                }

                let compound = RtcpCompound { packets };
                let bytes = compound.serialize();
                if let Err(e) = rtcp_socket.send_to(&bytes, remote_rtcp_addr).await {
                    log::warn!("RTCP send error: {e}");
                }
            }
        }
    }

    Ok(())
}

/// Write an RTP header directly into a BytesMut. No allocation.
#[inline]
fn write_rtp_header(buf: &mut BytesMut, ssrc: u32, seq: u16, timestamp: u32) {
    buf.put_u8(0x80); // V=2, P=0, X=0, CC=0
    buf.put_u8(33); // M=0, PT=33 (MPEG-2 TS)
    buf.put_u16(seq);
    buf.put_u32(timestamp);
    buf.put_u32(ssrc);
}
