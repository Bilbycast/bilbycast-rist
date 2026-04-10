//! RIST listener for accepting incoming sender connections.
//!
//! Binds on an RTP+RTCP port pair and waits for the first RTP packet
//! to arrive, then creates a RistSocket in receiver mode.

use crate::config::RistSocketConfig;

/// A listener that accepts incoming RIST sender connections.
///
/// In RIST Simple Profile, the "listener" is the receiver side.
/// It binds on an even port P (RTP) and P+1 (RTCP), then starts
/// processing when the first RTP packet arrives.
pub struct RistListener {
    pub config: RistSocketConfig,
}

impl RistListener {
    /// Create a new listener with the given configuration.
    pub fn new(config: RistSocketConfig) -> Self {
        Self { config }
    }

    /// Start listening. Returns a RistSocket in receiver mode.
    pub async fn accept(
        &self,
    ) -> Result<crate::socket::RistSocket, crate::channel::ChannelError> {
        crate::socket::RistSocket::receiver(self.config.clone()).await
    }
}
