//! RIST receiver task.
//!
//! Two-thread design (mirrors libsrt's recv + TSBPD split):
//!
//! - **Recv task** (tokio `select!`): reads RTP + RTCP off the sockets,
//!   runs the RTCP receiver state / NACK scheduler / RTT estimator,
//!   inserts each media packet into the shared reorder buffer, and wakes
//!   the delivery thread. Emits NACKs + RR/SDES on their timers.
//! - **Delivery thread** (dedicated `std::thread`): owns nothing but the
//!   release timing. It blocks on a condvar timed-wait until the head
//!   packet's exact `arrival + buffer_size` deadline, then drains ready
//!   packets in strict RTP sequence order and forwards them to the
//!   application channel. A single-purpose thread blocked precisely on
//!   the next deadline gives SRT-parity egress timing — far tighter than
//!   a tokio timer-wheel tick contending with recv work on one task.
//!
//! Delivery semantics: packets are held for `buffer_size` so NACK-driven
//! retransmits have a chance to fill gaps before downstream sees them,
//! then released in strict RTP sequence order. Gaps that age past the
//! hold budget are dropped and counted as lost. Each delivered packet
//! carries its true UDP-arrival `Instant` (captured pre-hold) and wire
//! RTP seq via [`RistDelivered`].

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
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
use rist_protocol::protocol::reorder::{DrainItem, InsertOutcome, ReorderBuffer};
use rist_protocol::protocol::rtcp_state::RtcpReceiverState;
use rist_protocol::protocol::rtt::RttEstimator;

use crate::config::RistSocketConfig;
use crate::stats::RistConnStats;

/// Maximum UDP datagram size we'll receive.
const MAX_UDP_RECV: usize = 2048;
/// Fast pump interval: drives NACK emission (NOT delivery — that is the
/// dedicated thread's job).
const NACK_PUMP_INTERVAL: Duration = Duration::from_millis(10);
/// Lower bound for NACK retry delay so we never spam. RTT-driven delay
/// wins when we have a sample; otherwise we fall back to this.
const MIN_NACK_RETRY_DELAY: Duration = Duration::from_millis(20);

/// A parsed RTP media packet awaiting batch insert into the reorder buffer.
struct ParsedRtp {
    seq: u16,
    rtp_ts: u32,
    payload: Bytes,
    arrival_us: u64,
    payload_len: usize,
    is_retransmit: bool,
}

/// Parse one RTP datagram: validate the header, copy the payload (the recv
/// buffer is reused across a batch), and stamp the true UDP-arrival time
/// (monotonic µs from `epoch`). Returns None on a malformed/short datagram.
fn parse_rtp(buf: &[u8], epoch: Instant) -> Option<ParsedRtp> {
    let (header, header_size) = RtpHeader::parse(buf).ok()?;
    Some(ParsedRtp {
        seq: header.sequence_number,
        rtp_ts: header.timestamp,
        payload: Bytes::copy_from_slice(&buf[header_size..]),
        arrival_us: Instant::now().duration_since(epoch).as_micros() as u64,
        payload_len: buf.len() - header_size,
        is_retransmit: header.is_retransmit(),
    })
}

/// A packet delivered to the application by the receiver, in RTP seq order.
#[derive(Debug, Clone)]
pub struct RistDelivered {
    /// RTP payload (header stripped) — the MPEG-TS bytes.
    pub data: Bytes,
    /// Instant the packet was first read off the UDP socket — its TRUE
    /// arrival, captured BEFORE the reorder/jitter hold. The consumer
    /// recovers the genuine arrival cadence (what the source-PCR PLL must
    /// see to lock) via `Instant::now() - arrival`, instead of the
    /// post-hold delivery time which carries the full hold + drain jitter.
    pub arrival: Instant,
    /// Wire RTP sequence number — shared across 2022-7 redundant legs, so
    /// the transport consumer can run a real-seq hitless merger.
    pub rtp_seq: u16,
}

/// Handle for receiving data from a RIST receiver task.
pub struct ReceiverHandle {
    pub rx: mpsc::Receiver<RistDelivered>,
}

/// Shared reorder buffer + its wake signal, between the recv task (which
/// inserts) and the dedicated delivery thread (which drains). The mutex
/// critical sections are tiny — a single O(1) `insert` ring write, or a
/// `drain_ready` loop that pops only already-expired head slots — and the
/// actual `try_send` happens outside the lock, so neither side stalls the
/// other. This is the one place the crate's "tasks own state" rule is
/// relaxed; it matches libsrt's recv-thread/TSBPD-thread mutex exactly.
struct DrainShared {
    reorder: Mutex<ReorderBuffer>,
    signal: Condvar,
}

/// Spawn a RIST receiver task.
pub fn spawn_receiver(
    config: RistSocketConfig,
    rtp_socket: UdpSocket,
    rtcp_socket: UdpSocket,
    cancel: CancellationToken,
    stats: Arc<RistConnStats>,
) -> (ReceiverHandle, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<RistDelivered>(1024);

    let handle = tokio::spawn(async move {
        if let Err(e) = receiver_loop(config, rtp_socket, rtcp_socket, tx, cancel, stats).await {
            log::error!("RIST receiver task exited with error: {e}");
        }
    });

    (ReceiverHandle { rx }, handle)
}

/// Dedicated delivery thread. Blocks on a condvar timed-wait until the
/// head packet's `arrival + buffer_size` deadline (or a new insert wakes
/// it earlier), drains everything ready in seq order, and forwards it.
/// Exits when `cancel` fires (the recv task notifies on shutdown).
fn run_drain_thread(
    shared: Arc<DrainShared>,
    tx: mpsc::Sender<RistDelivered>,
    stats: Arc<RistConnStats>,
    cancel: CancellationToken,
) {
    let mut scratch: Vec<DrainItem> = Vec::with_capacity(64);
    loop {
        let mut guard = shared
            .reorder
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if cancel.is_cancelled() {
            return;
        }
        let now = Instant::now();
        scratch.clear();
        guard.drain_ready(now, &mut scratch);
        if scratch.is_empty() {
            // Nothing ready — wait until the head's deadline or a new
            // insert. The guard is passed into wait/wait_timeout, which
            // atomically releases it, so an insert+notify between here and
            // the wait cannot be lost.
            match guard.next_drain_time() {
                None => {
                    // Empty buffer: park until the recv task notifies. Drop
                    // the re-acquired guard immediately so the next loop
                    // iteration can re-lock and drain.
                    drop(shared.signal.wait(guard));
                }
                Some(deadline) => {
                    let now = Instant::now();
                    if deadline > now {
                        drop(shared.signal.wait_timeout(guard, deadline - now));
                    }
                    // deadline already passed → loop and drain_ready clears it.
                }
            }
            continue;
        }
        drop(guard);
        // Deliver outside the lock so an insert never blocks on the channel.
        for item in scratch.drain(..) {
            match item {
                DrainItem::Delivered { data, seq, arrival } => {
                    if tx
                        .try_send(RistDelivered {
                            data,
                            arrival,
                            rtp_seq: seq,
                        })
                        .is_err()
                    {
                        // Consumer backed up — drop rather than stall the
                        // delivery thread. Bumping `reorder_drops` keeps the
                        // lost-vs-backpressure signals distinguishable.
                        stats.reorder_drops.fetch_add(1, Ordering::Relaxed);
                    }
                }
                DrainItem::Lost => {
                    stats.packets_lost.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

async fn receiver_loop(
    config: RistSocketConfig,
    rtp_socket: UdpSocket,
    rtcp_socket: UdpSocket,
    tx: mpsc::Sender<RistDelivered>,
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
    let mut nack_scheduler = NackScheduler::new(config.max_nack_retries, MIN_NACK_RETRY_DELAY);
    let mut rtt_estimator = RttEstimator::new(config.rtcp_interval * 10);

    // Shared reorder buffer + the dedicated delivery thread.
    let shared = Arc::new(DrainShared {
        reorder: Mutex::new(ReorderBuffer::new(config.buffer_size)),
        signal: Condvar::new(),
    });
    let drain_handle = {
        let shared = shared.clone();
        let stats = stats.clone();
        let cancel = cancel.clone();
        std::thread::Builder::new()
            .name("rist-drain".into())
            .spawn(move || run_drain_thread(shared, tx, stats, cancel))
            .expect("spawn rist-drain thread")
    };

    // Pre-allocated receive buffers
    let mut rtp_buf = vec![0u8; MAX_UDP_RECV];
    let mut rtcp_recv_buf = vec![0u8; MAX_UDP_RECV];

    let mut sender_rtcp_addr: Option<SocketAddr> = None;
    let mut rtcp_interval = tokio::time::interval(config.rtcp_interval);
    // Fast pump for NACK emission (delivery is the drain thread's job now).
    let mut pump_interval = tokio::time::interval(NACK_PUMP_INTERVAL);

    // Stable monotonic epoch for true interarrival jitter (RFC 3550): the
    // RTCP receiver state needs a real arrival timestamp, not the ~0 that
    // `now.elapsed()` (right after `Instant::now()`) used to produce.
    let epoch = Instant::now();

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

            // Incoming RTP media. Drain a bounded burst (this datagram plus
            // any already-queued ones via try_recv_from) and insert the whole
            // batch under ONE buffer lock + ONE delivery-thread wake, instead
            // of locking/notifying per packet — cuts recv<->delivery-thread
            // lock contention at high packet rates. MAX_BATCH bounds it so the
            // RTCP / NACK select arms aren't starved.
            result = rtp_socket.recv_from(&mut rtp_buf) => {
                match result {
                    Ok((len, from)) => {
                        if sender_rtcp_addr.is_none() {
                            sender_rtcp_addr = Some(crate::channel::RistChannel::rtcp_addr_for(from));
                            log::info!("RIST receiver: sender detected at {from}");
                        }

                        const MAX_BATCH: usize = 32;
                        let mut batch: Vec<ParsedRtp> = Vec::with_capacity(MAX_BATCH);
                        if let Some(p) = parse_rtp(&rtp_buf[..len], epoch) {
                            batch.push(p);
                        } else {
                            log::debug!("RTP parse error, len={len}");
                        }
                        while batch.len() < MAX_BATCH {
                            match rtp_socket.try_recv_from(&mut rtp_buf) {
                                Ok((l, _)) => {
                                    if let Some(p) = parse_rtp(&rtp_buf[..l], epoch) {
                                        batch.push(p);
                                    }
                                }
                                Err(_) => break, // WouldBlock (nothing more ready) or error
                            }
                        }

                        if !batch.is_empty() {
                            let now = Instant::now();
                            // One lock for the whole burst; `now` is the true
                            // UDP-arrival anchor carried through to delivery.
                            let mut outcomes: Vec<InsertOutcome> =
                                Vec::with_capacity(batch.len());
                            {
                                let mut g = shared.reorder.lock()
                                    .unwrap_or_else(|p| p.into_inner());
                                for p in &batch {
                                    outcomes.push(g.insert(p.seq, p.payload.clone(), now));
                                }
                            }
                            shared.signal.notify_one();

                            for (p, o) in batch.iter().zip(outcomes.iter()) {
                                // is_duplicate keeps duplicate retransmits out
                                // of the RFC3550 received count + jitter EWMA.
                                rtcp_state.on_packet_received(p.seq, p.rtp_ts, p.arrival_us, o.duplicate);
                                nack_scheduler.on_packet_received(p.seq, now);
                                stats.packets_received.fetch_add(1, Ordering::Relaxed);
                                stats.bytes_received.fetch_add(p.payload_len as u64, Ordering::Relaxed);
                                if p.is_retransmit {
                                    stats.retransmits_received.fetch_add(1, Ordering::Relaxed);
                                }
                                if o.duplicate {
                                    stats.duplicates.fetch_add(1, Ordering::Relaxed);
                                }
                                if o.recovered {
                                    stats.packets_recovered.fetch_add(1, Ordering::Relaxed);
                                }
                                if o.stale {
                                    stats.reorder_drops.fetch_add(1, Ordering::Relaxed);
                                }
                                if o.overflow_flushed > 0 {
                                    // TLPKTDROP: oldest buffered packets dropped
                                    // to keep fresh media flowing under a stuck gap.
                                    stats.packets_lost.fetch_add(
                                        o.overflow_flushed as u64, Ordering::Relaxed);
                                }
                            }
                            stats.jitter_us.store(
                                (rtcp_state.jitter * 1_000_000.0 / 90_000.0) as u64,
                                Ordering::Relaxed,
                            );
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
                                                    // Real receive→respond turnaround (was 0).
                                                    processing_delay_us: now.elapsed().as_micros().min(u32::MAX as u128) as u32,
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

            // Fast pump: emit any pending NACKs.
            _ = pump_interval.tick() => {
                let now = Instant::now();

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

    // Wake the delivery thread so it observes cancellation and exits, then
    // join it so no detached thread outlives the socket (shutdown hygiene).
    shared.signal.notify_all();
    let _ = tokio::task::spawn_blocking(move || {
        let _ = drain_handle.join();
    })
    .await;

    Ok(())
}
