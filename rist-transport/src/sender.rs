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
//! - RTP socket uses `send_to()` (unconnected, avoids ICMP errors when remote isn't ready)
//! - Retransmit buffer stores `Bytes` (refcounted, no copy on retransmit)
//! - NACK processing avoids Vec allocation for small NACK lists

use std::net::SocketAddr;
use std::time::{Instant, SystemTime};

use bytes::{Bytes, BytesMut, BufMut};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use rist_protocol::packet::rtcp::{RtcpCompound, RtcpPacket};
use rist_protocol::packet::rtcp_app::{RistApp, RttEchoRequest};
use rist_protocol::packet::rtcp_rr::ReceiverReport;
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

    let mut seq: u16 = rand::random();

    // RTP timestamp epoch: the Instant at sender start, paired with the SystemTime
    // for NTP↔RTP alignment. Both SR and RTP packets derive their timestamps from
    // the same wall-clock → 90kHz conversion, ensuring the SR NTP↔RTP mapping
    // is exact — essential for correct output timing in receivers like librist.
    let ts_epoch = Instant::now();
    let ts_epoch_system = SystemTime::now();

    let mut rtcp_state = RtcpSenderState::new(ssrc, cname, config.rtcp_interval, ts_epoch, ts_epoch_system);
    let mut retransmit_buf = RetransmitBuffer::new(config.retransmit_buffer_capacity);
    let mut rtt_estimator = RttEstimator::new(config.rtcp_interval * 10);

    // Pre-allocated buffers — reused every iteration, zero hot-path allocs
    let mut send_buf = BytesMut::with_capacity(MAX_RTP_PACKET);
    let mut rtcp_recv_buf = vec![0u8; 2048];

    let mut remote_rtcp_addr = crate::channel::RistChannel::rtcp_addr_for(remote_rtp_addr);
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

                // NTP-aligned RTP timestamp at 90 kHz.
                // Computed as: NTP_time(now) × 90000, truncated to 32 bits.
                // This matches what generate_sr() uses for the NTP↔RTP mapping.
                let now = Instant::now();
                let rtp_timestamp = ntp_to_rtp90k(ts_epoch_system, ts_epoch, now);

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

                rtcp_state.on_packet_sent(payload.len(), rtp_timestamp, now);
                seq = seq.wrapping_add(1);
            }

            // Incoming RTCP from receiver (NACKs, RTT echo responses)
            result = rtcp_socket.recv_from(&mut rtcp_recv_buf) => {
                let (len, from) = result?;
                // Learn receiver's actual RTCP address from first incoming packet
                if from != remote_rtcp_addr {
                    log::info!("RIST sender: learned receiver RTCP address {from} (was {remote_rtcp_addr})");
                    remote_rtcp_addr = from;
                }
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

/// Convert wall-clock time to an NTP-aligned 90 kHz RTP timestamp.
///
/// NTP seconds = unix_epoch + 2,208,988,800. RTP timestamp = NTP × 90000.
/// We use Instant arithmetic (monotonic, no drift) from a captured SystemTime epoch.
#[inline]
fn ntp_to_rtp90k(epoch_system: SystemTime, epoch_instant: Instant, now: Instant) -> u32 {
    const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;
    let epoch_unix = epoch_system
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let elapsed = now.duration_since(epoch_instant);
    let ntp_us = (epoch_unix.as_micros() as u64 + NTP_EPOCH_OFFSET * 1_000_000)
        + elapsed.as_micros() as u64;
    // Convert NTP microseconds to 90 kHz ticks, truncate to 32 bits
    (ntp_us * 90 / 1000) as u32
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
