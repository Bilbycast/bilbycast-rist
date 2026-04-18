//! Async RIST transport layer built on tokio.
//!
//! Provides `RistSender` and `RistReceiver` for reliable media transport
//! using the RIST Simple Profile (TR-06-1:2020).

pub mod bonding_task;
pub mod channel;
pub mod config;
pub mod listener;
pub mod receiver;
pub mod sender;
pub mod socket;
pub mod stats;
pub mod stats_poller;

// Stubbed for future phases
pub mod dtls_channel;
pub mod tunnel_task;

pub use config::RistSocketConfig;
pub use socket::RistSocket;
pub use stats::{RistConnStats, RistConnStatsSnapshot, RistRole};
