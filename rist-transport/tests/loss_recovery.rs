//! End-to-end NACK loss-recovery test through real RistSocket sender +
//! receiver over loopback, with an in-process lossy relay between them.
//!
//! Validates that the receiver rewrite (dedicated delivery thread) + the
//! NACK/retransmit path still recover dropped media: with ~5% RTP loss,
//! NACK-driven retransmission must recover the vast majority and deliver
//! in strict sequence order. RTCP (NACKs + SR) is relayed losslessly so
//! the control loop closes; only media is impaired.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

use rist_transport::{RistSocket, RistSocketConfig};

/// Lossy RTP relay + lossless RTCP relay between sender and receiver.
/// `loss_permille` of media datagrams (sender->receiver direction) are
/// dropped; retransmits included (they carry the retx flag but look like
/// media to the relay), so recovery has to actually work.
async fn spawn_relay(
    relay_rtp: SocketAddr,
    relay_rtcp: SocketAddr,
    receiver_rtp: SocketAddr,
    loss_permille: u64,
    dropped: Arc<AtomicU64>,
    cancel: CancellationToken,
) {
    let rtp = UdpSocket::bind(relay_rtp).await.unwrap();
    let rtcp = UdpSocket::bind(relay_rtcp).await.unwrap();
    let receiver_rtcp = SocketAddr::new(receiver_rtp.ip(), receiver_rtp.port() + 1);

    let mut sender_rtp: Option<SocketAddr> = None;
    let mut sender_rtcp: Option<SocketAddr> = None;
    let mut buf = vec![0u8; 2048];
    let mut cbuf = vec![0u8; 2048];
    // Cheap deterministic-ish PRNG (no rand dep needed here): xorshift.
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            r = rtp.recv_from(&mut buf) => {
                if let Ok((len, from)) = r {
                    // First RTP arrival is the sender's RTP addr.
                    if sender_rtp.is_none() {
                        sender_rtp = Some(from);
                        sender_rtcp = Some(SocketAddr::new(from.ip(), from.port() + 1));
                    }
                    // Drop loss_permille/1000 of media (both directions are
                    // media here — sender->receiver only).
                    if next() % 1000 < loss_permille {
                        dropped.fetch_add(1, Ordering::Relaxed);
                    } else {
                        let _ = rtp.send_to(&buf[..len], receiver_rtp).await;
                    }
                }
            }
            r = rtcp.recv_from(&mut cbuf) => {
                if let Ok((len, from)) = r {
                    // Demux RTCP by source: from receiver -> sender, from sender -> receiver.
                    let dst = if Some(from) == sender_rtcp {
                        Some(receiver_rtcp)
                    } else {
                        // From the receiver's RTCP (NACKs/RR) -> sender's RTCP.
                        sender_rtcp
                    };
                    if let Some(dst) = dst {
                        let _ = rtcp.send_to(&cbuf[..len], dst).await;
                    }
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nack_recovers_lossy_media_in_order() {
    // Port plan (loopback): receiver RX=42100/+1, relay R=42110/+1, sender S=42120/+1.
    let receiver_rtp: SocketAddr = "127.0.0.1:42100".parse().unwrap();
    let relay_rtp: SocketAddr = "127.0.0.1:42110".parse().unwrap();
    let relay_rtcp: SocketAddr = "127.0.0.1:42111".parse().unwrap();
    let sender_local: SocketAddr = "127.0.0.1:42120".parse().unwrap();

    let cancel = CancellationToken::new();
    let dropped = Arc::new(AtomicU64::new(0));

    // Relay between sender and receiver.
    {
        let dropped = dropped.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            spawn_relay(relay_rtp, relay_rtcp, receiver_rtp, 50, dropped, cancel).await;
        });
    }

    // Receiver bound at RX; generous hold so retransmits land in time.
    let mut rcfg = RistSocketConfig::default();
    rcfg.local_addr = receiver_rtp;
    rcfg.buffer_size = Duration::from_millis(800);
    let mut receiver = RistSocket::receiver(rcfg).await.unwrap();
    let rstats = receiver.stats();

    // Sender targets the relay's RTP port.
    let mut scfg = RistSocketConfig::default();
    scfg.local_addr = sender_local;
    let sender = RistSocket::sender(scfg, relay_rtp).await.unwrap();

    // Collector task: drain delivered payloads, record the embedded counter.
    let received: Arc<std::sync::Mutex<Vec<u32>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    {
        let received = received.clone();
        tokio::spawn(async move {
            while let Some(b) = receiver.recv().await {
                if b.len() >= 4 {
                    let n = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                    received.lock().unwrap().push(n);
                }
            }
        });
    }

    // Let the control loop establish (sender detected, SR/RR flowing).
    tokio::time::sleep(Duration::from_millis(150)).await;

    const N: u32 = 800;
    for i in 0..N {
        // 188*7-ish payload with the counter in the first 4 bytes.
        let mut payload = vec![0u8; 1316];
        payload[..4].copy_from_slice(&i.to_be_bytes());
        sender.send(Bytes::from(payload)).await.unwrap();
        // ~2 ms spacing → ~1.6 s stream, within retransmit budget.
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    // Drain: stream duration + hold + retransmit slack.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    cancel.cancel();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let got = received.lock().unwrap().clone();
    let dropped_n = dropped.load(Ordering::Relaxed);
    let recovered = rstats.packets_recovered.load(Ordering::Relaxed);

    eprintln!(
        "sent={N} delivered={} dropped_by_relay={dropped_n} packets_recovered={recovered}",
        got.len()
    );

    // The relay must have actually dropped media (else the test proves nothing).
    assert!(dropped_n > 10, "relay should have dropped media, dropped={dropped_n}");
    // NACK recovery must have engaged.
    assert!(recovered > 0, "NACK retransmission must recover dropped packets, recovered={recovered}");
    // Strict in-order delivery (the reorder buffer's contract).
    for w in got.windows(2) {
        assert!(w[0] < w[1], "delivery must be strictly increasing: {} then {}", w[0], w[1]);
    }
    // The vast majority must arrive despite 5% loss + recovery.
    assert!(
        got.len() as u32 >= N * 95 / 100,
        "expected >=95% delivered with recovery, got {}/{N}",
        got.len()
    );
}
