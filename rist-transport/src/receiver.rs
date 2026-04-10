//! RIST receiver task.
//!
//! Owns the RTCP receiver state, NACK scheduler, and RTT estimator.
//! Runs as a tokio task with a `select!` loop handling:
//! - Incoming RTP media from the sender
//! - Incoming RTCP (SR, RTT echo) from the sender
//! - Periodic RTCP RR + SDES + NACK emission

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use rist_protocol::packet::rtcp::{RtcpCompound, RtcpPacket};
use rist_protocol::packet::rtcp_app::{RistApp, RttEchoRequest};
use rist_protocol::packet::rtcp_nack::NackListBuilder;
use rist_protocol::packet::rtp::RtpHeader;
use rist_protocol::protocol::nack_tracker::NackScheduler;
use rist_protocol::protocol::rtcp_state::RtcpReceiverState;
use rist_protocol::protocol::rtt::RttEstimator;

use crate::config::RistSocketConfig;

/// Handle for receiving data from a RIST receiver task.
pub struct ReceiverHandle {
    pub rx: mpsc::Receiver<Bytes>,
}

/// Spawn a RIST receiver task.
pub fn spawn_receiver(
    config: RistSocketConfig,
    rtp_socket: UdpSocket,
    rtcp_socket: UdpSocket,
    cancel: CancellationToken,
) -> (ReceiverHandle, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Bytes>(256);

    let handle = tokio::spawn(async move {
        if let Err(e) = receiver_loop(config, rtp_socket, rtcp_socket, tx, cancel).await {
            log::error!("RIST receiver task exited with error: {e}");
        }
    });

    (ReceiverHandle { rx }, handle)
}

async fn receiver_loop(
    config: RistSocketConfig,
    rtp_socket: UdpSocket,
    rtcp_socket: UdpSocket,
    tx: mpsc::Sender<Bytes>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let ssrc: u32 = rand::random();
    let cname = config
        .cname
        .unwrap_or_else(|| format!("{}", rtp_socket.local_addr().unwrap()));

    let mut rtcp_state = RtcpReceiverState::new(ssrc, cname, config.rtcp_interval);
    let mut nack_scheduler =
        NackScheduler::new(config.max_nack_retries, Duration::from_millis(50));
    let mut rtt_estimator = RttEstimator::new(config.rtcp_interval * 10);

    let mut rtp_buf = vec![0u8; 2048];
    let mut rtcp_buf = vec![0u8; 2048];
    let mut sender_addr: Option<SocketAddr> = None;
    let mut sender_rtcp_addr: Option<SocketAddr> = None;
    let mut rtcp_interval = tokio::time::interval(config.rtcp_interval);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                log::info!("RIST receiver shutting down");
                break;
            }

            // Incoming RTP media
            result = rtp_socket.recv_from(&mut rtp_buf) => {
                let (len, from) = result?;
                let now = Instant::now();
                let arrival_us = now.elapsed().as_micros() as u64; // monotonic

                // Learn sender address from first packet
                if sender_addr.is_none() {
                    sender_addr = Some(from);
                    sender_rtcp_addr = Some(crate::channel::RistChannel::rtcp_addr_for(from));
                    log::info!("RIST receiver: sender detected at {from}");
                }

                // Parse RTP header
                if let Ok((header, header_size)) = RtpHeader::parse(&rtp_buf[..len]) {
                    let seq = header.sequence_number;
                    let rtp_ts = header.timestamp;

                    // Update RTCP receiver state
                    rtcp_state.on_packet_received(seq, rtp_ts, arrival_us);

                    // Detect gaps for NACK scheduling
                    let new_gaps = nack_scheduler.on_packet_received(seq, now);
                    if !new_gaps.is_empty() {
                        log::debug!("RIST receiver: detected gaps: {new_gaps:?}");
                    }

                    // If this was a retransmitted packet, mark as recovered
                    // (the NackScheduler handles this via on_packet_received for out-of-order)

                    // Deliver payload to application
                    let payload = Bytes::copy_from_slice(&rtp_buf[header_size..len]);
                    if tx.try_send(payload).is_err() {
                        log::warn!("RIST receiver: application channel full, dropping packet");
                    }
                }
            }

            // Incoming RTCP from sender (SR, RTT echo)
            result = rtcp_socket.recv_from(&mut rtcp_buf) => {
                let (len, from) = result?;
                let now = Instant::now();

                if sender_rtcp_addr.is_none() {
                    sender_rtcp_addr = Some(from);
                }

                if let Ok(compound) = RtcpCompound::parse(&rtcp_buf[..len]) {
                    for pkt in &compound.packets {
                        match pkt {
                            RtcpPacket::SenderReport(sr) => {
                                rtcp_state.on_sr_received(sr, now);
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
                                let compound = RtcpCompound {
                                    packets: vec![RtcpPacket::App(response)],
                                };
                                let bytes = compound.serialize();
                                let _ = rtcp_socket.send_to(&bytes, from).await;
                            }
                            RtcpPacket::App(RistApp::RttEchoResponse(resp)) => {
                                rtt_estimator.on_response(
                                    now,
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

                if let Some(rtcp_dest) = sender_rtcp_addr {
                    let rr = rtcp_state.generate_rr(now);
                    let sdes = rtcp_state.generate_sdes();

                    let mut packets: Vec<RtcpPacket> = vec![
                        RtcpPacket::ReceiverReport(rr),
                        RtcpPacket::Sdes(sdes),
                    ];

                    // Generate NACKs for lost packets
                    let rtt = rtt_estimator.srtt();
                    let pending_nacks = nack_scheduler.get_pending_nacks(now, rtt);
                    if !pending_nacks.is_empty() {
                        let sender_ssrc = rtcp_state.sender_ssrc.unwrap_or(0);
                        let builder = NackListBuilder::new(ssrc, sender_ssrc);
                        let nack_pkt = builder.build_bitmask(&pending_nacks);
                        packets.push(RtcpPacket::Nack(nack_pkt));
                    }

                    // RTT echo request
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
                    if let Err(e) = rtcp_socket.send_to(&bytes, rtcp_dest).await {
                        log::warn!("RTCP send error: {e}");
                    }
                }
            }
        }
    }

    Ok(())
}
