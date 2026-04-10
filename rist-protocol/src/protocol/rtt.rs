//! RTT estimation via RTCP APP echo request/response.
//!
//! Uses exponentially weighted moving average (EWMA), similar to TCP's SRTT.

use std::time::{Duration, Instant};

/// RTT estimator using RTCP RTT Echo Request/Response.
#[derive(Debug)]
pub struct RttEstimator {
    /// Smoothed RTT (EWMA).
    srtt: Option<Duration>,
    /// RTT variance.
    rttvar: Option<Duration>,
    /// Time when the last echo request was sent.
    last_request_time: Option<Instant>,
    /// Timestamp value included in the last request (for matching responses).
    last_request_timestamp: Option<u64>,
    /// Interval between RTT echo requests.
    pub request_interval: Duration,
    /// Time of next scheduled request.
    pub next_request_at: Option<Instant>,
}

impl RttEstimator {
    pub fn new(request_interval: Duration) -> Self {
        Self {
            srtt: None,
            rttvar: None,
            last_request_time: None,
            last_request_timestamp: None,
            request_interval,
            next_request_at: None,
        }
    }

    /// Generate a timestamp for an outgoing RTT Echo Request.
    /// Returns (msw, lsw) to include in the RTCP APP packet.
    pub fn generate_request(&mut self, now: Instant) -> (u32, u32) {
        let elapsed = now
            .duration_since(self.last_request_time.unwrap_or(now))
            .as_micros() as u64;
        // Use elapsed microseconds as the timestamp
        let ts = self.last_request_timestamp.map_or(0, |t| t + elapsed);
        self.last_request_time = Some(now);
        self.last_request_timestamp = Some(ts);
        self.next_request_at = Some(now + self.request_interval);
        // Split into MSW and LSW
        ((ts >> 32) as u32, ts as u32)
    }

    /// Process an incoming RTT Echo Response.
    /// `request_msw`/`request_lsw` are the echoed timestamp from our request.
    /// `processing_delay_us` is the responder's processing time.
    pub fn on_response(
        &mut self,
        now: Instant,
        request_msw: u32,
        request_lsw: u32,
        processing_delay_us: u32,
    ) {
        if let Some(sent_time) = self.last_request_time {
            let total_time = now.duration_since(sent_time);
            let processing = Duration::from_micros(processing_delay_us as u64);
            let rtt = total_time.saturating_sub(processing);
            self.update_rtt(rtt);
        }
        // Also verify timestamp matches
        let _ = (request_msw, request_lsw); // Used for multi-request tracking in future
    }

    /// Update SRTT and RTTVAR using TCP-style EWMA (RFC 6298).
    fn update_rtt(&mut self, sample: Duration) {
        match self.srtt {
            None => {
                // First measurement
                self.srtt = Some(sample);
                self.rttvar = Some(sample / 2);
            }
            Some(srtt) => {
                let rttvar = self.rttvar.unwrap_or(sample / 2);
                // RTTVAR = (1 - 1/4) * RTTVAR + 1/4 * |SRTT - R|
                let diff = if sample > srtt {
                    sample - srtt
                } else {
                    srtt - sample
                };
                let new_rttvar = rttvar * 3 / 4 + diff / 4;
                // SRTT = (1 - 1/8) * SRTT + 1/8 * R
                let new_srtt = srtt * 7 / 8 + sample / 8;
                self.srtt = Some(new_srtt);
                self.rttvar = Some(new_rttvar);
            }
        }
    }

    /// Get the current smoothed RTT estimate.
    pub fn srtt(&self) -> Option<Duration> {
        self.srtt
    }

    /// Get the recommended NACK delay based on RTT.
    /// Returns half the SRTT (fire NACK after half RTT for timely recovery).
    pub fn nack_delay(&self) -> Option<Duration> {
        self.srtt.map(|s| s / 2)
    }

    /// Check if it's time to send an RTT Echo Request.
    pub fn should_send_request(&self, now: Instant) -> bool {
        match self.next_request_at {
            None => true,
            Some(next) => now >= next,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rtt_first_measurement() {
        let mut est = RttEstimator::new(Duration::from_secs(1));
        est.update_rtt(Duration::from_millis(50));
        assert_eq!(est.srtt(), Some(Duration::from_millis(50)));
    }

    #[test]
    fn test_rtt_ewma() {
        let mut est = RttEstimator::new(Duration::from_secs(1));
        est.update_rtt(Duration::from_millis(100));
        est.update_rtt(Duration::from_millis(60));

        let srtt = est.srtt().unwrap();
        // Should be between 60 and 100, closer to 100 (7/8 weight on old)
        assert!(srtt > Duration::from_millis(60));
        assert!(srtt < Duration::from_millis(100));
    }

    #[test]
    fn test_nack_delay() {
        let mut est = RttEstimator::new(Duration::from_secs(1));
        assert!(est.nack_delay().is_none());

        est.update_rtt(Duration::from_millis(80));
        assert_eq!(est.nack_delay(), Some(Duration::from_millis(40)));
    }
}
