use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::{Sink, Stream};
use tokio::sync::mpsc;

use crate::{
    Packet,
    error::{Error, Result},
    task::{PeerKey, ToTask},
};

// ── PeerSender ────────────────────────────────────────────────────────────────

/// The sending half of a [`Peer`] connection.
///
/// Implements [`Sink<Packet>`].
pub struct PeerSender {
    pub(crate) key: PeerKey,
    pub(crate) task_tx: mpsc::Sender<ToTask>,
}

impl PeerSender {
    /// Send a packet, waiting until the internal send buffer has room.
    pub async fn send_packet(&self, pkt: Packet) -> Result<()> {
        self.task_tx
            .send(ToTask::Send {
                peer_key: self.key,
                packet: pkt,
            })
            .await
            .map_err(|_| Error::ChannelClosed)
    }

    /// Initiate a graceful disconnect.
    pub async fn disconnect(&self, data: u32) -> Result<()> {
        self.task_tx
            .send(ToTask::Disconnect {
                peer_key: self.key,
                data,
            })
            .await
            .map_err(|_| Error::ChannelClosed)
    }
}

impl Sink<Packet> for PeerSender {
    type Error = Error;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        // The backing channel has a large buffer; report always-ready.
        // `start_send` will return an error if the channel is genuinely full.
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Packet) -> Result<()> {
        self.task_tx
            .try_send(ToTask::Send {
                peer_key: self.key,
                packet: item,
            })
            .map_err(|_| Error::ChannelClosed)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }
}

// ── PeerReceiver ──────────────────────────────────────────────────────────────

/// The receiving half of a [`Peer`] connection.
///
/// Implements [`Stream<Item = Packet>`].
pub struct PeerReceiver {
    pub rx: mpsc::Receiver<Packet>,
}

impl Stream for PeerReceiver {
    type Item = Packet;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Packet>> {
        self.rx.poll_recv(cx)
    }
}

// ── Peer ──────────────────────────────────────────────────────────────────────

/// A connected ENet peer.
///
/// Implements both [`Sink<Packet>`] and [`Stream<Item = Packet>`] and can be
/// split into a [`PeerSender`] and [`PeerReceiver`] with [`Peer::split`].
///
/// # Example
///
/// ```rust,no_run
/// use enet_rs::{Host, Packet};
/// use futures::{SinkExt, StreamExt};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let mut peer = Host::connect("127.0.0.1:7777", 2).await?;
/// peer.send(Packet::reliable(b"hello".as_slice(), 0)).await?;
/// if let Some(reply) = peer.next().await {
///     println!("got {} bytes on channel {}", reply.data.len(), reply.channel);
/// }
/// # Ok(())
/// # }
/// ```
pub struct Peer {
    sender: PeerSender,
    receiver: PeerReceiver,
}

impl Peer {
    pub(crate) fn new(key: PeerKey, task_tx: mpsc::Sender<ToTask>, rx: mpsc::Receiver<Packet>) -> Self {
        Self {
            sender: PeerSender {
                key,
                task_tx,
            },
            receiver: PeerReceiver { rx },
        }
    }

    /// Split into independent sender and receiver.
    pub fn split(self) -> (PeerSender, PeerReceiver) {
        (self.sender, self.receiver)
    }

    /// The peer's slot key (for use with advanced [`Host`] APIs).
    pub fn key(&self) -> PeerKey {
        self.sender.key
    }

    /// Send a packet.
    pub async fn send_packet(&self, pkt: Packet) -> crate::error::Result<()> {
        self.sender.send_packet(pkt).await
    }

    /// Receive the next packet. Returns `None` when disconnected.
    pub async fn recv(&mut self) -> Option<Packet> {
        self.receiver.rx.recv().await
    }

    /// Initiate a graceful disconnect.
    pub async fn disconnect(&self, data: u32) -> crate::error::Result<()> {
        self.sender.disconnect(data).await
    }
}

// Sink: delegate to sender
impl Sink<Packet> for Peer {
    type Error = Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.sender).poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Packet) -> Result<()> {
        Pin::new(&mut self.sender).start_send(item)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.sender).poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.sender).poll_close(cx)
    }
}

// Stream: delegate to receiver
impl Stream for Peer {
    type Item = Packet;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Packet>> {
        Pin::new(&mut self.receiver).poll_next(cx)
    }
}
