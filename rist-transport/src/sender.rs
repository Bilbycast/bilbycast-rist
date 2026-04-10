//! RIST sender task.
//!
//! Owns the RTP sequence counter, RTCP sender state, and retransmit buffer.
//! Runs as a tokio task with a `select!` loop handling:
//! - Outgoing media from the application
//! - Incoming RTCP (NACKs, RTT echo responses) from the receiver
//! - Periodic RTCP SR + SDES emission

use std::net::SocketAddr;
use std::time::Instant;

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use rist_protocol::packet::rtcp::{RtcpCompound, RtcpPacket};
use rist_protocol::packet::rtcp_app::{RistApp, RttEchoRequest};
use rist_protocol::packet::rtp::RtpHeader;
use rist_protocol::protocol::nack_tracker::RetransmitBuffer;
use rist_protocol::protocol::rtcp_state::RtcpSenderState;
use rist_protocol::protocol::rtt::RttEstimator;

use crate::config::RistSocketConfig;

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
        if let Err(e) = sender_loop(config, rtp_socket, rtcp_socket, remote_rtp_addr, rx, cancel).await {
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
    let mut rtcp_buf = vec![0u8; 2048];

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

                let header = RtpHeader::new_ts(ssrc, seq, rtp_timestamp);
                let header_bytes = header.serialize();

                let mut pkt_buf = Vec::with_capacity(header_bytes.len() + payload.len());
                pkt_buf.extend_from_slice(&header_bytes);
                pkt_buf.extend_from_slice(&payload);

                // Store for retransmission
                retransmit_buf.insert(seq, Bytes::copy_from_slice(&pkt_buf));

                // Send RTP
                if let Err(e) = rtp_socket.send_to(&pkt_buf, remote_rtp_addr).await {
                    log::warn!("RTP send error: {e}");
                }

                rtcp_state.on_packet_sent(payload.len(), rtp_timestamp);
                seq = seq.wrapping_add(1);
                // Advance timestamp by 7 TS packets worth (1316 bytes / 188 = 7 packets * 2700 ticks)
                rtp_timestamp = rtp_timestamp.wrapping_add(2700 * 7);
            }

            // Incoming RTCP from receiver (NACKs, RTT echo responses)
            result = rtcp_socket.recv_from(&mut rtcp_buf) => {
                let (len, _from) = result?;
                if let Ok(compound) = RtcpCompound::parse(&rtcp_buf[..len]) {
                    for pkt in &compound.packets {
                        match pkt {
                            RtcpPacket::Nack(nack) => {
                                // Retransmit requested packets
                                let lost_seqs: Vec<u16> = match &nack.entries {
                                    rist_protocol::packet::rtcp_nack::NackEntries::Bitmask(v) => {
                                        v.iter().flat_map(|n| n.lost_seqs()).collect()
                                    }
                                    rist_protocol::packet::rtcp_nack::NackEntries::Range(v) => {
                                        v.iter().flat_map(|n| n.lost_seqs()).collect()
                                    }
                                };
                                for lost_seq in lost_seqs {
                                    if let Some(pkt_data) = retransmit_buf.get(lost_seq) {
                                        if let Err(e) = rtp_socket.send_to(pkt_data, remote_rtp_addr).await {
                                            log::warn!("Retransmit send error: {e}");
                                        }
                                    }
                                }
                            }
                            RtcpPacket::App(RistApp::RttEchoRequest(req)) => {
                                // Respond to RTT echo request
                                let response = RistApp::RttEchoResponse(
                                    rist_protocol::packet::rtcp_app::RttEchoResponse {
                                        ssrc: req.ssrc,
                                        timestamp_msw: req.timestamp_msw,
                                        timestamp_lsw: req.timestamp_lsw,
                                        processing_delay_us: 0,
                                    },
                                );
                                let resp_bytes = response.serialize();
                                let _ = rtcp_socket.send_to(&resp_bytes, _from).await;
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

            // Periodic RTCP emission
            _ = rtcp_interval.tick() => {
                let now = Instant::now();
                let sr = rtcp_state.generate_sr(now);
                let sdes = rtcp_state.generate_sdes();

                let mut packets: Vec<RtcpPacket> = vec![
                    RtcpPacket::SenderReport(sr),
                    RtcpPacket::Sdes(sdes),
                ];

                // Optionally include RTT echo request
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
