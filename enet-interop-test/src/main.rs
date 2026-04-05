//! Interoperability tests between the Rust `enet-rs` implementation and the
//! original C ENet library.
//!
//! Two scenarios are verified:
//!   1. **Rust server ↔ C client** — the Rust server echoes; the C client
//!      sends a packet and checks the reply.
//!   2. **C server ↔ Rust client** — the C server echoes; the Rust client
//!      sends a packet and checks the reply.

use std::ffi::{CString, c_int};
use std::time::Duration;

use enet_rs::{Host, HostConfig, Packet};

// ---------------------------------------------------------------------------
// FFI — functions implemented in c_helper.c / the compiled C ENet library
// ---------------------------------------------------------------------------

extern "C" {
    /// Bind an ENet server on `port`, accept one client, echo one packet, exit.
    fn c_server_echo_once(port: u16) -> c_int;

    /// Connect to `host_str:port`, send `data[..data_len]`, receive the reply.
    /// Copies at most `out_buf_len` bytes into `out_buf`.
    /// Returns the number of bytes received, or -1 on failure.
    fn c_client_send_recv(
        host_str: *const std::ffi::c_char,
        port: u16,
        data: *const u8,
        data_len: usize,
        out_buf: *mut u8,
        out_buf_len: usize,
    ) -> c_int;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Reserve a random free local UDP port.
///
/// There is an inherent TOCTOU gap between this call and the actual bind, but
/// for loopback-only tests the window is negligibly small.
fn free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

// ---------------------------------------------------------------------------
// Test 1: Rust server  ←→  C client
// ---------------------------------------------------------------------------

fn test_rust_server_c_client(rt: &tokio::runtime::Runtime) {
    println!("[test 1] Rust server + C client …");

    let port = free_port();

    // The Rust server lives inside an async task.
    let server_handle = rt.spawn(async move {
        let addr = format!("127.0.0.1:{port}");
        let mut server = Host::bind(&addr, HostConfig::default()).await.unwrap();

        // Accept one peer, receive one packet, echo it back, then let the task end.
        if let Some(mut peer) = server.accept().await {
            if let Ok(pkt) = peer.recv().await {
                let reply = Packet::reliable(pkt.data.clone(), pkt.channel);
                peer.send_packet(reply).await.unwrap();
                // Give the host task time to flush before we drop everything.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    });

    // Give the async server a moment to bind.
    std::thread::sleep(Duration::from_millis(100));

    // Run the C client on a dedicated OS thread (C ENet is synchronous).
    let msg: &[u8] = b"hello from C client";
    let msg_vec = msg.to_vec();
    let c_thread = std::thread::spawn(move || {
        let host_cstr = CString::new("127.0.0.1").unwrap();
        let mut out_buf = vec![0u8; 1024];
        let n = unsafe {
            c_client_send_recv(
                host_cstr.as_ptr(),
                port,
                msg_vec.as_ptr(),
                msg_vec.len(),
                out_buf.as_mut_ptr(),
                out_buf.len(),
            )
        };
        assert!(n >= 0, "C client failed (returned {n})");
        out_buf[..n as usize].to_vec()
    });

    let received = c_thread.join().expect("C client thread panicked");
    assert_eq!(
        received.as_slice(),
        msg,
        "Test 1: echo mismatch — got {:?}",
        received
    );

    rt.block_on(server_handle).expect("server task panicked");
    println!("[test 1] passed");
}

// ---------------------------------------------------------------------------
// Test 2: C server  ←→  Rust client
// ---------------------------------------------------------------------------

fn test_c_server_rust_client(rt: &tokio::runtime::Runtime) {
    println!("[test 2] C server + Rust client …");

    let port = free_port();

    // Spawn the C server on an OS thread (blocking, synchronous).
    let c_server = std::thread::spawn(move || {
        let rc = unsafe { c_server_echo_once(port) };
        assert_eq!(rc, 0, "C server returned error code {rc}");
    });

    // Give the C server a moment to bind.
    std::thread::sleep(Duration::from_millis(100));

    let msg: &[u8] = b"hello from Rust client";

    rt.block_on(async move {
        let addr = format!("127.0.0.1:{port}");
        let mut peer = Host::connect(&addr, 2).await.expect("Rust client connect failed");

        peer.send_packet(Packet::reliable(msg, 0))
            .await
            .expect("send failed");

        let reply = tokio::time::timeout(Duration::from_secs(5), peer.recv())
            .await
            .expect("timeout waiting for echo")
            .expect("peer disconnected before reply");

        assert_eq!(
            reply.data.as_ref(),
            msg,
            "Test 2: echo mismatch — got {:?}",
            reply.data
        );
    });

    c_server.join().expect("C server thread panicked");
    println!("[test 2] passed");
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();

    test_rust_server_c_client(&rt);
    test_c_server_rust_client(&rt);

    println!("All interop tests passed!");
}
