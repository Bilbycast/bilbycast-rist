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
use std::sync::Arc;
use std::sync::atomic::Ordering;
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
use crate::guard::{NackWorkBudget, nack_targets_us, peer_move_allowed};
use crate::stats::RistConnStats;

/// Maximum RTP packet size (header + payload).
const MAX_RTP_PACKET: usize = 1500;

/// Retransmit every sequence number `seqs` yields, charging the datagram-wide
/// work budget as it goes. Returns `true` if the budget ran out, which is the
/// caller's signal to abandon the rest of the datagram — the budget is shared
/// by every sub-packet in the compound, so a spent budget means the datagram is
/// finished, not just this sub-packet.
///
/// The iterator is walked lazily and nothing is collected: a `Range` NACK entry
/// names up to 65 536 sequence numbers, so materialising the expansion first
/// was an allocation sized straight off the wire.
///
/// Each retransmit flips the SSRC LSB to 1 so librist's receiver (which uses
/// `flow_id & 1` as the retry flag — see libRIST `rist-common.c`) counts these
/// as retransmits rather than fresh data. The buffered packet has LSB=0; we
/// copy into the caller's reusable `retx_buf` and flip byte 11 (low byte of the
/// SSRC u32 in big-endian wire order). Zero allocations on the hot path.
#[allow(clippy::too_many_arguments)]
async fn serve_nack_entries<I: Iterator<Item = u16>>(
    seqs: I,
    retransmit_buf: &RetransmitBuffer,
    retx_buf: &mut BytesMut,
    rtp_socket: &UdpSocket,
    remote_rtp_addr: SocketAddr,
    budget: &mut NackWorkBudget,
    requested: &mut u64,
    retransmitted: &mut u64,
) -> bool {
    for lost_seq in seqs {
        if !budget.charge_scan() {
            return true;
        }
        *requested += 1;
        // A miss costs one ring index and nothing else — only the scan
        // budget was charged, so stale requests at the edge of the peer's
        // window cannot starve the ones that hit.
        let Some(pkt_data) = retransmit_buf.get(lost_seq) else {
            continue;
        };
        if !budget.charge_retx() {
            return true;
        }
        retx_buf.clear();
        retx_buf.extend_from_slice(pkt_data);
        if retx_buf.len() > 11 {
            retx_buf[11] |= 0x01;
        }
        if rtp_socket.send_to(retx_buf, remote_rtp_addr).await.is_ok() {
            *retransmitted += 1;
        }
    }
    false
}

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
    stats: Arc<RistConnStats>,
) -> (SenderHandle, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Bytes>(256);

    let handle = tokio::spawn(async move {
        if let Err(e) =
            sender_loop(config, rtp_socket, rtcp_socket, remote_rtp_addr, rx, cancel, stats).await
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
    stats: Arc<RistConnStats>,
) -> anyhow::Result<()> {
    // Keep SSRC LSB = 0. librist treats an odd SSRC on an RTP data packet as a
    // retransmission flag (see librist rist-common.c "if (flow_id & 1UL) retry = 1"),
    // and rejects the first such packet of a flow (`!receiver_queue_has_items && retry`),
    // so an odd random SSRC causes 100% packet loss against librist ristreceiver.
    let ssrc: u32 = rand::random::<u32>() & !1u32;
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
    // Retransmit scratch buffer: we can't mutate the buffered `Bytes` in-place
    // because it may be referenced elsewhere, so copy once per retransmit and
    // flip the SSRC LSB in the copy. Cap is `MAX_RTP_PACKET` so the Vec never
    // re-allocates on resend.
    let mut retx_buf = BytesMut::with_capacity(MAX_RTP_PACKET);
    let mut rtcp_recv_buf = vec![0u8; 2048];

    // The RTCP peer starts at the address derived from the operator-configured
    // RTP destination and is held there once it goes live — see `guard` for the
    // hold-down rules and the residual risk an unauthenticated profile leaves.
    let mut remote_rtcp_addr = crate::channel::RistChannel::rtcp_addr_for(remote_rtp_addr);
    let mut rtcp_peer_live: Option<Instant> = None;
    let mut rtcp_interval = tokio::time::interval(config.rtcp_interval);

    // Sized once: the retransmit ring's capacity is fixed for the session.
    let nack_budget_template = NackWorkBudget::for_capacity(retransmit_buf.capacity());

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
                } else {
                    stats.packets_sent.fetch_add(1, Ordering::Relaxed);
                    stats.bytes_sent.fetch_add(send_buf.len() as u64, Ordering::Relaxed);
                }

                rtcp_state.on_packet_sent(payload.len(), rtp_timestamp, now);
                seq = seq.wrapping_add(1);
            }

            // Incoming RTCP from receiver (NACKs, RTT echo responses)
            result = rtcp_socket.recv_from(&mut rtcp_recv_buf) => {
                let (len, from) = result?;
                let recv_at = Instant::now();
                // Trace: dump the first 64 bytes of every RTCP packet we
                // receive until we know the peer NACK format. Remove once
                // interop is proven. Gated at `trace` so it's silent by
                // default.
                log::trace!(
                    "RIST sender: received {} RTCP bytes: {:02x?}",
                    len,
                    &rtcp_recv_buf[..len.min(256)]
                );
                // The peer slot moves only under the hold-down rules, and only
                // on a datagram that actually decoded as RTCP — adopting on the
                // raw source address (the old behaviour) let one unauthenticated
                // datagram of any content permanently redirect our SR / SDES /
                // RTT-echo feedback away from the real receiver.
                let Ok(compound) = RtcpCompound::parse(&rtcp_recv_buf[..len]) else {
                    log::debug!("RIST sender: undecodable RTCP from {from} ({len} bytes)");
                    continue;
                };
                if compound.packets.is_empty() {
                    continue;
                }
                if !peer_move_allowed(from, remote_rtcp_addr, rtcp_peer_live, recv_at) {
                    // No warn!: this is attacker-reachable at line rate, and a
                    // log line per datagram is its own amplification.
                    log::debug!(
                        "RIST sender: ignoring RTCP from {from}; peer {remote_rtcp_addr} is live"
                    );
                    continue;
                }
                if from != remote_rtcp_addr {
                    if from.ip() == remote_rtcp_addr.ip() {
                        // A NAT port rebind is always allowed and therefore
                        // NOT rate-limited — so it must not log at info, or a
                        // spoofer varying its source port gets a log line per
                        // datagram out of us.
                        log::debug!("RIST sender: receiver RTCP port moved to {from}");
                    } else {
                        // Rate-limited by construction: a different-IP move
                        // needs PEER_TAKEOVER_GRACE of silence from the
                        // incumbent first.
                        log::info!("RIST sender: learned receiver RTCP address {from} (was {remote_rtcp_addr})");
                    }
                    remote_rtcp_addr = from;
                }
                rtcp_peer_live = Some(recv_at);
                // One work budget for the WHOLE datagram, shared by every NACK
                // sub-packet in it. A per-sub-packet cap is not a bound: 128
                // minimal APP Range NACKs fit in this 2048-byte buffer, each
                // with fresh counters.
                let mut budget = nack_budget_template;
                log::trace!(
                    "RIST sender: RTCP compound parsed, {} sub-packets",
                    compound.packets.len()
                );
                for pkt in &compound.packets {
                    log::trace!("RIST sender: RTCP sub-packet: {:?}", pkt);
                    match pkt {
                        RtcpPacket::Nack(nack) => {
                            // RFC 4585 names the media source explicitly; a
                            // NACK for someone else's SSRC is not ours to
                            // answer.
                            if !nack_targets_us(nack.media_ssrc, ssrc) {
                                log::debug!(
                                    "RIST sender: NACK for foreign media SSRC {:#010x}, ignoring",
                                    nack.media_ssrc
                                );
                                continue;
                            }
                            // Iterate lazily: `Range` entries carry a
                            // 16-bit run length, so collecting the expanded
                            // sequence list first was an unbounded
                            // allocation driven straight off the wire
                            // (509 entries x 65 536 seqs = a 66 MB Vec).
                            let mut requested: u64 = 0;
                            let mut retransmitted: u64 = 0;
                            let spent = match &nack.entries {
                                rist_protocol::packet::rtcp_nack::NackEntries::Bitmask(v) => {
                                    serve_nack_entries(
                                        v.iter().flat_map(|e| e.lost_seqs()),
                                        &retransmit_buf, &mut retx_buf, &rtp_socket,
                                        remote_rtp_addr, &mut budget,
                                        &mut requested, &mut retransmitted,
                                    ).await
                                }
                                rist_protocol::packet::rtcp_nack::NackEntries::Range(v) => {
                                    serve_nack_entries(
                                        v.iter().flat_map(|e| e.lost_seqs()),
                                        &retransmit_buf, &mut retx_buf, &rtp_socket,
                                        remote_rtp_addr, &mut budget,
                                        &mut requested, &mut retransmitted,
                                    ).await
                                }
                            };
                            stats.nacks_received.fetch_add(requested, Ordering::Relaxed);
                            stats.packets_retransmitted.fetch_add(retransmitted, Ordering::Relaxed);
                            if spent {
                                // Budget gone: the rest of this datagram is
                                // not serviced. Anything genuinely still
                                // missing is re-NACKed next round.
                                break;
                            }
                        }
                        RtcpPacket::App(RistApp::RttEchoRequest(req)) => {
                            // Report our actual receive→respond turnaround so
                            // the requester can subtract it from the measured
                            // RTT (was hard-coded 0, biasing SRT/NACK-retry high).
                            let response = RistApp::RttEchoResponse(
                                rist_protocol::packet::rtcp_app::RttEchoResponse {
                                    ssrc: req.ssrc,
                                    timestamp_msw: req.timestamp_msw,
                                    timestamp_lsw: req.timestamp_lsw,
                                    processing_delay_us: recv_at.elapsed().as_micros().min(u32::MAX as u128) as u32,
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
                            if let Some(rtt) = rtt_estimator.srtt() {
                                stats.rtt_us.store(
                                    rtt.as_micros() as u64,
                                    Ordering::Relaxed,
                                );
                            }
                        }
                        RtcpPacket::App(RistApp::RangeNack(nack)) => {
                            // librist's default NACK format (PT=204 APP
                            // "RIST" subtype 0). Each entry covers a run
                            // of seqs: `start` and `extra` more after it —
                            // up to 65 536 per entry, which is why this arm
                            // needs the same budget the RTPFB arm gets.
                            // Until now it had none at all.
                            if !nack_targets_us(nack.media_ssrc, ssrc) {
                                log::debug!(
                                    "RIST sender: APP NACK for foreign flow {:#010x}, ignoring",
                                    nack.media_ssrc
                                );
                                continue;
                            }
                            let mut requested: u64 = 0;
                            let mut retransmitted: u64 = 0;
                            let seqs = nack.entries.iter().flat_map(|(start, extra)| {
                                let start = *start;
                                (0..=(*extra as u32)).map(move |i| start.wrapping_add(i as u16))
                            });
                            let spent = serve_nack_entries(
                                seqs,
                                &retransmit_buf, &mut retx_buf, &rtp_socket,
                                remote_rtp_addr, &mut budget,
                                &mut requested, &mut retransmitted,
                            ).await;
                            stats.nacks_received.fetch_add(requested, Ordering::Relaxed);
                            stats.packets_retransmitted.fetch_add(retransmitted, Ordering::Relaxed);
                            if spent {
                                break;
                            }
                        }
                        _ => {}
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
