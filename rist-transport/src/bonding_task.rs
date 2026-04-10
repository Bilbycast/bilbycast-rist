//! Multi-path bonding manager for SMPTE 2022-7 redundancy.
//!
//! When bonding is active:
//! - Sender duplicates each RTP packet across all paths
//! - Receiver merges packets from all paths via BondingMerger

use rist_protocol::protocol::bonding::BondingMerger;

/// Bonding configuration for a RIST connection.
#[derive(Debug, Clone)]
pub struct BondingConfig {
    /// Additional remote addresses for bonded paths.
    /// The primary address is in the main RistSocketConfig.
    pub additional_paths: Vec<std::net::SocketAddr>,
}

/// Bonding state for a receiver.
pub struct ReceiverBonding {
    pub merger: BondingMerger,
}

impl ReceiverBonding {
    pub fn new() -> Self {
        Self {
            merger: BondingMerger::new(),
        }
    }
}

impl Default for ReceiverBonding {
    fn default() -> Self {
        Self::new()
    }
}
