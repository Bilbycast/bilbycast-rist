//! Periodic stats snapshot for RIST connections.

use rist_protocol::stats::RistStats;

/// Stats collector that periodically snapshots connection statistics.
pub struct StatsCollector {
    pub stats: RistStats,
}

impl StatsCollector {
    pub fn new() -> Self {
        Self {
            stats: RistStats::default(),
        }
    }

    pub fn snapshot(&self) -> RistStats {
        self.stats.clone()
    }
}

impl Default for StatsCollector {
    fn default() -> Self {
        Self::new()
    }
}
