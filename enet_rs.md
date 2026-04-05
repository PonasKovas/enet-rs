# Crate Documentation

**Version:** 0.1.0

**Format Version:** 57

# Module `enet_rs`

# enet-rs

Async Rust implementation of the [ENet](http://enet.bespin.org/) reliable UDP protocol.

## Quick-start — Server

```rust,no_run
use enet_rs::{Host, HostConfig, Packet};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut host = Host::bind("0.0.0.0:7777", HostConfig::default()).await?;

    while let Some(mut peer) = host.accept().await {
        tokio::spawn(async move {
            while let Some(pkt) = peer.recv().await {
                println!("ch{}: {} bytes", pkt.channel, pkt.data.len());
                peer.send_packet(Packet::reliable(b"pong".as_ref(), 0)).await.ok();
            }
        });
    }
    Ok(())
}
```

## Quick-start — Client

```rust,no_run
use enet_rs::{Host, HostConfig, Packet};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut peer = Host::connect("127.0.0.1:7777", 2).await?;
    peer.send_packet(Packet::reliable(b"ping".as_ref(), 0)).await?;
    if let Some(pkt) = peer.recv().await {
        println!("reply: {:?}", pkt.data);
    }
    Ok(())
}
```

## Types

### Enum `SendMode`

Delivery semantics for a [`Packet`].

```rust
pub enum SendMode {
    Reliable,
    Unreliable,
    Unsequenced,
}
```

#### Variants

##### `Reliable`

Reliable, ordered delivery (retransmitted until ACKed).

##### `Unreliable`

Unreliable, ordered delivery (late arrivals are discarded).

##### `Unsequenced`

Fire-and-forget, no ordering guarantees.

#### Implementations

##### Trait Implementations

- **Any**
  - ```rust
    fn type_id(self: &Self) -> TypeId { /* ... */ }
    ```

- **Borrow**
  - ```rust
    fn borrow(self: &Self) -> &T { /* ... */ }
    ```

- **BorrowMut**
  - ```rust
    fn borrow_mut(self: &mut Self) -> &mut T { /* ... */ }
    ```

- **Clone**
  - ```rust
    fn clone(self: &Self) -> SendMode { /* ... */ }
    ```

- **CloneToUninit**
  - ```rust
    unsafe fn clone_to_uninit(self: &Self, dest: *mut u8) { /* ... */ }
    ```

- **Copy**
- **Debug**
  - ```rust
    fn fmt(self: &Self, f: &mut $crate::fmt::Formatter<''_>) -> $crate::fmt::Result { /* ... */ }
    ```

- **Eq**
- **Freeze**
- **From**
  - ```rust
    fn from(t: T) -> T { /* ... */ }
    ```
    Returns the argument unchanged.

- **Instrument**
- **Into**
  - ```rust
    fn into(self: Self) -> U { /* ... */ }
    ```
    Calls `U::from(self)`.

- **PartialEq**
  - ```rust
    fn eq(self: &Self, other: &SendMode) -> bool { /* ... */ }
    ```

- **RefUnwindSafe**
- **Send**
- **StructuralPartialEq**
- **Sync**
- **ToOwned**
  - ```rust
    fn to_owned(self: &Self) -> T { /* ... */ }
    ```

  - ```rust
    fn clone_into(self: &Self, target: &mut T) { /* ... */ }
    ```

- **TryFrom**
  - ```rust
    fn try_from(value: U) -> Result<T, <T as TryFrom<U>>::Error> { /* ... */ }
    ```

- **TryInto**
  - ```rust
    fn try_into(self: Self) -> Result<U, <U as TryFrom<T>>::Error> { /* ... */ }
    ```

- **Unpin**
- **UnwindSafe**
- **VZip**
  - ```rust
    fn vzip(self: Self) -> V { /* ... */ }
    ```

- **WithSubscriber**
### Struct `Packet`

A message sent to or received from a peer.

```rust
pub struct Packet {
    pub data: bytes::Bytes,
    pub channel: u8,
    pub mode: SendMode,
}
```

#### Fields

| Name | Type | Documentation |
|------|------|---------------|
| `data` | `bytes::Bytes` | The raw payload. |
| `channel` | `u8` | Channel index (0-based). |
| `mode` | `SendMode` | Delivery semantics. |

#### Implementations

##### Methods

- ```rust
  pub fn reliable</* synthetic */ impl Into<Bytes>: Into<Bytes>>(data: impl Into<Bytes>, channel: u8) -> Self { /* ... */ }
  ```
  Create a reliable packet.

- ```rust
  pub fn unreliable</* synthetic */ impl Into<Bytes>: Into<Bytes>>(data: impl Into<Bytes>, channel: u8) -> Self { /* ... */ }
  ```
  Create an unreliable, sequenced packet.

- ```rust
  pub fn unsequenced</* synthetic */ impl Into<Bytes>: Into<Bytes>>(data: impl Into<Bytes>) -> Self { /* ... */ }
  ```
  Create an unsequenced (fire-and-forget) packet.

##### Trait Implementations

- **Any**
  - ```rust
    fn type_id(self: &Self) -> TypeId { /* ... */ }
    ```

- **Borrow**
  - ```rust
    fn borrow(self: &Self) -> &T { /* ... */ }
    ```

- **BorrowMut**
  - ```rust
    fn borrow_mut(self: &mut Self) -> &mut T { /* ... */ }
    ```

- **Clone**
  - ```rust
    fn clone(self: &Self) -> Packet { /* ... */ }
    ```

- **CloneToUninit**
  - ```rust
    unsafe fn clone_to_uninit(self: &Self, dest: *mut u8) { /* ... */ }
    ```

- **Debug**
  - ```rust
    fn fmt(self: &Self, f: &mut $crate::fmt::Formatter<''_>) -> $crate::fmt::Result { /* ... */ }
    ```

- **Freeze**
- **From**
  - ```rust
    fn from(t: T) -> T { /* ... */ }
    ```
    Returns the argument unchanged.

- **Instrument**
- **Into**
  - ```rust
    fn into(self: Self) -> U { /* ... */ }
    ```
    Calls `U::from(self)`.

- **RefUnwindSafe**
- **Send**
- **Sink**
  - ```rust
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<''_>) -> Poll<Result<()>> { /* ... */ }
    ```
    Waits until the backing MPSC channel has capacity (correct backpressure).

  - ```rust
    fn start_send(self: Pin<&mut Self>, item: Packet) -> Result<()> { /* ... */ }
    ```

  - ```rust
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<''_>) -> Poll<Result<()>> { /* ... */ }
    ```

  - ```rust
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<''_>) -> Poll<Result<()>> { /* ... */ }
    ```

  - ```rust
    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<''_>) -> Poll<Result<()>> { /* ... */ }
    ```

  - ```rust
    fn start_send(self: Pin<&mut Self>, item: Packet) -> Result<()> { /* ... */ }
    ```

  - ```rust
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<''_>) -> Poll<Result<()>> { /* ... */ }
    ```

  - ```rust
    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<''_>) -> Poll<Result<()>> { /* ... */ }
    ```

- **Sync**
- **ToOwned**
  - ```rust
    fn to_owned(self: &Self) -> T { /* ... */ }
    ```

  - ```rust
    fn clone_into(self: &Self, target: &mut T) { /* ... */ }
    ```

- **TryFrom**
  - ```rust
    fn try_from(value: U) -> Result<T, <T as TryFrom<U>>::Error> { /* ... */ }
    ```

- **TryInto**
  - ```rust
    fn try_into(self: Self) -> Result<U, <U as TryFrom<T>>::Error> { /* ... */ }
    ```

- **Unpin**
- **UnwindSafe**
- **VZip**
  - ```rust
    fn vzip(self: Self) -> V { /* ... */ }
    ```

- **WithSubscriber**
### Struct `HostConfig`

Configuration for a [`Host`].

```rust
pub struct HostConfig {
    pub max_peers: usize,
    pub channel_count: u8,
    pub use_checksum: bool,
    pub incoming_bandwidth: u32,
    pub outgoing_bandwidth: u32,
}
```

#### Fields

| Name | Type | Documentation |
|------|------|---------------|
| `max_peers` | `usize` | Maximum number of simultaneous peers (default: 64). |
| `channel_count` | `u8` | Number of channels per peer (default: 2). |
| `use_checksum` | `bool` | Whether to compute and verify CRC-32 checksums (default: false). |
| `incoming_bandwidth` | `u32` | Incoming bandwidth limit in bytes/sec (0 = unlimited). |
| `outgoing_bandwidth` | `u32` | Outgoing bandwidth limit in bytes/sec (0 = unlimited). |

#### Implementations

##### Trait Implementations

- **Any**
  - ```rust
    fn type_id(self: &Self) -> TypeId { /* ... */ }
    ```

- **Borrow**
  - ```rust
    fn borrow(self: &Self) -> &T { /* ... */ }
    ```

- **BorrowMut**
  - ```rust
    fn borrow_mut(self: &mut Self) -> &mut T { /* ... */ }
    ```

- **Clone**
  - ```rust
    fn clone(self: &Self) -> HostConfig { /* ... */ }
    ```

- **CloneToUninit**
  - ```rust
    unsafe fn clone_to_uninit(self: &Self, dest: *mut u8) { /* ... */ }
    ```

- **Debug**
  - ```rust
    fn fmt(self: &Self, f: &mut $crate::fmt::Formatter<''_>) -> $crate::fmt::Result { /* ... */ }
    ```

- **Default**
  - ```rust
    fn default() -> Self { /* ... */ }
    ```

- **Freeze**
- **From**
  - ```rust
    fn from(t: T) -> T { /* ... */ }
    ```
    Returns the argument unchanged.

- **Instrument**
- **Into**
  - ```rust
    fn into(self: Self) -> U { /* ... */ }
    ```
    Calls `U::from(self)`.

- **RefUnwindSafe**
- **Send**
- **Sync**
- **ToOwned**
  - ```rust
    fn to_owned(self: &Self) -> T { /* ... */ }
    ```

  - ```rust
    fn clone_into(self: &Self, target: &mut T) { /* ... */ }
    ```

- **TryFrom**
  - ```rust
    fn try_from(value: U) -> Result<T, <T as TryFrom<U>>::Error> { /* ... */ }
    ```

- **TryInto**
  - ```rust
    fn try_into(self: Self) -> Result<U, <U as TryFrom<T>>::Error> { /* ... */ }
    ```

- **Unpin**
- **UnwindSafe**
- **VZip**
  - ```rust
    fn vzip(self: Self) -> V { /* ... */ }
    ```

- **WithSubscriber**
### Struct `Host`

An ENet host (server or client-side connection manager).

Spawns an internal tokio task that drives all protocol logic. User code
interacts via async channels — no manual event loops required.

```rust
pub struct Host {
    // Some fields omitted
}
```

#### Fields

| Name | Type | Documentation |
|------|------|---------------|
| *private fields* | ... | *Some fields have been omitted* |

#### Implementations

##### Methods

- ```rust
  pub async fn bind</* synthetic */ impl ToSocketAddrs: ToSocketAddrs>(addr: impl ToSocketAddrs, config: HostConfig) -> Result<Self> { /* ... */ }
  ```
  Bind a server host to `addr` and start the background task.

- ```rust
  pub async fn accept(self: &mut Self) -> Option<Peer> { /* ... */ }
  ```
  Wait for the next incoming [`Peer`] connection.

- ```rust
  pub async fn connect</* synthetic */ impl ToSocketAddrs: ToSocketAddrs>(addr: impl ToSocketAddrs, channel_count: u8) -> Result<Peer> { /* ... */ }
  ```
  Connect to a remote ENet server.

- ```rust
  pub async fn connect_with_timeout</* synthetic */ impl ToSocketAddrs: ToSocketAddrs>(addr: impl ToSocketAddrs, channel_count: u8, connect_timeout: Duration) -> Result<Peer> { /* ... */ }
  ```
  Like [`connect`](Self::connect) but with a custom timeout.

##### Trait Implementations

- **Any**
  - ```rust
    fn type_id(self: &Self) -> TypeId { /* ... */ }
    ```

- **Borrow**
  - ```rust
    fn borrow(self: &Self) -> &T { /* ... */ }
    ```

- **BorrowMut**
  - ```rust
    fn borrow_mut(self: &mut Self) -> &mut T { /* ... */ }
    ```

- **Freeze**
- **From**
  - ```rust
    fn from(t: T) -> T { /* ... */ }
    ```
    Returns the argument unchanged.

- **Instrument**
- **Into**
  - ```rust
    fn into(self: Self) -> U { /* ... */ }
    ```
    Calls `U::from(self)`.

- **RefUnwindSafe**
- **Send**
- **Sync**
- **TryFrom**
  - ```rust
    fn try_from(value: U) -> Result<T, <T as TryFrom<U>>::Error> { /* ... */ }
    ```

- **TryInto**
  - ```rust
    fn try_into(self: Self) -> Result<U, <U as TryFrom<T>>::Error> { /* ... */ }
    ```

- **Unpin**
- **UnwindSafe**
- **VZip**
  - ```rust
    fn vzip(self: Self) -> V { /* ... */ }
    ```

- **WithSubscriber**
## Re-exports

### Re-export `Error`

```rust
pub use error::Error;
```

### Re-export `Result`

```rust
pub use error::Result;
```

### Re-export `Peer`

```rust
pub use peer::Peer;
```

### Re-export `PeerReceiver`

```rust
pub use peer::PeerReceiver;
```

### Re-export `PeerSender`

```rust
pub use peer::PeerSender;
```

### Re-export `PeerKey`

```rust
pub use task::PeerKey;
```

