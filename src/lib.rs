//! # enet-rs
//!
//! Async Rust implementation of the [ENet](http://enet.bespin.org/) reliable UDP protocol.
//!
//! ## Quick-start — Server
//!
//! ```rust,no_run
//! use enet_rs::{Host, HostConfig, Packet};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut host = Host::bind("0.0.0.0:7777", HostConfig::default()).await?;
//!
//!     while let Some(mut peer) = host.accept().await {
//!         tokio::spawn(async move {
//!             while let Some(pkt) = peer.recv().await {
//!                 println!("ch{}: {} bytes", pkt.channel, pkt.data.len());
//!                 peer.send_packet(Packet::reliable(b"pong".as_ref(), 0)).await.ok();
//!             }
//!         });
//!     }
//!     Ok(())
//! }
//! ```
//!
//! ## Quick-start — Client
//!
//! ```rust,no_run
//! use enet_rs::{Host, HostConfig, Packet};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut peer = Host::connect("127.0.0.1:7777", 2).await?;
//!     peer.send_packet(Packet::reliable(b"ping".as_ref(), 0)).await?;
//!     if let Some(pkt) = peer.recv().await {
//!         println!("reply: {:?}", pkt.data);
//!     }
//!     Ok(())
//! }
//! ```

mod channel;
mod error;
mod peer;
mod protocol;
mod task;

pub use error::{Error, Result};
pub use peer::{Peer, PeerReceiver, PeerSender};
pub use task::PeerKey;

use std::{net::ToSocketAddrs, sync::Arc, time::Duration};
use bytes::Bytes;
use tokio::{net::UdpSocket, sync::mpsc, time::timeout};

use task::{HostTask, ToTask};

// ── Packet ────────────────────────────────────────────────────────────────────

/// Delivery semantics for a [`Packet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendMode {
    /// Reliable, ordered delivery (retransmitted until ACKed).
    Reliable,
    /// Unreliable, ordered delivery (late arrivals are discarded).
    Unreliable,
    /// Fire-and-forget, no ordering guarantees.
    Unsequenced,
}

/// A message sent to or received from a peer.
#[derive(Debug, Clone)]
pub struct Packet {
    /// The raw payload.
    pub data: Bytes,
    /// Channel index (0-based).
    pub channel: u8,
    /// Delivery semantics.
    pub mode: SendMode,
}

impl Packet {
    /// Create a reliable packet.
    pub fn reliable(data: impl Into<Bytes>, channel: u8) -> Self {
        Self { data: data.into(), channel, mode: SendMode::Reliable }
    }

    /// Create an unreliable, sequenced packet.
    pub fn unreliable(data: impl Into<Bytes>, channel: u8) -> Self {
        Self { data: data.into(), channel, mode: SendMode::Unreliable }
    }

    /// Create an unsequenced (fire-and-forget) packet.
    pub fn unsequenced(data: impl Into<Bytes>) -> Self {
        Self { data: data.into(), channel: 0, mode: SendMode::Unsequenced }
    }
}

// ── HostConfig ────────────────────────────────────────────────────────────────

/// Configuration for a [`Host`].
#[derive(Debug, Clone)]
pub struct HostConfig {
    /// Maximum number of simultaneous peers (default: 64).
    pub max_peers: usize,
    /// Number of channels per peer (default: 2).
    pub channel_count: u8,
    /// Whether to compute and verify CRC-32 checksums (default: false).
    pub use_checksum: bool,
    /// Incoming bandwidth limit in bytes/sec (0 = unlimited).
    pub incoming_bandwidth: u32,
    /// Outgoing bandwidth limit in bytes/sec (0 = unlimited).
    pub outgoing_bandwidth: u32,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            max_peers: 64,
            channel_count: 2,
            use_checksum: false,
            incoming_bandwidth: 0,
            outgoing_bandwidth: 0,
        }
    }
}

// ── Host ─────────────────────────────────────────────────────────────────────

/// An ENet host (server or client-side connection manager).
///
/// Spawns an internal tokio task that drives all protocol logic. User code
/// interacts via async channels — no manual event loops required.
pub struct Host {
    /// Receives newly accepted [`Peer`] connections.
    accept_rx: mpsc::Receiver<(PeerKey, mpsc::Receiver<Packet>)>,
    /// Sends control messages to the background task.
    task_tx: mpsc::Sender<ToTask>,
}

impl Host {
    /// Bind a server host to `addr` and start the background task.
    pub async fn bind(addr: impl ToSocketAddrs, config: HostConfig) -> Result<Self> {
        let addr = addr
            .to_socket_addrs()
            .map_err(Error::Io)?
            .next()
            .ok_or_else(|| Error::Io(std::io::Error::other("no address")))?;

        let socket = Arc::new(UdpSocket::bind(addr).await?);

        let (accept_tx, accept_rx) = mpsc::channel(64);
        let (task_tx, task_rx) = mpsc::channel(1024);

        let task = HostTask::new(
            socket,
            accept_tx,
            task_rx,
            config.max_peers,
            config.use_checksum,
            config.incoming_bandwidth,
            config.outgoing_bandwidth,
        );
        tokio::spawn(task.run());

        Ok(Self { accept_rx, task_tx })
    }

    /// Wait for the next incoming [`Peer`] connection.
    ///
    /// Returns `None` when the host task has shut down.
    pub async fn accept(&mut self) -> Option<Peer> {
        let (key, rx) = self.accept_rx.recv().await?;
        Some(Peer::new(key, self.task_tx.clone(), rx))
    }

    /// Connect to a remote ENet server.
    ///
    /// Opens an ephemeral UDP socket, performs the ENet handshake, and returns
    /// a [`Peer`] when the connection is established.
    ///
    /// `channel_count` is the number of channels to negotiate (≥ 1).
    pub async fn connect(
        addr: impl ToSocketAddrs,
        channel_count: u8,
    ) -> Result<Peer> {
        Self::connect_with_timeout(addr, channel_count, Duration::from_secs(5)).await
    }

    /// Like [`connect`](Self::connect) but with a custom timeout.
    pub async fn connect_with_timeout(
        addr: impl ToSocketAddrs,
        channel_count: u8,
        connect_timeout: Duration,
    ) -> Result<Peer> {
        let addr = addr
            .to_socket_addrs()
            .map_err(Error::Io)?
            .next()
            .ok_or_else(|| Error::Io(std::io::Error::other("no address")))?;

        // Bind to a random local port
        let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);

        let (accept_tx, _accept_rx) = mpsc::channel(1); // unused for client
        let (task_tx, task_rx) = mpsc::channel(1024);
        let task = HostTask::new(
            socket,
            accept_tx,
            task_rx,
            1, // only one peer
            false,
            0, // no incoming bandwidth limit for client
            0, // no outgoing bandwidth limit for client
        );
        tokio::spawn(task.run());

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        task_tx
            .send(ToTask::Connect {
                addr,
                channel_count,
                data: 0,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::ChannelClosed)?;

        // Unwrap the three layers: timeout, oneshot cancellation, inner Result.
        let (key, rx) = match timeout(connect_timeout, reply_rx).await {
            Err(_elapsed) => return Err(Error::TimedOut),
            Ok(Err(_canceled)) => return Err(Error::ChannelClosed),
            Ok(Ok(result)) => result?,
        };

        Ok(Peer::new(key, task_tx, rx))
    }
}
