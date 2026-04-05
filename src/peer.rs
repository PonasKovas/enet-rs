use std::{
    pin::Pin,
    task::{Context, Poll},
};

use futures::{Sink, Stream};
use tokio::sync::mpsc;
use tokio_util::sync::PollSender;

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
    /// Backpressure-aware sender used by the `Sink` implementation.
    poll_sender: PollSender<ToTask>,
}

impl PeerSender {
    pub(crate) fn new(key: PeerKey, task_tx: mpsc::Sender<ToTask>) -> Self {
        let poll_sender = PollSender::new(task_tx.clone());
        Self { key, task_tx, poll_sender }
    }
}

impl Drop for PeerSender {
    fn drop(&mut self) {
        // Best-effort graceful disconnect when the sending half is dropped without
        // an explicit call to `disconnect()`. Silently ignored if the task is gone.
        let _ = self.task_tx.try_send(ToTask::Disconnect {
            peer_key: self.key,
            data: 0,
        });
    }
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

    /// Waits until the backing MPSC channel has capacity (correct backpressure).
    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.poll_sender)
            .poll_reserve(cx)
            .map_err(|_| Error::ChannelClosed)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Packet) -> Result<()> {
        let key = self.key;
        Pin::new(&mut self.poll_sender)
            .send_item(ToTask::Send { peer_key: key, packet: item })
            .map_err(|_| Error::ChannelClosed)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.poll_sender)
            .poll_close(cx)
            .map_err(|_| Error::ChannelClosed)
    }
}

// ── PeerReceiver ──────────────────────────────────────────────────────────────

/// Internal channel item: `Ok(pkt)` = data, `Err(data)` = disconnect with optional app data.
type PeerMsg = std::result::Result<Packet, Option<u32>>;

/// The receiving half of a [`Peer`] connection.
///
/// Implements [`Stream<Item = Result<Packet>>`].
pub struct PeerReceiver {
    pub rx: mpsc::Receiver<PeerMsg>,
}

impl Stream for PeerReceiver {
    type Item = Result<Packet>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Packet>>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(Ok(pkt)))    => Poll::Ready(Some(Ok(pkt))),
            Poll::Ready(Some(Err(data)))  => Poll::Ready(Some(Err(Error::Disconnected(data)))),
            Poll::Ready(None)             => Poll::Ready(None),
            Poll::Pending                 => Poll::Pending,
        }
    }
}

// ── Peer ──────────────────────────────────────────────────────────────────────

/// A connected ENet peer.
///
/// Implements [`Sink<Packet>`] for sending and [`Stream<Item = Result<Packet>>`] for
/// receiving. Can be split into a [`PeerSender`] and [`PeerReceiver`] with [`Peer::split`].
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
/// while let Ok(pkt) = peer.recv().await {
///     println!("got {} bytes on ch{}", pkt.data.len(), pkt.channel);
/// }
/// # Ok(())
/// # }
/// ```
pub struct Peer {
    sender: PeerSender,
    receiver: PeerReceiver,
}

impl Peer {
    pub(crate) fn new(key: PeerKey, task_tx: mpsc::Sender<ToTask>, rx: mpsc::Receiver<PeerMsg>) -> Self {
        Self {
            sender: PeerSender::new(key, task_tx),
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

    /// Receive the next packet.
    ///
    /// Returns `Err(Error::Disconnected(Some(data)))` when the remote peer sends a
    /// graceful disconnect (with the application-defined reason in `data`).
    /// Returns `Err(Error::Disconnected(None))` on connection timeout.
    /// Returns `Err(Error::ChannelClosed)` when the host task has shut down.
    pub async fn recv(&mut self) -> Result<Packet> {
        match self.receiver.rx.recv().await {
            Some(Ok(pkt))   => Ok(pkt),
            Some(Err(data)) => Err(Error::Disconnected(data)),
            None            => Err(Error::ChannelClosed),
        }
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
    type Item = Result<Packet>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Packet>>> {
        Pin::new(&mut self.receiver).poll_next(cx)
    }
}
