//! RIST receiver task.
//!
//! Owns the RTCP receiver state, NACK scheduler, RTT estimator, and the
//! receiver-side reorder / jitter buffer. Runs as a tokio task with a
//! `select!` loop handling:
//! - Incoming RTP media from the sender (stored in the reorder buffer)
//! - Incoming RTCP (SR, RTT echo) from the sender
//! - A fast pump tick that drains the reorder buffer in-order and emits
//!   any pending NACKs
//! - A periodic RTCP tick that emits RR + SDES and optionally an RTT
//!   echo request
//!
//! Delivery semantics: packets are held for `buffer_size` so NACK-driven
//! retransmits have a chance to fill gaps before downstream sees them,
//! then released to the application in strict RTP sequence order. Gaps
//! that age past the hold budget are dropped and counted as lost.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use rist_protocol::packet::rtcp::{RtcpCompound, RtcpPacket};
use rist_protocol::packet::rtcp_app::{RistApp, RttEchoRequest};
use rist_protocol::packet::rtcp_nack::NackListBuilder;
use rist_protocol::packet::rtcp_rr::ReceiverReport;
use rist_protocol::packet::rtp::RtpHeader;
use rist_protocol::protocol::nack_tracker::NackScheduler;
use rist_protocol::protocol::reorder::{DrainItem, ReorderBuffer};
use rist_protocol::protocol::rtcp_state::RtcpReceiverState;
use rist_protocol::protocol::rtt::RttEstimator;

use crate::config::RistSocketConfig;
use crate::stats::RistConnStats;

/// Maximum UDP datagram size we'll receive.
const MAX_UDP_RECV: usize = 2048;
/// Fast pump interval: drives reorder-buffer drain and NACK emission.
/// Short enough to keep NACK-to-wire latency well under the typical
/// buffer_size, long enough to batch coincident losses.
const NACK_PUMP_INTERVAL: Duration = Duration::from_millis(10);
/// Lower bound for NACK retry delay so we never spam. RTT-driven delay
/// wins when we have a sample; otherwise we fall back to this.
const MIN_NACK_RETRY_DELAY: Duration = Duration::from_millis(20);

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
    stats: Arc<RistConnStats>,
) -> (ReceiverHandle, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Bytes>(1024);

    let handle = tokio::spawn(async move {
        if let Err(e) = receiver_loop(config, rtp_socket, rtcp_socket, tx, cancel, stats).await {
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
    stats: Arc<RistConnStats>,
) -> anyhow::Result<()> {
    // Keep SSRC LSB = 0 so RTCP RRs/SDES follow the librist convention where the
    // data-path LSB is reserved as a retransmission flag (see sender.rs for the
    // full explanation and the librist source reference).
    let ssrc: u32 = rand::random::<u32>() & !1u32;
    let cname = config
        .cname
        .clone()
        .unwrap_or_else(|| format!("{}", rtp_socket.local_addr().unwrap()));

    let mut rtcp_state = RtcpReceiverState::new(ssrc, cname, config.rtcp_interval);
    let mut nack_scheduler =
        NackScheduler::new(config.max_nack_retries, MIN_NACK_RETRY_DELAY);
    let mut rtt_estimator = RttEstimator::new(config.rtcp_interval * 10);
    let mut reorder = ReorderBuffer::new(config.buffer_size);

    // Pre-allocated receive buffers
    let mut rtp_buf = vec![0u8; MAX_UDP_RECV];
    let mut rtcp_recv_buf = vec![0u8; MAX_UDP_RECV];

    let mut sender_rtcp_addr: Option<SocketAddr> = None;
    let mut rtcp_interval = tokio::time::interval(config.rtcp_interval);
    // Fast pump for reorder drain + early NACK emission.
    let mut pump_interval = tokio::time::interval(NACK_PUMP_INTERVAL);
    // Scratch buffer re-used across drain calls to avoid per-tick allocation.
    let mut drained: Vec<DrainItem> = Vec::with_capacity(32);

    log::info!(
        "RIST receiver loop started on RTP={} RTCP={} buffer={}ms",
        rtp_socket.local_addr()?,
        rtcp_socket.local_addr()?,
        config.buffer_size.as_millis(),
    );

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                log::info!("RIST receiver shutting down");
                break;
            }

            // Incoming RTP media
            result = rtp_socket.recv_from(&mut rtp_buf) => {
                match result {
                    Ok((len, from)) => {
                        let now = Instant::now();
                        let arrival_us = now.elapsed().as_micros() as u64;

                        // Learn sender RTCP address from first packet
                        if sender_rtcp_addr.is_none() {
                            sender_rtcp_addr = Some(crate::channel::RistChannel::rtcp_addr_for(from));
                            log::info!("RIST receiver: sender detected at {from}");
                        }

                        // Parse RTP header
                        match RtpHeader::parse(&rtp_buf[..len]) {
                            Ok((header, header_size)) => {
                                let seq = header.sequence_number;
                                let rtp_ts = header.timestamp;
                                let payload_len = len - header_size;
                                let is_retransmit = header.is_retransmit();

                                // Update RTCP receiver state (RR stats + jitter)
                                rtcp_state.on_packet_received(seq, rtp_ts, arrival_us);

                                // Detect gaps for NACK scheduling — also handles
                                // recovery (out-of-order arrivals deactivate pending
                                // NACKs for that seq).
                                nack_scheduler.on_packet_received(seq, now);

                                // Store in reorder buffer, stripping the RTP header.
                                let payload = Bytes::copy_from_slice(&rtp_buf[header_size..len]);
                                let outcome = reorder.insert(seq, payload, now);

                                stats.packets_received.fetch_add(1, Ordering::Relaxed);
                                stats.bytes_received.fetch_add(payload_len as u64, Ordering::Relaxed);
                                if is_retransmit {
                                    stats.retransmits_received.fetch_add(1, Ordering::Relaxed);
                                }
                                if outcome.duplicate {
                                    stats.duplicates.fetch_add(1, Ordering::Relaxed);
                                }
                                if outcome.recovered {
                                    stats.packets_recovered.fetch_add(1, Ordering::Relaxed);
                                }
                                if outcome.stale {
                                    stats.reorder_drops.fetch_add(1, Ordering::Relaxed);
                                }
                                stats.jitter_us.store(
                                    (rtcp_state.jitter * 1_000_000.0 / 90_000.0) as u64,
                                    Ordering::Relaxed,
                                );

                                drain_reorder(&mut reorder, &tx, &stats, now, &mut drained).await;
                            }
                            Err(e) => {
                                log::debug!("RTP parse error: {e}, len={len}");
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("RTP recv error: {e}");
                    }
                }
            }

            // Incoming RTCP from sender (SR, RTT echo)
            result = rtcp_socket.recv_from(&mut rtcp_recv_buf) => {
                match result {
                    Ok((len, from)) => {
                        let now = Instant::now();

                        if sender_rtcp_addr.is_none() {
                            sender_rtcp_addr = Some(from);
                        }

                        match RtcpCompound::parse(&rtcp_recv_buf[..len]) {
                            Ok(compound) => {
                                for pkt in &compound.packets {
                                    match pkt {
                                        RtcpPacket::SenderReport(sr) => {
                                            rtcp_state.on_sr_received(sr, now);
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
                                            // RFC 3550 Section 6.1: compound RTCP must start with SR or RR
                                            let compound = RtcpCompound {
                                                packets: vec![
                                                    RtcpPacket::ReceiverReport(ReceiverReport::empty(ssrc)),
                                                    RtcpPacket::App(response),
                                                ],
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
                                            if let Some(rtt) = rtt_estimator.srtt() {
                                                stats.rtt_us.store(
                                                    rtt.as_micros() as u64,
                                                    Ordering::Relaxed,
                                                );
                                                // Tighten NACK retry delay to match
                                                // measured RTT, never below floor.
                                                let retry = (rtt / 2).max(MIN_NACK_RETRY_DELAY);
                                                nack_scheduler.update_base_delay(retry);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            Err(e) => {
                                log::debug!("RTCP parse error: {e}, len={len}");
                            }
                        }
                    }
                    Err(e) => {
                        log::warn!("RTCP recv error: {e}");
                    }
                }
            }

            // Fast pump: drain reorder buffer + emit any pending NACKs.
            _ = pump_interval.tick() => {
                let now = Instant::now();
                drain_reorder(&mut reorder, &tx, &stats, now, &mut drained).await;

                if let Some(rtcp_dest) = sender_rtcp_addr {
                    let rtt = rtt_estimator.srtt();
                    let pending_nacks = nack_scheduler.get_pending_nacks(now, rtt);
                    if !pending_nacks.is_empty() {
                        let sender_ssrc = rtcp_state.sender_ssrc.unwrap_or(0);
                        let builder = NackListBuilder::new(ssrc, sender_ssrc);
                        let nack_pkt = builder.build_bitmask(&pending_nacks);
                        let rr = rtcp_state.generate_rr(now);
                        let compound = RtcpCompound {
                            packets: vec![
                                RtcpPacket::ReceiverReport(rr),
                                RtcpPacket::Nack(nack_pkt),
                            ],
                        };
                        let bytes = compound.serialize();
                        if let Err(e) = rtcp_socket.send_to(&bytes, rtcp_dest).await {
                            log::warn!("RTCP NACK send error: {e}");
                        } else {
                            stats.nacks_sent.fetch_add(
                                pending_nacks.len() as u64,
                                Ordering::Relaxed,
                            );
                        }
                    }
                }
            }

            // Periodic RR + SDES emission (and scheduled RTT echo)
            _ = rtcp_interval.tick() => {
                let now = Instant::now();
                drain_reorder(&mut reorder, &tx, &stats, now, &mut drained).await;

                if let Some(rtcp_dest) = sender_rtcp_addr {
                    let rr = rtcp_state.generate_rr(now);
                    let sdes = rtcp_state.generate_sdes();

                    let mut packets: Vec<RtcpPacket> = vec![
                        RtcpPacket::ReceiverReport(rr),
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
                    if let Err(e) = rtcp_socket.send_to(&bytes, rtcp_dest).await {
                        log::warn!("RTCP send error: {e}");
                    }
                }
            }
        }
    }

    Ok(())
}

/// Drain packets whose hold time has elapsed, forwarding to the app channel
/// in strict sequence order. Gaps that timed out without a retransmit are
/// counted as lost so downstream stats stay accurate.
async fn drain_reorder(
    reorder: &mut ReorderBuffer,
    tx: &mpsc::Sender<Bytes>,
    stats: &Arc<RistConnStats>,
    now: Instant,
    scratch: &mut Vec<DrainItem>,
) {
    scratch.clear();
    reorder.drain_ready(now, scratch);
    for item in scratch.drain(..) {
        match item {
            DrainItem::Delivered(payload) => {
                if tx.try_send(payload).is_err() {
                    // Application consumer is backed up — drop rather than
                    // stall the reorder pump. Bumping `reorder_drops` keeps
                    // the lost vs. sender/receiver backpressure signals
                    // distinguishable in stats.
                    stats.reorder_drops.fetch_add(1, Ordering::Relaxed);
                }
            }
            DrainItem::Lost => {
                stats.packets_lost.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}
