//! RTCP sender and receiver state machines.
//!
//! Tracks the state needed to generate RTCP SR and RR packets per RFC 3550.

use std::time::{Duration, Instant, SystemTime};

use crate::packet::rtcp_rr::{ReceiverReport, ReportBlock};
use crate::packet::rtcp_sdes::Sdes;
use crate::packet::rtcp_sr::SenderReport;

/// State maintained by a RIST sender for RTCP generation.
#[derive(Debug)]
pub struct RtcpSenderState {
    /// Our SSRC.
    pub ssrc: u32,
    /// CNAME for SDES packets.
    pub cname: String,
    /// Total packets sent.
    pub packet_count: u32,
    /// Total payload bytes sent.
    pub octet_count: u32,
    /// Last RTP timestamp sent (from actual RTP packets).
    pub last_rtp_timestamp: u32,
    /// Wall-clock time when the last RTP timestamp was captured.
    last_rtp_wallclock: Option<Instant>,
    /// RTP clock epoch (Instant) for wall-clock-to-RTP conversion.
    rtp_ts_epoch: Instant,
    /// RTP clock epoch (SystemTime) for NTP-aligned timestamp generation.
    rtp_ts_epoch_system: SystemTime,
    /// Last time an RTCP packet was sent.
    pub last_rtcp_time: Option<Instant>,
    /// RTCP emission interval.
    pub rtcp_interval: Duration,
}

impl RtcpSenderState {
    pub fn new(
        ssrc: u32,
        cname: String,
        rtcp_interval: Duration,
        rtp_ts_epoch: Instant,
        rtp_ts_epoch_system: SystemTime,
    ) -> Self {
        Self {
            ssrc,
            cname,
            packet_count: 0,
            octet_count: 0,
            last_rtp_timestamp: 0,
            last_rtp_wallclock: None,
            rtp_ts_epoch,
            rtp_ts_epoch_system,
            last_rtcp_time: None,
            rtcp_interval,
        }
    }

    /// Record that a packet was sent.
    pub fn on_packet_sent(&mut self, payload_size: usize, rtp_timestamp: u32, now: Instant) {
        self.packet_count += 1;
        self.octet_count += payload_size as u32;
        self.last_rtp_timestamp = rtp_timestamp;
        self.last_rtp_wallclock = Some(now);
    }

    /// Check if it's time to send an RTCP compound packet.
    pub fn should_send_rtcp(&self, now: Instant) -> bool {
        match self.last_rtcp_time {
            None => true,
            Some(last) => now.duration_since(last) >= self.rtcp_interval,
        }
    }

    /// Generate a Sender Report.
    ///
    /// The RTP timestamp in the SR corresponds to the same wall-clock instant
    /// as the NTP timestamp (RFC 3550 Section 6.4.1). Computed from the same
    /// clock base as the RTP data packets.
    pub fn generate_sr(&mut self, now: Instant) -> SenderReport {
        let (ntp_msw, ntp_lsw) = system_time_to_ntp(SystemTime::now());

        // NTP-aligned RTP timestamp: same formula as the sender's data path.
        // NTP_time(now) × 90000, truncated to 32 bits.
        let elapsed = now.duration_since(self.rtp_ts_epoch);
        let epoch_unix = self
            .rtp_ts_epoch_system
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let ntp_us = (epoch_unix.as_micros() as u64 + NTP_EPOCH_OFFSET * 1_000_000)
            + elapsed.as_micros() as u64;
        let rtp_timestamp = (ntp_us * 90 / 1000) as u32;

        self.last_rtcp_time = Some(now);

        SenderReport {
            ssrc: self.ssrc,
            ntp_msw,
            ntp_lsw,
            rtp_timestamp,
            sender_packet_count: self.packet_count,
            sender_octet_count: self.octet_count,
        }
    }

    /// Generate an SDES packet.
    pub fn generate_sdes(&self) -> Sdes {
        Sdes {
            ssrc: self.ssrc,
            cname: self.cname.clone(),
        }
    }
}

/// State maintained by a RIST receiver for RTCP generation.
#[derive(Debug)]
pub struct RtcpReceiverState {
    /// Our SSRC.
    pub ssrc: u32,
    /// CNAME for SDES packets.
    pub cname: String,
    /// SSRC of the sender we're receiving from.
    pub sender_ssrc: Option<u32>,
    /// Highest sequence number received (extended to 32 bits for cycle counting).
    pub extended_max_seq: u32,
    /// Sequence number cycles (how many times we've wrapped around).
    seq_cycles: u16,
    /// Highest 16-bit sequence number received in current cycle.
    max_seq: u16,
    /// Whether we've received the first packet.
    first_packet_received: bool,
    /// Total packets expected (based on seq range).
    base_seq: u16,
    /// Total packets received.
    pub packets_received: u32,
    /// Total packets expected at last RR.
    expected_prior: u32,
    /// Total packets received at last RR.
    received_prior: u32,
    /// Interarrival jitter estimate (RFC 3550 A.8).
    pub jitter: f64,
    /// Last transit time for jitter calculation.
    last_transit: f64,
    /// Compact NTP timestamp from last received SR.
    pub last_sr_ntp: u32,
    /// Time when last SR was received.
    pub last_sr_time: Option<Instant>,
    /// Last RTCP send time.
    pub last_rtcp_time: Option<Instant>,
    /// RTCP emission interval.
    pub rtcp_interval: Duration,
}

impl RtcpReceiverState {
    pub fn new(ssrc: u32, cname: String, rtcp_interval: Duration) -> Self {
        Self {
            ssrc,
            cname,
            sender_ssrc: None,
            extended_max_seq: 0,
            seq_cycles: 0,
            max_seq: 0,
            first_packet_received: false,
            base_seq: 0,
            packets_received: 0,
            expected_prior: 0,
            received_prior: 0,
            jitter: 0.0,
            last_transit: 0.0,
            last_sr_ntp: 0,
            last_sr_time: None,
            last_rtcp_time: None,
            rtcp_interval,
        }
    }

    /// Update state when an RTP packet is received.
    /// `seq` is the 16-bit RTP sequence number.
    /// `rtp_timestamp` is the RTP timestamp from the packet.
    /// `arrival_time_us` is the local arrival time in microseconds (monotonic).
    pub fn on_packet_received(&mut self, seq: u16, rtp_timestamp: u32, arrival_time_us: u64) {
        if !self.first_packet_received {
            self.first_packet_received = true;
            self.base_seq = seq;
            self.max_seq = seq;
            self.extended_max_seq = seq as u32;
        } else {
            let diff = seq.wrapping_sub(self.max_seq) as i16;
            if diff > 0 {
                // Normal forward progression
                if seq < self.max_seq {
                    // Wraparound
                    self.seq_cycles += 1;
                }
                self.max_seq = seq;
                self.extended_max_seq = (self.seq_cycles as u32) << 16 | (seq as u32);
            }
        }

        self.packets_received += 1;

        // Jitter calculation per RFC 3550 A.8
        let arrival_rtp =
            (arrival_time_us as f64 / 1_000_000.0 * 90000.0) as u32;
        let transit = arrival_rtp.wrapping_sub(rtp_timestamp) as f64;
        if self.last_transit != 0.0 {
            let d = (transit - self.last_transit).abs();
            self.jitter += (d - self.jitter) / 16.0;
        }
        self.last_transit = transit;
    }

    /// Record receipt of a Sender Report.
    pub fn on_sr_received(&mut self, sr: &SenderReport, now: Instant) {
        self.sender_ssrc = Some(sr.ssrc);
        self.last_sr_ntp = sr.compact_ntp();
        self.last_sr_time = Some(now);
    }

    /// Check if it's time to send an RTCP compound packet.
    pub fn should_send_rtcp(&self, now: Instant) -> bool {
        match self.last_rtcp_time {
            None => self.first_packet_received,
            Some(last) => now.duration_since(last) >= self.rtcp_interval,
        }
    }

    /// Generate a Receiver Report with one report block.
    pub fn generate_rr(&mut self, now: Instant) -> ReceiverReport {
        self.last_rtcp_time = Some(now);

        let sender_ssrc = self.sender_ssrc.unwrap_or(0);

        if !self.first_packet_received {
            return ReceiverReport::empty(self.ssrc);
        }

        // Calculate fraction lost since last RR
        // expected = number of packets in the range [base_seq, extended_max_seq] inclusive
        let expected = self.extended_max_seq - self.base_seq as u32 + 1;
        let expected_interval = expected - self.expected_prior;
        let received_interval = self.packets_received - self.received_prior;
        self.expected_prior = expected;
        self.received_prior = self.packets_received;

        let fraction_lost = if expected_interval == 0 || received_interval >= expected_interval {
            0u8
        } else {
            let lost_interval = expected_interval - received_interval;
            ((lost_interval as f64 / expected_interval as f64) * 256.0) as u8
        };

        let cumulative_lost = (expected as i64 - self.packets_received as i64)
            .clamp(-8_388_608, 8_388_607) as i32;

        // Delay since last SR
        let delay_since_last_sr = match self.last_sr_time {
            Some(sr_time) => {
                let delay = now.duration_since(sr_time);
                // In 1/65536 seconds
                (delay.as_secs_f64() * 65536.0) as u32
            }
            None => 0,
        };

        ReceiverReport {
            ssrc: self.ssrc,
            reports: vec![ReportBlock {
                ssrc: sender_ssrc,
                fraction_lost,
                cumulative_lost,
                extended_highest_seq: self.extended_max_seq,
                jitter: self.jitter as u32,
                last_sr: self.last_sr_ntp,
                delay_since_last_sr,
            }],
        }
    }

    /// Generate an SDES packet.
    pub fn generate_sdes(&self) -> Sdes {
        Sdes {
            ssrc: self.ssrc,
            cname: self.cname.clone(),
        }
    }
}

/// NTP epoch offset from Unix epoch: 70 years in seconds.
const NTP_EPOCH_OFFSET: u64 = 2_208_988_800;

/// Convert a SystemTime to NTP timestamp (seconds since 1900-01-01).
/// Returns (MSW, LSW).
fn system_time_to_ntp(time: SystemTime) -> (u32, u32) {
    let unix_duration = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = unix_duration.as_secs() + NTP_EPOCH_OFFSET;
    let frac = ((unix_duration.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (secs as u32, frac as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sender_state_sr_generation() {
        let mut state =
            RtcpSenderState::new(0x1234, "test".to_string(), Duration::from_millis(100), Instant::now(), SystemTime::now());

        let now = Instant::now();
        state.on_packet_sent(1316, 90000, now);
        state.on_packet_sent(1316, 90000 + 3600, now);

        assert_eq!(state.packet_count, 2);
        assert_eq!(state.octet_count, 2632);

        let sr = state.generate_sr(Instant::now());
        assert_eq!(sr.ssrc, 0x1234);
        assert_eq!(sr.sender_packet_count, 2);
        assert_eq!(sr.sender_octet_count, 2632);

        // NTP timestamp should reflect wall-clock time (year 2020+),
        // not elapsed-since-start (~0 seconds + NTP offset = ~1970)
        // NTP seconds for 2020-01-01 ≈ 3_786_825_600
        assert!(
            sr.ntp_msw > 3_786_825_600,
            "NTP MSW {} should be > 3_786_825_600 (year 2020+)",
            sr.ntp_msw
        );
    }

    #[test]
    fn test_system_time_to_ntp() {
        let (msw, lsw) = system_time_to_ntp(SystemTime::now());
        // NTP seconds for 2020-01-01 ≈ 3_786_825_600
        assert!(msw > 3_786_825_600, "NTP MSW should reflect current wall-clock time");
        // Fractional part should be non-trivial (very unlikely to be exactly 0)
        let _ = lsw; // Just ensure it doesn't panic
    }

    #[test]
    fn test_receiver_state_basic() {
        let mut state =
            RtcpReceiverState::new(0x5678, "receiver".to_string(), Duration::from_millis(100));

        // Receive packets in order
        for i in 0..100u16 {
            state.on_packet_received(i, i as u32 * 3600, i as u64 * 40_000);
        }

        assert_eq!(state.packets_received, 100);
        assert_eq!(state.max_seq, 99);

        let rr = state.generate_rr(Instant::now());
        assert_eq!(rr.reports.len(), 1);
        assert_eq!(rr.reports[0].extended_highest_seq, 99);
    }

    #[test]
    fn test_receiver_state_with_loss() {
        let mut state =
            RtcpReceiverState::new(0x5678, "receiver".to_string(), Duration::from_millis(100));

        // Receive packets with a gap (skip seq 5)
        for i in 0..10u16 {
            if i != 5 {
                state.on_packet_received(i, i as u32 * 3600, i as u64 * 40_000);
            }
        }

        assert_eq!(state.packets_received, 9);
        let rr = state.generate_rr(Instant::now());
        assert!(rr.reports[0].cumulative_lost > 0);
    }

    #[test]
    fn test_should_send_rtcp() {
        let state =
            RtcpSenderState::new(0x1234, "test".to_string(), Duration::from_millis(100), Instant::now(), SystemTime::now());
        assert!(state.should_send_rtcp(Instant::now()));
    }
}
