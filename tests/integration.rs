use std::time::Duration;

use enet_rs::{Host, HostConfig, Packet, SendMode};
use tokio::time::timeout;

/// Bind a server on an OS-assigned port, return the address.
async fn bind_server(cfg: HostConfig) -> (Host, std::net::SocketAddr) {
    let host = Host::bind("127.0.0.1:0", cfg).await.unwrap();
    // We need to know the port — bind with port 0 then read it back.
    // Since we can't query the port from Host directly, bind on a fixed ephemeral port.
    // Workaround: bind on port 0 is fine but we need the address. Let's use a helper socket
    // to find a free port first.
    // Actually: just bind on a known free port by using port=0 and query via a temp socket.
    (host, "127.0.0.1:0".parse().unwrap())
}

/// Pick a free local TCP/UDP port.
fn free_port() -> u16 {
    let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.local_addr().unwrap().port()
}

// ── Basic connect / send / receive ───────────────────────────────────────────

#[tokio::test]
async fn test_reliable_echo() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let mut server = Host::bind(&addr, HostConfig::default()).await.unwrap();

    // Server: echo task
    tokio::spawn(async move {
        while let Some(mut peer) = server.accept().await {
            tokio::spawn(async move {
                while let Some(pkt) = peer.recv().await {
                    let reply = Packet::reliable(pkt.data.clone(), pkt.channel);
                    peer.send_packet(reply).await.unwrap();
                }
            });
        }
    });

    // Client
    let mut peer = Host::connect(&addr, 2).await.unwrap();
    peer.send_packet(Packet::reliable(b"hello world".as_ref(), 0))
        .await
        .unwrap();

    let reply = timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("timeout")
        .expect("disconnected");

    assert_eq!(reply.data.as_ref(), b"hello world");
    assert_eq!(reply.channel, 0);
    assert_eq!(reply.mode, SendMode::Reliable);
}

// ── Unreliable send ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_unreliable_send() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let mut server = Host::bind(&addr, HostConfig::default()).await.unwrap();

    tokio::spawn(async move {
        if let Some(mut peer) = server.accept().await {
            while let Some(pkt) = peer.recv().await {
                let reply = Packet::unreliable(pkt.data, pkt.channel);
                peer.send_packet(reply).await.unwrap();
            }
        }
    });

    let mut peer = Host::connect(&addr, 2).await.unwrap();
    peer.send_packet(Packet::unreliable(b"unreliable!".as_ref(), 1))
        .await
        .unwrap();

    let reply = timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("timeout")
        .expect("disconnected");

    assert_eq!(reply.data.as_ref(), b"unreliable!");
    assert_eq!(reply.channel, 1);
    assert_eq!(reply.mode, SendMode::Unreliable);
}

// ── Unsequenced send ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_unsequenced_send() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let mut server = Host::bind(&addr, HostConfig::default()).await.unwrap();

    tokio::spawn(async move {
        if let Some(mut peer) = server.accept().await {
            while let Some(pkt) = peer.recv().await {
                let reply = Packet::unsequenced(pkt.data);
                peer.send_packet(reply).await.unwrap();
            }
        }
    });

    let mut peer = Host::connect(&addr, 2).await.unwrap();
    peer.send_packet(Packet::unsequenced(b"fire and forget".as_ref()))
        .await
        .unwrap();

    let reply = timeout(Duration::from_secs(5), peer.recv())
        .await
        .expect("timeout")
        .expect("disconnected");

    assert_eq!(reply.data.as_ref(), b"fire and forget");
    assert_eq!(reply.mode, SendMode::Unsequenced);
}

// ── Multiple messages, ordering preserved ────────────────────────────────────

#[tokio::test]
async fn test_reliable_ordering() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let mut server = Host::bind(&addr, HostConfig::default()).await.unwrap();

    tokio::spawn(async move {
        if let Some(mut peer) = server.accept().await {
            while let Some(pkt) = peer.recv().await {
                peer.send_packet(Packet::reliable(pkt.data, 0)).await.unwrap();
            }
        }
    });

    let mut peer = Host::connect(&addr, 2).await.unwrap();
    const N: usize = 20;
    for i in 0u32..N as u32 {
        let msg = i.to_be_bytes().to_vec();
        peer.send_packet(Packet::reliable(msg, 0)).await.unwrap();
    }

    for i in 0u32..N as u32 {
        let pkt = timeout(Duration::from_secs(5), peer.recv())
            .await
            .expect("timeout")
            .expect("disconnected");
        let got = u32::from_be_bytes(pkt.data[..4].try_into().unwrap());
        assert_eq!(got, i, "out-of-order delivery at index {i}");
    }
}

// ── Large message (fragmented) ────────────────────────────────────────────────

#[tokio::test]
async fn test_large_reliable_fragmented() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let mut server = Host::bind(&addr, HostConfig::default()).await.unwrap();

    tokio::spawn(async move {
        if let Some(mut peer) = server.accept().await {
            while let Some(pkt) = peer.recv().await {
                peer.send_packet(Packet::reliable(pkt.data, 0)).await.unwrap();
            }
        }
    });

    let mut peer = Host::connect(&addr, 2).await.unwrap();

    // 64 KiB — definitely larger than MTU (1392 bytes) → triggers fragmentation
    let big: Vec<u8> = (0..65536u32).map(|i| (i & 0xFF) as u8).collect();
    peer.send_packet(Packet::reliable(big.clone(), 0)).await.unwrap();

    let reply = timeout(Duration::from_secs(10), peer.recv())
        .await
        .expect("timeout")
        .expect("disconnected");

    assert_eq!(reply.data.len(), big.len());
    assert_eq!(reply.data.as_ref(), big.as_slice());
}

// ── Sink + Stream API ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_sink_stream_api() {
    use futures::{SinkExt, StreamExt};

    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let mut server = Host::bind(&addr, HostConfig::default()).await.unwrap();

    tokio::spawn(async move {
        if let Some(mut peer) = server.accept().await {
            while let Some(pkt) = peer.next().await {
                peer.send(Packet::reliable(pkt.data, 0)).await.unwrap();
            }
        }
    });

    let mut peer = Host::connect(&addr, 2).await.unwrap();
    peer.send(Packet::reliable(b"sink api".as_ref(), 0)).await.unwrap();

    let reply = timeout(Duration::from_secs(5), peer.next())
        .await
        .expect("timeout")
        .expect("disconnected");

    assert_eq!(reply.data.as_ref(), b"sink api");
}

// ── Split into sender + receiver ──────────────────────────────────────────────

#[tokio::test]
async fn test_split_peer() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let mut server = Host::bind(&addr, HostConfig::default()).await.unwrap();

    tokio::spawn(async move {
        if let Some(mut peer) = server.accept().await {
            while let Some(pkt) = peer.recv().await {
                peer.send_packet(Packet::reliable(pkt.data, 0)).await.unwrap();
            }
        }
    });

    let peer = Host::connect(&addr, 2).await.unwrap();
    let (sender, mut receiver) = peer.split();

    sender.send_packet(Packet::reliable(b"split works".as_ref(), 0)).await.unwrap();

    let reply: Packet = timeout(Duration::from_secs(5), receiver.rx.recv())
        .await
        .expect("timeout")
        .expect("disconnected");

    assert_eq!(reply.data.as_ref(), b"split works");
}

// ── Multiple concurrent clients ───────────────────────────────────────────────

#[tokio::test]
async fn test_multiple_clients() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");

    let mut server = Host::bind(&addr, HostConfig::default()).await.unwrap();

    tokio::spawn(async move {
        while let Some(mut peer) = server.accept().await {
            tokio::spawn(async move {
                while let Some(pkt) = peer.recv().await {
                    peer.send_packet(Packet::reliable(pkt.data, 0)).await.unwrap();
                }
            });
        }
    });

    const CLIENTS: usize = 5;
    let mut handles = Vec::new();
    for i in 0u32..CLIENTS as u32 {
        let addr = addr.clone();
        handles.push(tokio::spawn(async move {
            let mut peer = Host::connect(&addr, 2).await.unwrap();
            let msg = i.to_be_bytes().to_vec();
            peer.send_packet(Packet::reliable(msg.clone(), 0)).await.unwrap();
            let reply = timeout(Duration::from_secs(5), peer.recv())
                .await
                .expect("timeout")
                .expect("disconnected");
            assert_eq!(reply.data.as_ref(), msg.as_slice());
        }));
    }

    for h in handles {
        h.await.unwrap();
    }
}
