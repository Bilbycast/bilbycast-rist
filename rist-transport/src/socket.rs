//! RistSocket — the main public API for RIST connections.

use std::net::SocketAddr;

use bytes::Bytes;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::channel::RistChannel;
use crate::config::RistSocketConfig;
use crate::receiver::{self, ReceiverHandle};
use crate::sender::{self, SenderHandle};

/// A RIST socket that can send or receive media.
pub struct RistSocket {
    /// Handle for sending data (sender mode).
    sender: Option<SenderHandle>,
    /// Handle for receiving data (receiver mode).
    receiver: Option<ReceiverHandle>,
    /// Cancellation token for shutdown.
    cancel: CancellationToken,
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

        let (sender_handle, task) = sender::spawn_sender(
            config,
            channel.rtp,
            channel.rtcp,
            remote_addr,
            cancel.clone(),
        );

        Ok(RistSocket {
            sender: Some(sender_handle),
            receiver: None,
            cancel,
            _tasks: vec![task],
        })
    }

    /// Create a RIST receiver that listens for incoming media.
    pub async fn receiver(
        config: RistSocketConfig,
    ) -> Result<Self, crate::channel::ChannelError> {
        let channel = RistChannel::bind(config.local_addr).await?;
        let cancel = CancellationToken::new();

        let (receiver_handle, task) = receiver::spawn_receiver(
            config,
            channel.rtp,
            channel.rtcp,
            cancel.clone(),
        );

        Ok(RistSocket {
            sender: None,
            receiver: Some(receiver_handle),
            cancel,
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

    /// Receive data (receiver mode only).
    pub async fn recv(&mut self) -> Option<Bytes> {
        if let Some(receiver) = &mut self.receiver {
            receiver.rx.recv().await
        } else {
            None
        }
    }

    /// Shut down the socket.
    pub fn close(self) {
        self.cancel.cancel();
    }
}
