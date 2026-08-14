//! Admission + amplification bounds on the two unauthenticated RIST sockets.
//!
//! RIST Simple Profile has no handshake and no per-packet authentication, so
//! everything here is about what an *unauthenticated* datagram is allowed to
//! cost us:
//!
//! * A librist-format NACK (PT=204 APP "RIST") is a request to put up to
//!   65 536 sequence numbers per entry back on the wire, and a 2048-byte
//!   datagram holds hundreds of entries across dozens of sub-packets. The work
//!   one received datagram can force must be bounded once, per datagram.
//! * A NACK naming someone else's media SSRC is not ours to answer.
//! * A live session must not be redirected — or injected into — by a datagram
//!   from a different source IP.
//!
//! Every case here drives real sockets through the public `RistSocket` API, so
//! the bounds are exercised where they actually run rather than in isolation.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use tokio::net::UdpSocket;

use rist_transport::stats::RistConnStats;
use rist_transport::{RistSocket, RistSocketConfig};

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
/// A second loopback address — 127.0.0.0/8 is routed to `lo` wholesale, so
/// this is a genuinely different source IP without any interface setup.
const OTHER_HOST: IpAddr = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2));

/// Claim a free even port plus its odd RTCP partner.
fn free_even_port_pair() -> u16 {
    for _ in 0..200 {
        let probe = std::net::UdpSocket::bind((LOCALHOST, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let even = port & !1;
        if even < 1024 {
            continue;
        }
        let a = std::net::UdpSocket::bind((LOCALHOST, even));
        let b = std::net::UdpSocket::bind((LOCALHOST, even + 1));
        if a.is_ok() && b.is_ok() {
            return even;
        }
    }
    panic!("no free even RIST port pair");
}

/// A librist-format Range NACK: PT=204 APP, subtype 0, name "RIST".
fn app_range_nack(media_ssrc: u32, entries: &[(u16, u16)]) -> Vec<u8> {
    let total = 12 + 4 * entries.len();
    let mut buf = Vec::with_capacity(total);
    buf.push(0x80); // V=2, subtype 0 (Range NACK)
    buf.push(204); // PT = APP
    buf.extend_from_slice(&(((total / 4) - 1) as u16).to_be_bytes());
    buf.extend_from_slice(&media_ssrc.to_be_bytes());
    buf.extend_from_slice(b"RIST");
    for (start, extra) in entries {
        buf.extend_from_slice(&start.to_be_bytes());
        buf.extend_from_slice(&extra.to_be_bytes());
    }
    buf
}

fn rtp_packet(ssrc: u32, seq: u16, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12 + payload.len());
    buf.push(0x80); // V=2
    buf.push(33); // PT=33 (MPEG-2 TS)
    buf.extend_from_slice(&seq.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes()); // timestamp
    buf.extend_from_slice(&ssrc.to_be_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// A sender pumping media at a stand-in receiver, plus that receiver's two
/// sockets and the sender's observed SSRC / first sequence number.
struct SenderFixture {
    _socket: RistSocket,
    stats: Arc<RistConnStats>,
    /// The stand-in receiver's RTCP socket — the sender's legitimate peer.
    peer_rtcp: UdpSocket,
    sender_rtcp_addr: SocketAddr,
    ssrc: u32,
    first_seq: u16,
}

/// Bring up a sender, push `count` packets into it, and observe the SSRC and
/// first sequence number it actually put on the wire. The retransmit ring is
/// left full, which is what makes a sweeping NACK expensive.
async fn sender_fixture(count: usize) -> SenderFixture {
    let sender_port = free_even_port_pair();
    let peer_port = free_even_port_pair();
    let peer_rtp_addr = SocketAddr::new(LOCALHOST, peer_port);

    let peer_rtp = UdpSocket::bind(peer_rtp_addr).await.unwrap();
    let peer_rtcp = UdpSocket::bind(SocketAddr::new(LOCALHOST, peer_port + 1))
        .await
        .unwrap();

    let config = RistSocketConfig {
        local_addr: SocketAddr::new(LOCALHOST, sender_port),
        // Keep the periodic RTCP tick out of the way of the assertions.
        rtcp_interval: Duration::from_secs(30),
        rtt_echo_enabled: false,
        ..Default::default()
    };
    let socket = RistSocket::sender(config, peer_rtp_addr).await.unwrap();
    let stats = socket.stats();

    for i in 0..count {
        socket
            .send(Bytes::from(vec![(i & 0xFF) as u8; 200]))
            .await
            .unwrap();
    }

    // Read the first datagram off the wire for the SSRC + starting seq, then
    // wait for the ring to fill.
    let mut buf = vec![0u8; 2048];
    let (len, _) = tokio::time::timeout(Duration::from_secs(5), peer_rtp.recv_from(&mut buf))
        .await
        .expect("sender emitted media")
        .unwrap();
    assert!(len >= 12);
    let first_seq = u16::from_be_bytes([buf[2], buf[3]]);
    let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
    assert_eq!(ssrc & 1, 0, "sender SSRC must be even (retransmit flag is LSB)");

    settle_until(&stats.packets_sent, count as u64).await;

    SenderFixture {
        _socket: socket,
        stats,
        peer_rtcp,
        sender_rtcp_addr: SocketAddr::new(LOCALHOST, sender_port + 1),
        ssrc,
        first_seq,
    }
}

async fn settle_until(counter: &std::sync::atomic::AtomicU64, target: u64) {
    for _ in 0..200 {
        if counter.load(Ordering::Relaxed) >= target {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "counter stuck at {} (wanted {target})",
        counter.load(Ordering::Relaxed)
    );
}

/// Let the sender chew on whatever it was just handed, then report the
/// retransmit count once it has stopped moving.
async fn quiesced_retransmits(stats: &RistConnStats) -> u64 {
    let mut last = u64::MAX;
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = stats.packets_retransmitted.load(Ordering::Relaxed);
        if now == last {
            return now;
        }
        last = now;
    }
    last
}

/// One legitimate-looking APP Range NACK sweeping the sequence space must not
/// pull the retransmit ring out more than once.
///
/// Four entries of `(start, 0xFFFF)` name 262 144 sequence numbers. Unbounded,
/// each sweep re-sends every packet the ring holds — four full buffers of
/// egress from a single 28-byte datagram, and the real attack shape (509
/// entries across 128 sub-packets) scales that to gigabytes. The per-datagram
/// budget stops it at one ring's worth, which is all the useful work there was.
#[tokio::test]
async fn one_rtcp_datagram_cannot_pull_the_ring_out_repeatedly() {
    let fixture = sender_fixture(3000).await;
    // The claim is semantic and deliberately NOT expressed via
    // `NackWorkBudget` — asserting against the budget helper would be
    // circular, passing for any budget the helper happened to hand back.
    // A datagram may cost at most one full retransmit ring, because at that
    // point every packet the sender still holds has already gone back out.
    let ring = RistSocketConfig::default().retransmit_buffer_capacity as u64;
    assert_eq!(ring, 2048, "fixture assumes the default ring size");

    let nack = app_range_nack(
        fixture.ssrc,
        &[
            (fixture.first_seq, 0xFFFF),
            (fixture.first_seq, 0xFFFF),
            (fixture.first_seq, 0xFFFF),
            (fixture.first_seq, 0xFFFF),
        ],
    );
    fixture
        .peer_rtcp
        .send_to(&nack, fixture.sender_rtcp_addr)
        .await
        .unwrap();

    let retransmitted = quiesced_retransmits(&fixture.stats).await;
    assert!(
        retransmitted > 0,
        "a well-formed NACK from the real peer must still be served"
    );
    assert!(
        retransmitted <= ring,
        "one datagram forced {retransmitted} retransmits; one ring is {ring}"
    );
}

/// The NACK's media SSRC identifies whose packets are missing. Ours is 32
/// random bits only visible on the wire, so honouring a NACK that names a
/// different source hands blind off-path attackers the whole retransmit path.
#[tokio::test]
async fn nack_for_a_foreign_media_ssrc_is_not_served() {
    let fixture = sender_fixture(3000).await;

    let foreign = fixture.ssrc ^ 0x0100_0000;
    let nack = app_range_nack(foreign, &[(fixture.first_seq.wrapping_add(2500), 0x0FFF)]);
    fixture
        .peer_rtcp
        .send_to(&nack, fixture.sender_rtcp_addr)
        .await
        .unwrap();

    assert_eq!(
        quiesced_retransmits(&fixture.stats).await,
        0,
        "a NACK naming another source's SSRC must be ignored"
    );
}

/// A live RTCP peer must not be displaced by a stranger, and the stranger's
/// NACK must not be served — otherwise one spoofed datagram both redirects our
/// control feedback and aims the retransmit egress.
#[tokio::test]
async fn a_live_rtcp_peer_is_not_displaced_by_another_source() {
    let fixture = sender_fixture(3000).await;

    // The real peer speaks first, which makes its slot live. Pick a sequence
    // still resident in the ring (3000 sent, 2048 held) so the NACK is
    // answerable and the "served" baseline is unambiguous.
    let resident = fixture.first_seq.wrapping_add(2500);
    let hello = app_range_nack(fixture.ssrc, &[(resident, 0)]);
    fixture
        .peer_rtcp
        .send_to(&hello, fixture.sender_rtcp_addr)
        .await
        .unwrap();
    let baseline = quiesced_retransmits(&fixture.stats).await;
    assert_eq!(baseline, 1, "the real peer's one-packet NACK is served");

    // Now a stranger on a different source IP sends a sweeping NACK with the
    // correct SSRC — the SSRC check alone would let this through.
    let stranger = UdpSocket::bind(SocketAddr::new(OTHER_HOST, 0)).await.unwrap();
    let sweep = app_range_nack(fixture.ssrc, &[(resident, 0x0FFF)]);
    stranger
        .send_to(&sweep, fixture.sender_rtcp_addr)
        .await
        .unwrap();

    assert_eq!(
        quiesced_retransmits(&fixture.stats).await,
        baseline,
        "a stranger's NACK must not be served while the real peer is live"
    );
}

/// The receiver used to hand its session to whoever spoke first and then never
/// check again — any host could inject RTP into the reorder buffer, moving the
/// sequence window and manufacturing gaps for a live flow.
#[tokio::test]
async fn receiver_media_is_pinned_to_the_established_source() {
    let port = free_even_port_pair();
    let config = RistSocketConfig {
        local_addr: SocketAddr::new(LOCALHOST, port),
        buffer_size: Duration::from_millis(20),
        rtcp_interval: Duration::from_secs(30),
        rtt_echo_enabled: false,
        ..Default::default()
    };
    let socket = RistSocket::receiver(config).await.unwrap();
    let stats = socket.stats();
    let rtp_addr = SocketAddr::new(LOCALHOST, port);

    let real = UdpSocket::bind(SocketAddr::new(LOCALHOST, 0)).await.unwrap();
    for seq in 0u16..8 {
        real.send_to(&rtp_packet(0x1234_5678, seq, &[7u8; 100]), rtp_addr)
            .await
            .unwrap();
    }
    settle_until(&stats.packets_received, 8).await;

    // A second host injects into the same flow.
    let attacker = UdpSocket::bind(SocketAddr::new(OTHER_HOST, 0)).await.unwrap();
    for seq in 30000u16..30016 {
        attacker
            .send_to(&rtp_packet(0x1234_5678, seq, &[9u8; 100]), rtp_addr)
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(400)).await;

    assert_eq!(
        stats.packets_received.load(Ordering::Relaxed),
        8,
        "injected media from a foreign source must never reach the reorder buffer"
    );
}
