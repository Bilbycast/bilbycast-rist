//! RistSocket — the main public API for RIST connections.

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::channel::RistChannel;
use crate::config::RistSocketConfig;
use crate::receiver::{self, ReceiverHandle, RistDelivered};
use crate::sender::{self, SenderHandle};
use crate::stats::{RistConnStats, RistRole};

/// A RIST socket that can send or receive media.
pub struct RistSocket {
    /// Handle for sending data (sender mode).
    sender: Option<SenderHandle>,
    /// Handle for receiving data (receiver mode).
    receiver: Option<ReceiverHandle>,
    /// Cancellation token for shutdown.
    cancel: CancellationToken,
    /// Shared connection-level stats, populated by the sender / receiver task.
    stats: Arc<RistConnStats>,
    /// Socket role (sender or receiver).
    role: RistRole,
    /// Task handles.
    _tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl RistSocket {
    /// Create a RIST sender that transmits to the given remote address.
    pub async fn sender(
        config: RistSocketConfig,
        remote_addr: SocketAddr,
    ) -> Result<Self, crate::channel::ChannelError> {
        let channel = RistChannel::bind(config.local_addr).await?;
        let cancel = CancellationToken::new();
        let stats = RistConnStats::new();

        let (sender_handle, task) = sender::spawn_sender(
            config,
            channel.rtp,
            channel.rtcp,
            remote_addr,
            cancel.clone(),
            stats.clone(),
        );

        Ok(RistSocket {
            sender: Some(sender_handle),
            receiver: None,
            cancel,
            stats,
            role: RistRole::Sender,
            _tasks: vec![task],
        })
    }

    /// Create a RIST receiver that listens for incoming media.
    pub async fn receiver(
        config: RistSocketConfig,
    ) -> Result<Self, crate::channel::ChannelError> {
        let channel = RistChannel::bind(config.local_addr).await?;
        let cancel = CancellationToken::new();
        let stats = RistConnStats::new();

        let (receiver_handle, task) = receiver::spawn_receiver(
            config,
            channel.rtp,
            channel.rtcp,
            cancel.clone(),
            stats.clone(),
        );

        Ok(RistSocket {
            sender: None,
            receiver: Some(receiver_handle),
            cancel,
            stats,
            role: RistRole::Receiver,
            _tasks: vec![task],
        })
    }

    /// Send data (sender mode only).
    pub async fn send(&self, data: Bytes) -> Result<(), mpsc::error::SendError<Bytes>> {
        if let Some(sender) = &self.sender {
            sender.tx.send(data).await
        } else {
            Err(mpsc::error::SendError(data))
        }
    }

    /// Receive a payload (receiver mode only). Convenience wrapper that
    /// drops the delivery metadata; callers that need the true UDP arrival
    /// time or wire RTP seq (e.g. for source-PCR-PLL feed or 2022-7 merge)
    /// should use [`recv_delivered`](Self::recv_delivered).
    pub async fn recv(&mut self) -> Option<Bytes> {
        self.recv_delivered().await.map(|d| d.data)
    }

    /// Receive the next packet with its delivery metadata: payload + true
    /// UDP-arrival `Instant` (captured pre-reorder-hold) + wire RTP seq.
    pub async fn recv_delivered(&mut self) -> Option<RistDelivered> {
        if let Some(receiver) = &mut self.receiver {
            receiver.rx.recv().await
        } else {
            None
        }
    }

    /// Shared stats handle — readers and the underlying task share this Arc.
    pub fn stats(&self) -> Arc<RistConnStats> {
        self.stats.clone()
    }

    /// Whether this socket was created in sender or receiver mode.
    pub fn role(&self) -> RistRole {
        self.role
    }

    /// Shut down the socket.
    pub fn close(self) {
        self.cancel.cancel();
    }
}
