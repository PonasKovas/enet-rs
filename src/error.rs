use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("connection timed out")]
    TimedOut,

    #[error("peer disconnected")]
    Disconnected,

    #[error("connection refused")]
    ConnectionRefused,

    #[error("invalid packet: {0}")]
    InvalidPacket(&'static str),

    #[error("channel closed")]
    ChannelClosed,

    #[error("connect ID mismatch")]
    ConnectIdMismatch,

    #[error("too many peers")]
    TooManyPeers,
}

pub type Result<T> = std::result::Result<T, Error>;
