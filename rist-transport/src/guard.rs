//! Admission guards for an unauthenticated RIST Simple Profile session.
//!
//! RIST Simple Profile (TR-06-1:2020) has **no authentication** — there is no
//! handshake, no shared secret and no per-packet MAC. Anything arriving on the
//! RTP or RTCP port is, at the protocol level, indistinguishable from the peer.
//! A complete fix does not exist inside the profile; what these predicates buy
//! is a *hold-down*, borrowed from the shape already shipped in
//! `bilbycast-relay`'s `udp_relay` (`slot_is_protected` /
//! `SLOT_TAKEOVER_GRACE_MS`):
//!
//! * A peer slot that has been **live** — carrying media or valid RTCP — within
//!   the grace window does not move to a different source **IP**.
//! * A move within the **same IP** is always allowed, because that is a NAT
//!   rebinding a real peer performs routinely and refusing it would break
//!   working sessions.
//! * A slot that has **never** been live is last-writer-wins, so a session can
//!   still bootstrap against a peer whose address nobody configured.
//!
//! Liveness is an `Option<Instant>`: `None` unambiguously means "never live",
//! with no sentinel value to get wrong (the relay needs a `+1` offset on its
//! millisecond counter for exactly this reason; a monotonic `Instant` carries
//! the distinction in the type).
//!
//! **Residual risk, stated plainly.** An attacker who can reach the port and
//! wins the race *before* the real peer's first packet owns the slot for the
//! grace window; so does one who arrives during a genuine ≥12 s outage. Neither
//! is closed by anything short of authentication — i.e. RIST Main Profile with
//! DTLS or PSK, which this crate stubs but does not yet implement. What the
//! hold-down does close is the far likelier case: a *live* session being taken
//! over mid-flight by a single spoofed datagram.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// How long a peer slot stays pinned to its current source IP after the last
/// packet accepted from it. Matches `bilbycast-relay`'s `SLOT_TAKEOVER_GRACE_MS`
/// so an operator reasoning about one transport's takeover window is reasoning
/// about both. Long enough to span an RTCP gap (the interval is ≤100 ms per
/// TR-06-1) plus a deep reorder buffer; short enough that a genuine sender
/// restart from a new address recovers without operator action.
pub const PEER_TAKEOVER_GRACE: Duration = Duration::from_secs(12);

/// Is a peer slot protected from moving to a different source IP?
///
/// `None` means the slot has never been live and is deliberately unprotected —
/// that is the bootstrap path.
#[inline]
pub fn peer_is_protected(last_live: Option<Instant>, now: Instant) -> bool {
    match last_live {
        None => false,
        Some(t) => now.saturating_duration_since(t) < PEER_TAKEOVER_GRACE,
    }
}

/// May a datagram from `from` be accepted for a slot currently latched to
/// `current`, and latch the slot to `from` if it differs?
///
/// Same address: trivially yes. Same IP, different port: yes — NAT rebinding.
/// Different IP: only while the slot is unprotected.
#[inline]
pub fn peer_move_allowed(
    from: SocketAddr,
    current: SocketAddr,
    last_live: Option<Instant>,
    now: Instant,
) -> bool {
    from.ip() == current.ip() || !peer_is_protected(last_live, now)
}

/// Same decision for a slot that may not be latched yet (`None` = unlatched,
/// so anything is accepted), plus an optional operator-configured source-IP
/// pin that overrides everything.
///
/// The pin comes from `RistSocketConfig::remote_addr`, documented since the
/// crate's first release as the receiver's "optional sender filter" and until
/// now never consulted. Only the **IP** is compared: a sender's source port is
/// its own bound port (ephemeral for librist's `ristsender`), so pinning the
/// port would reject legitimate peers.
#[inline]
pub fn source_allowed(
    from: SocketAddr,
    pinned_ip: Option<std::net::IpAddr>,
    current: Option<SocketAddr>,
    last_live: Option<Instant>,
    now: Instant,
) -> bool {
    if let Some(ip) = pinned_ip
        && from.ip() != ip
    {
        return false;
    }
    match current {
        None => true,
        Some(cur) => peer_move_allowed(from, cur, last_live, now),
    }
}

/// Does a NACK naming `media_ssrc` refer to media *we* sent?
///
/// RFC 4585 gives the PT=205 RTPFB NACK an explicit media-source SSRC field;
/// librist's default PT=204 APP "RIST" Range NACK carries the same thing in its
/// APP SSRC slot (its writer stores the peer's `flow_id` there — the field the
/// `"Peer with id #%u associated with flow #%lu"` log line reports). In Simple
/// Profile that flow id **is** the media RTP SSRC, and librist requires it to be
/// even (`"Flow ID must be an even number!"`) because the LSB is the retransmit
/// flag. This crate's sender already forces `ssrc & !1`, so a librist NACK
/// carries our SSRC exactly; the LSB is masked here anyway so a peer that
/// echoed a retransmit-flagged SSRC still matches.
///
/// **Zero is accepted, and that is a real residual — read this before relying
/// on the check.** Older bilbycast receivers build their NACK with
/// `sender_ssrc.unwrap_or(0)`, so every NACK they emit before seeing a Sender
/// Report names media SSRC 0. Rejecting 0 would silently break ARQ against
/// them, so it is accepted.
///
/// The consequence, stated plainly: a **blind off-path attacker can name 0**
/// and be served. Against a sender that has never received RTCP — a one-way
/// contribution feed, or one whose RTCP return path is firewalled — the source
/// hold-down below has no incumbent to defend, so this SSRC check is the only
/// gate, and 0 walks through it without the attacker ever having observed the
/// stream. An earlier version of this comment claimed the opposite; it was
/// wrong.
///
/// What the check does buy, precisely: a NACK naming a *non-zero* SSRC must
/// name ours, which is 32 bits visible only on the wire. And what bounds the
/// residual is not this function but the per-datagram [`NackWorkBudget`] —
/// one datagram can pull at most one retransmit ring, and that egress is aimed
/// at the operator's own configured `remote_rtp_addr`, never a third party, so
/// this is a bounded self-inflicted cost rather than a reflector.
///
/// The close is on the receiver side and has landed: `receiver.rs` now names
/// the SSRC it observes on inbound RTP headers, available from the first media
/// packet, so a current bilbycast receiver never sends 0. Once no supported
/// peer emits 0, this accept can be dropped and the property above becomes
/// true as written.
pub fn nack_targets_us(media_ssrc: u32, our_ssrc: u32) -> bool {
    media_ssrc == 0 || media_ssrc & !1u32 == our_ssrc & !1u32
}

/// Per-received-datagram budget for the retransmit work one RTCP datagram may
/// force out of the sender.
///
/// **Why this is per datagram and not per sub-packet.** A cap on a single NACK
/// sub-packet is not a bound at all: `RtcpCompound::parse` will read as many
/// sub-packets as fit, a minimal APP Range NACK is 16 bytes, and each one
/// arrives with fresh counters. One 2048-byte datagram of them multiplies
/// straight through a per-sub-packet cap. So the budget is created once per
/// `recv_from` and threaded through every NACK in the compound.
///
/// **Why it cannot bind on honest ARQ.** `retx` is seeded from the retransmit
/// ring's capacity: serving more retransmits than the ring holds is provably
/// duplicate work, because at that point every packet we still have has already
/// gone back out. A peer therefore cannot be under-served by this budget unless
/// it asked for strictly more than our entire buffer in one datagram — and the
/// surplus could not have been answered anyway. `scan` (sequence numbers looked
/// up, hit or miss) is four times that, so requests that miss the ring — the
/// stale edge of a peer's window — cannot starve the ones that hit.
///
/// The clamp keeps both ends sane. The floor of 512 is the previously shipped
/// `MAX_RETX_PER_NACK`, so this is never *tighter* than the bound librist
/// interop was verified against. The ceiling of 8192 caps one datagram at
/// roughly 11 MB of retransmit egress and a few milliseconds of `send_to`
/// syscalls, keeping the sender's `select!` responsive to live media — at the
/// receiver's 10 ms NACK cadence that is still 819 200 retransmits per second,
/// far past any real ARQ session, and well above the 200 entries librist puts
/// in a NACK packet.
#[derive(Debug, Clone, Copy)]
pub struct NackWorkBudget {
    /// Sequence numbers still allowed to be looked up for this datagram.
    pub scan: u32,
    /// Retransmit datagrams still allowed to be emitted for this datagram.
    pub retx: u32,
}

impl NackWorkBudget {
    /// Floor — the previously shipped per-NACK retransmit cap.
    pub const MIN_RETX: u32 = 512;
    /// Ceiling — see the type docs for the egress / syscall arithmetic.
    pub const MAX_RETX: u32 = 8192;

    /// Budget for one received RTCP datagram, sized from the sender's
    /// retransmit ring capacity.
    #[inline]
    pub fn for_capacity(retransmit_capacity: usize) -> Self {
        let retx = (retransmit_capacity.min(u32::MAX as usize) as u32)
            .clamp(Self::MIN_RETX, Self::MAX_RETX);
        Self {
            scan: retx.saturating_mul(4),
            retx,
        }
    }

    /// Charge one sequence-number lookup. `false` means the datagram's scan
    /// budget is spent and the caller must stop walking its entries.
    #[inline]
    pub fn charge_scan(&mut self) -> bool {
        if self.scan == 0 {
            return false;
        }
        self.scan -= 1;
        true
    }

    /// Charge one retransmit about to go on the wire. `false` means the
    /// datagram's retransmit budget is spent.
    #[inline]
    pub fn charge_retx(&mut self) -> bool {
        if self.retx == 0 {
            return false;
        }
        self.retx -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn unlatched_slot_accepts_anyone() {
        let now = Instant::now();
        assert!(source_allowed(addr("203.0.113.9:5000"), None, None, None, now));
    }

    #[test]
    fn live_slot_does_not_move_to_a_different_ip() {
        let now = Instant::now();
        let peer = addr("198.51.100.4:5000");
        let live = Some(now - Duration::from_secs(1));
        assert!(!source_allowed(
            addr("203.0.113.9:5000"),
            None,
            Some(peer),
            live,
            now
        ));
        // ...but the peer itself keeps flowing.
        assert!(source_allowed(addr("198.51.100.4:5000"), None, Some(peer), live, now));
    }

    #[test]
    fn same_ip_port_change_is_always_allowed() {
        let now = Instant::now();
        let peer = addr("198.51.100.4:5000");
        let live = Some(now);
        assert!(source_allowed(
            addr("198.51.100.4:41234"),
            None,
            Some(peer),
            live,
            now
        ));
    }

    #[test]
    fn slot_releases_after_the_grace_window() {
        let now = Instant::now();
        let peer = addr("198.51.100.4:5000");
        let stale = Some(now - PEER_TAKEOVER_GRACE - Duration::from_millis(1));
        assert!(!peer_is_protected(stale, now));
        assert!(source_allowed(
            addr("203.0.113.9:5000"),
            None,
            Some(peer),
            stale,
            now
        ));
    }

    #[test]
    fn a_slot_that_was_never_live_is_unprotected() {
        assert!(!peer_is_protected(None, Instant::now()));
    }

    #[test]
    fn configured_pin_rejects_a_foreign_ip_even_when_unlatched() {
        let now = Instant::now();
        let pin = Some(addr("198.51.100.4:5000").ip());
        assert!(!source_allowed(addr("203.0.113.9:5000"), pin, None, None, now));
        assert!(source_allowed(addr("198.51.100.4:41234"), pin, None, None, now));
    }

    /// Zero is what this crate's own receiver emits until it has seen a Sender
    /// Report, so it must be accepted or bilbycast↔bilbycast ARQ stalls at
    /// session start.
    #[test]
    fn nack_ssrc_zero_is_accepted() {
        assert!(nack_targets_us(0, 0xDEAD_BEEE));
    }

    #[test]
    fn nack_ssrc_matches_ignoring_the_retransmit_flag() {
        let ours = 0xDEAD_BEEEu32; // sender SSRCs always have LSB = 0
        assert!(nack_targets_us(ours, ours));
        assert!(nack_targets_us(ours | 1, ours));
        assert!(!nack_targets_us(ours ^ 0x0100_0000, ours));
    }

    #[test]
    fn budget_is_never_tighter_than_the_shipped_per_nack_cap() {
        assert_eq!(NackWorkBudget::for_capacity(64).retx, NackWorkBudget::MIN_RETX);
        assert_eq!(NackWorkBudget::for_capacity(2048).retx, 2048);
        assert_eq!(
            NackWorkBudget::for_capacity(65536).retx,
            NackWorkBudget::MAX_RETX
        );
    }

    #[test]
    fn budget_drains_and_then_refuses() {
        let mut b = NackWorkBudget::for_capacity(512);
        for _ in 0..512 {
            assert!(b.charge_retx());
        }
        assert!(!b.charge_retx());
        // Scan headroom is 4x so misses cannot starve hits.
        assert_eq!(b.scan, 2048);
    }
}
