/// Background I/O task: owns the UDP socket and all peer state machines.
use std::{
    collections::HashMap,
    net::SocketAddr,
    time::{Duration, Instant},
};

use bytes::{Bytes, BytesMut};
use rand::Rng;
use tokio::{net::UdpSocket, sync::mpsc, time};
use tracing::warn;

use crate::{
    Packet, SendMode,
    channel::{Channel, FragmentBuffer, UnsequencedWindow, encode_command},
    error::Error,
    protocol::{
        self, AcknowledgeCmd, ConnectCmd, Command, CommandBody, CommandHeader, ProtocolHeader,
        VerifyConnectCmd,
        CHANNEL_ID_CONNECTION, CMD_ACKNOWLEDGE, CMD_CONNECT, CMD_DISCONNECT, CMD_PING,
        CMD_SEND_FRAGMENT, CMD_SEND_RELIABLE, CMD_SEND_UNRELIABLE,
        CMD_SEND_UNRELIABLE_FRAGMENT, CMD_SEND_UNSEQUENCED, CMD_VERIFY_CONNECT,
        COMMAND_FLAG_ACKNOWLEDGE, COMMAND_FLAG_UNSEQUENCED,
        DEFAULT_PING_INTERVAL_MS, DEFAULT_RTT_MS, HEADER_FLAG_SENT_TIME, HEADER_SESSION_SHIFT,
        MAX_FRAGMENT_COUNT, MTU_DEFAULT, MTU_MAX, MTU_MIN, PEER_ID_UNASSIGNED,
        THROTTLE_ACCELERATION, THROTTLE_DECELERATION, THROTTLE_INTERVAL_MS, TIMEOUT_LIMIT,
        TIMEOUT_MAXIMUM_MS, TIMEOUT_MINIMUM_MS, WINDOW_SIZE_MAX, WINDOW_SIZE_MIN,
    },
};

// ── Messages: user ↔ task ─────────────────────────────────────────────────────

pub enum ToTask {
    Send {
        peer_key: PeerKey,
        packet: Packet,
    },
    Disconnect {
        peer_key: PeerKey,
        data: u32,
    },
    /// Client-side connect request. Reply = `(PeerKey, Receiver)` on success.
    Connect {
        addr: SocketAddr,
        channel_count: u8,
        data: u32,
        reply: tokio::sync::oneshot::Sender<Result<(PeerKey, mpsc::Receiver<Packet>), Error>>,
    },
}

/// Opaque key identifying a peer slot in the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerKey(pub usize);

// ── Peer state ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Connecting,
    AcknowledgingConnect,
    Connected,
    Disconnecting,
}

struct OutstandingReliable {
    reliable_seq: u16,
    channel_id: u8,
    sent_time: u32,
    rtt_timeout: u32,
    send_attempts: u32,
    packet_bytes: Bytes,
}

struct PeerEntry {
    key: PeerKey,
    addr: SocketAddr,
    state: PeerState,

    outgoing_peer_id: u16,
    connect_id: u32,
    incoming_session_id: u8,
    outgoing_session_id: u8,

    mtu: u32,
    window_size: u32,
    channel_count: u8,

    throttle_interval: u32,
    throttle_acceleration: u32,
    throttle_deceleration: u32,

    rtt: u32,
    rtt_var: u32,
    rtt_initialized: bool,

    last_send_time: u32,
    last_receive_time: u32,

    channels: Vec<Channel>,
    unsequenced_window: UnsequencedWindow,
    outgoing_reliable: Vec<OutstandingReliable>,

    /// Delivers decoded packets to the user-facing Peer.
    to_user: mpsc::Sender<Packet>,
    /// Held until the connection is established, then given to the user.
    to_user_rx: Option<mpsc::Receiver<Packet>>,

    /// Client-side: reply channel for Host::connect().
    connect_reply: Option<
        tokio::sync::oneshot::Sender<Result<(PeerKey, mpsc::Receiver<Packet>), Error>>,
    >,
    /// True for server-side (incoming) peers; on connect they go to accept_tx.
    is_incoming: bool,

    // Connection-level sequence number tracking
    incoming_reliable_seq: u16,
    outgoing_reliable_seq: u16,
}

impl PeerEntry {
    fn update_rtt(&mut self, sample: u32) {
        if !self.rtt_initialized {
            self.rtt = sample;
            self.rtt_var = (sample + 1) / 2;
            self.rtt_initialized = true;
        } else {
            self.rtt_var = self.rtt_var.saturating_sub(self.rtt_var / 4);
            if sample >= self.rtt {
                let diff = sample - self.rtt;
                self.rtt_var += diff / 4;
                self.rtt += diff / 8;
            } else {
                let diff = self.rtt - sample;
                self.rtt_var += diff / 4;
                self.rtt = self.rtt.saturating_sub(diff / 8);
            }
        }
    }

    fn initial_rtt_timeout(&self) -> u32 {
        (self.rtt + 4 * self.rtt_var).max(200)
    }

    /// Bump the outgoing connection-level reliable sequence number.
    fn next_connect_seq(&mut self) -> u16 {
        self.outgoing_reliable_seq = self.outgoing_reliable_seq.wrapping_add(1);
        self.outgoing_reliable_seq
    }
}

// ── Host task ─────────────────────────────────────────────────────────────────

pub(crate) struct HostTask {
    socket: std::sync::Arc<UdpSocket>,
    peers: Vec<Option<PeerEntry>>,
    addr_map: HashMap<SocketAddr, PeerKey>,

    accept_tx: mpsc::Sender<(PeerKey, mpsc::Receiver<Packet>)>,
    user_rx: mpsc::Receiver<ToTask>,
    /// Sender for user → task messages, stored so we can hand it to new Peers.
    user_tx: mpsc::Sender<ToTask>,

    host_mtu: u32,
    incoming_bandwidth: u32,
    outgoing_bandwidth: u32,
    use_checksum: bool,
    epoch: Instant,
}

impl HostTask {
    pub fn new(
        socket: std::sync::Arc<UdpSocket>,
        accept_tx: mpsc::Sender<(PeerKey, mpsc::Receiver<Packet>)>,
        user_tx: mpsc::Sender<ToTask>,
        user_rx: mpsc::Receiver<ToTask>,
        max_peers: usize,
        use_checksum: bool,
    ) -> Self {
        let mut peers = Vec::with_capacity(max_peers);
        peers.resize_with(max_peers, || None);
        Self {
            socket,
            peers,
            addr_map: HashMap::new(),
            accept_tx,
            user_rx,
            user_tx,
            host_mtu: MTU_DEFAULT,
            incoming_bandwidth: 0,
            outgoing_bandwidth: 0,
            use_checksum,
            epoch: Instant::now(),
        }
    }

    fn now_ms(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    fn alloc_slot(&self) -> Option<usize> {
        self.peers.iter().position(|s| s.is_none())
    }

    fn peer(&self, key: PeerKey) -> Option<&PeerEntry> {
        self.peers.get(key.0)?.as_ref()
    }

    fn peer_mut(&mut self, key: PeerKey) -> Option<&mut PeerEntry> {
        self.peers.get_mut(key.0)?.as_mut()
    }

    // ── Datagram emission ─────────────────────────────────────────────────────

    async fn emit(&self, peer: &PeerEntry, cmds: &[Bytes], now_ms: u32) {
        if cmds.is_empty() {
            return;
        }
        let datagram = self.make_datagram(peer, cmds, now_ms);
        if let Err(e) = self.socket.send_to(&datagram, peer.addr).await {
            warn!("send_to {}: {}", peer.addr, e);
        }
    }

    fn make_datagram(&self, peer: &PeerEntry, cmds: &[Bytes], now_ms: u32) -> Bytes {
        let mut buf = BytesMut::new();
        let peer_id_raw = HEADER_FLAG_SENT_TIME
            | ((peer.outgoing_session_id as u16 & 0x3) << HEADER_SESSION_SHIFT)
            | (peer.outgoing_peer_id & 0x0FFF);
        buf.extend_from_slice(&peer_id_raw.to_be_bytes());
        buf.extend_from_slice(&(now_ms as u16).to_be_bytes());

        if self.use_checksum {
            let ck_offset = buf.len();
            buf.extend_from_slice(&peer.connect_id.to_be_bytes());
            for c in cmds {
                buf.extend_from_slice(c);
            }
            let ck = protocol::crc32(&buf);
            buf[ck_offset..ck_offset + 4].copy_from_slice(&ck.to_be_bytes());
        } else {
            for c in cmds {
                buf.extend_from_slice(c);
            }
        }
        buf.freeze()
    }

    async fn emit_by_key(&mut self, key: PeerKey, cmds: &[Bytes], now_ms: u32) {
        if cmds.is_empty() {
            return;
        }
        let peer = match self.peer(key) {
            Some(p) => p,
            None => return,
        };
        let datagram = self.make_datagram(peer, cmds, now_ms);
        let addr = peer.addr;
        if let Err(e) = self.socket.send_to(&datagram, addr).await {
            warn!("send_to {}: {}", addr, e);
        }
        if let Some(p) = self.peer_mut(key) {
            p.last_send_time = now_ms;
        }
    }

    // ── Reliable command helper ───────────────────────────────────────────────

    fn make_reliable(
        peer: &mut PeerEntry,
        channel_id: u8,
        cmd_type: u8,
        extra_flags: u8,
        now_ms: u32,
        body: impl FnOnce(&mut BytesMut),
    ) -> Bytes {
        let seq = if channel_id == CHANNEL_ID_CONNECTION {
            peer.next_connect_seq()
        } else {
            peer.channels[channel_id as usize].next_reliable_seq()
        };

        let cmd = encode_command(
            cmd_type,
            COMMAND_FLAG_ACKNOWLEDGE | extra_flags,
            channel_id,
            seq,
            body,
        );

        let timeout = peer.initial_rtt_timeout();
        peer.outgoing_reliable.push(OutstandingReliable {
            reliable_seq: seq,
            channel_id,
            sent_time: now_ms,
            rtt_timeout: timeout,
            send_attempts: 1,
            packet_bytes: cmd.clone(),
        });

        cmd
    }

    // ── ACK helpers ───────────────────────────────────────────────────────────

    fn ack(channel_id: u8, seq: u16, sent_time: u16) -> Bytes {
        encode_command(CMD_ACKNOWLEDGE, 0, channel_id, 0, |buf| {
            buf.extend_from_slice(&seq.to_be_bytes());
            buf.extend_from_slice(&sent_time.to_be_bytes());
        })
    }

    // ── Connection initiation (client) ────────────────────────────────────────

    fn begin_connect(
        &mut self,
        addr: SocketAddr,
        channel_count: u8,
        user_data: u32,
        now_ms: u32,
        reply: tokio::sync::oneshot::Sender<Result<(PeerKey, mpsc::Receiver<Packet>), Error>>,
    ) -> Option<(PeerKey, Bytes)> {
        let slot = self.alloc_slot()?;
        let key = PeerKey(slot);

        let (tx, rx) = mpsc::channel(256);
        let connect_id: u32 = rand::thread_rng().gen();
        let ch = channel_count.max(1);

        let mut entry = PeerEntry {
            key,
            addr,
            state: PeerState::Connecting,
            outgoing_peer_id: PEER_ID_UNASSIGNED,
            connect_id,
            incoming_session_id: 0xFF,
            outgoing_session_id: 0xFF,
            mtu: self.host_mtu,
            window_size: WINDOW_SIZE_MAX,
            channel_count: ch,
            throttle_interval: THROTTLE_INTERVAL_MS,
            throttle_acceleration: THROTTLE_ACCELERATION,
            throttle_deceleration: THROTTLE_DECELERATION,
            rtt: DEFAULT_RTT_MS,
            rtt_var: DEFAULT_RTT_MS / 2,
            rtt_initialized: false,
            last_send_time: now_ms,
            last_receive_time: now_ms,
            channels: (0..ch).map(|_| Channel::new()).collect(),
            unsequenced_window: UnsequencedWindow::new(),
            outgoing_reliable: Vec::new(),
            to_user: tx,
            to_user_rx: Some(rx),
            connect_reply: Some(reply),
            is_incoming: false,
            incoming_reliable_seq: 0,
            outgoing_reliable_seq: 0,
        };

        let connect_id = connect_id;
        let mtu = self.host_mtu;
        let ib = self.incoming_bandwidth;
        let ob = self.outgoing_bandwidth;
        let cc = ch as u32;
        let key_idx = key.0 as u16;

        let cmd = Self::make_reliable(
            &mut entry,
            CHANNEL_ID_CONNECTION,
            CMD_CONNECT,
            0,
            now_ms,
            move |buf| {
                buf.extend_from_slice(&key_idx.to_be_bytes()); // outgoingPeerID
                buf.extend_from_slice(&[0xFF]); // incomingSessionID: request fresh
                buf.extend_from_slice(&[0xFF]); // outgoingSessionID: request fresh
                buf.extend_from_slice(&mtu.to_be_bytes());
                buf.extend_from_slice(&WINDOW_SIZE_MAX.to_be_bytes());
                buf.extend_from_slice(&cc.to_be_bytes());
                buf.extend_from_slice(&ib.to_be_bytes());
                buf.extend_from_slice(&ob.to_be_bytes());
                buf.extend_from_slice(&THROTTLE_INTERVAL_MS.to_be_bytes());
                buf.extend_from_slice(&THROTTLE_ACCELERATION.to_be_bytes());
                buf.extend_from_slice(&THROTTLE_DECELERATION.to_be_bytes());
                buf.extend_from_slice(&connect_id.to_be_bytes());
                buf.extend_from_slice(&user_data.to_be_bytes());
            },
        );

        self.addr_map.insert(addr, key);
        self.peers[slot] = Some(entry);
        Some((key, cmd))
    }

    // ── Incoming datagram ─────────────────────────────────────────────────────

    async fn on_datagram(&mut self, raw: &[u8], from: SocketAddr, now_ms: u32) {
        let mut slice: &[u8] = raw;
        let hdr = match ProtocolHeader::parse(&mut slice) {
            Some(h) => h,
            None => return,
        };

        if self.use_checksum {
            if slice.len() < 4 {
                return;
            }
            slice = &slice[4..];
        }

        let sent_time = hdr.sent_time;
        let peer_id = hdr.peer_id();

        let key = if peer_id == PEER_ID_UNASSIGNED {
            self.addr_map.get(&from).copied()
        } else if (peer_id as usize) < self.peers.len() {
            Some(PeerKey(peer_id as usize))
        } else {
            None
        };

        let cmds = protocol::parse_commands(slice);
        for cmd in cmds {
            self.on_command(key, &hdr, sent_time, cmd, from, now_ms)
                .await;
        }
    }

    async fn on_command(
        &mut self,
        key: Option<PeerKey>,
        _hdr: &ProtocolHeader,
        sent_time: Option<u16>,
        cmd: Command,
        from: SocketAddr,
        now_ms: u32,
    ) {
        match cmd.header.command_type() {
            CMD_CONNECT => {
                if let CommandBody::Connect(c) = cmd.body {
                    self.on_connect(c, cmd.header, from, now_ms).await;
                }
            }
            CMD_VERIFY_CONNECT => {
                if let (Some(k), CommandBody::VerifyConnect(vc)) = (key, cmd.body) {
                    self.on_verify_connect(k, vc, cmd.header, sent_time, now_ms)
                        .await;
                }
            }
            CMD_ACKNOWLEDGE => {
                if let (Some(k), CommandBody::Acknowledge(ack)) = (key, cmd.body) {
                    self.on_ack(k, ack, now_ms);
                }
            }
            CMD_DISCONNECT => {
                if let Some(k) = key {
                    let graceful = cmd.header.has_ack_flag();
                    let st = sent_time.unwrap_or(0);
                    if graceful {
                        let a = Self::ack(cmd.header.channel_id, cmd.header.reliable_seq, st);
                        self.emit_by_key(k, &[a], now_ms).await;
                    }
                    self.remove_peer(k);
                }
            }
            CMD_PING => {
                if let Some(k) = key {
                    if let Some(p) = self.peer_mut(k) {
                        p.last_receive_time = now_ms;
                    }
                    if cmd.header.has_ack_flag() {
                        let st = sent_time.unwrap_or(0);
                        let a =
                            Self::ack(cmd.header.channel_id, cmd.header.reliable_seq, st);
                        self.emit_by_key(k, &[a], now_ms).await;
                    }
                }
            }
            CMD_SEND_RELIABLE => {
                if let (Some(k), CommandBody::SendReliable(sr)) = (key, cmd.body) {
                    let ch = cmd.header.channel_id;
                    let seq = cmd.header.reliable_seq;
                    let st = sent_time.unwrap_or(0);
                    let a = Self::ack(ch, seq, st);
                    self.emit_by_key(k, &[a], now_ms).await;
                    let ready = {
                        let p = match self.peer_mut(k) { Some(p) => p, None => return };
                        p.last_receive_time = now_ms;
                        if ch as usize >= p.channels.len() { return; }
                        p.channels[ch as usize].receive_reliable(seq, sr.data)
                    };
                    for data in ready {
                        self.deliver(k, Packet::reliable(data, ch));
                    }
                }
            }
            CMD_SEND_UNRELIABLE => {
                if let (Some(k), CommandBody::SendUnreliable(su)) = (key, cmd.body) {
                    let ch = cmd.header.channel_id;
                    let accepted = {
                        let p = match self.peer_mut(k) { Some(p) => p, None => return };
                        p.last_receive_time = now_ms;
                        if ch as usize >= p.channels.len() { return; }
                        p.channels[ch as usize].receive_unreliable(su.unreliable_seq, su.data)
                    };
                    if let Some(data) = accepted {
                        self.deliver(k, Packet::unreliable(data, ch));
                    }
                }
            }
            CMD_SEND_FRAGMENT => {
                if let (Some(k), CommandBody::SendFragment(sf)) = (key, cmd.body) {
                    let ch = cmd.header.channel_id;
                    let seq = cmd.header.reliable_seq;
                    let st = sent_time.unwrap_or(0);
                    let a = Self::ack(ch, seq, st);
                    self.emit_by_key(k, &[a], now_ms).await;
                    self.on_fragment(k, ch, sf, true, now_ms);
                }
            }
            CMD_SEND_UNRELIABLE_FRAGMENT => {
                if let (Some(k), CommandBody::SendUnreliableFragment(sf)) = (key, cmd.body) {
                    let ch = cmd.header.channel_id;
                    self.on_fragment(k, ch, sf, false, now_ms);
                }
            }
            CMD_SEND_UNSEQUENCED => {
                if let (Some(k), CommandBody::SendUnsequenced(su)) = (key, cmd.body) {
                    let accepted = {
                        let p = match self.peer_mut(k) { Some(p) => p, None => return };
                        p.last_receive_time = now_ms;
                        p.unsequenced_window.check_and_set(su.unsequenced_group)
                    };
                    if accepted {
                        self.deliver(k, Packet::unsequenced(su.data));
                    }
                }
            }
            _ => {}
        }
    }

    fn on_fragment(
        &mut self,
        key: PeerKey,
        ch: u8,
        sf: crate::protocol::SendFragmentCmd,
        reliable: bool,
        now_ms: u32,
    ) {
        if sf.fragment_count > MAX_FRAGMENT_COUNT {
            return;
        }
        let ready = {
            let p = match self.peer_mut(key) { Some(p) => p, None => return };
            p.last_receive_time = now_ms;
            if ch as usize >= p.channels.len() { return; }

            let buf = p.channels[ch as usize]
                .fragment_buffers
                .entry(sf.start_seq)
                .or_insert_with(|| {
                    FragmentBuffer::new(sf.total_length as usize, sf.fragment_count)
                });

            let done = buf.receive(sf.fragment_number, sf.fragment_offset as usize, &sf.data);
            if done {
                let assembled = p.channels[ch as usize]
                    .fragment_buffers
                    .remove(&sf.start_seq)
                    .unwrap()
                    .finish();
                if reliable {
                    p.channels[ch as usize].receive_reliable(sf.start_seq, assembled)
                } else {
                    vec![assembled]
                }
            } else {
                vec![]
            }
        };

        for data in ready {
            let mode = if reliable { SendMode::Reliable } else { SendMode::Unreliable };
            self.deliver(key, Packet { data, channel: ch, mode });
        }
    }

    fn deliver(&self, key: PeerKey, pkt: Packet) {
        if let Some(p) = self.peer(key) {
            let _ = p.to_user.try_send(pkt);
        }
    }

    // ── CONNECT (server side) ─────────────────────────────────────────────────

    async fn on_connect(
        &mut self,
        connect: ConnectCmd,
        hdr: CommandHeader,
        from: SocketAddr,
        now_ms: u32,
    ) {
        if self.addr_map.contains_key(&from) {
            return;
        }
        let slot = match self.alloc_slot() {
            Some(s) => s,
            None => { warn!("no peer slots"); return; }
        };
        let key = PeerKey(slot);
        let (tx, rx) = mpsc::channel(256);

        let in_sess = if connect.incoming_session_id == 0xFF { 1u8 } else {
            (connect.incoming_session_id.wrapping_add(1)) & 0x3
        };
        let out_sess = if connect.outgoing_session_id == 0xFF { 2u8 } else {
            (connect.outgoing_session_id.wrapping_add(1)) & 0x3
        };

        let mtu = connect.mtu.clamp(MTU_MIN, MTU_MAX).min(self.host_mtu);
        let window_size = connect.window_size.clamp(WINDOW_SIZE_MIN, WINDOW_SIZE_MAX);
        let ch = (connect.channel_count as u8).max(1);

        let mut entry = PeerEntry {
            key,
            addr: from,
            state: PeerState::AcknowledgingConnect,
            outgoing_peer_id: connect.outgoing_peer_id,
            connect_id: connect.connect_id,
            incoming_session_id: in_sess,
            outgoing_session_id: out_sess,
            mtu,
            window_size,
            channel_count: ch,
            throttle_interval: connect.throttle_interval,
            throttle_acceleration: connect.throttle_acceleration,
            throttle_deceleration: connect.throttle_deceleration,
            rtt: DEFAULT_RTT_MS,
            rtt_var: DEFAULT_RTT_MS / 2,
            rtt_initialized: false,
            last_send_time: now_ms,
            last_receive_time: now_ms,
            channels: (0..ch).map(|_| Channel::new()).collect(),
            unsequenced_window: UnsequencedWindow::new(),
            outgoing_reliable: Vec::new(),
            to_user: tx,
            to_user_rx: Some(rx),
            connect_reply: None,
            is_incoming: true,
            incoming_reliable_seq: hdr.reliable_seq,
            outgoing_reliable_seq: 0,
        };

        // ACK for CONNECT
        let ack = Self::ack(CHANNEL_ID_CONNECTION, hdr.reliable_seq, 0);

        // VERIFY_CONNECT
        let assigned_id = key.0 as u16;
        let mtu_ = mtu;
        let ws_ = window_size;
        let cc_ = ch as u32;
        let ib_ = self.incoming_bandwidth;
        let ob_ = self.outgoing_bandwidth;
        let ti_ = entry.throttle_interval;
        let ta_ = entry.throttle_acceleration;
        let td_ = entry.throttle_deceleration;
        let cid_ = connect.connect_id;
        let in_s = in_sess;
        let out_s = out_sess;

        let verify = Self::make_reliable(
            &mut entry,
            CHANNEL_ID_CONNECTION,
            CMD_VERIFY_CONNECT,
            0,
            now_ms,
            move |buf| {
                buf.extend_from_slice(&assigned_id.to_be_bytes());
                buf.extend_from_slice(&[in_s]);
                buf.extend_from_slice(&[out_s]);
                buf.extend_from_slice(&mtu_.to_be_bytes());
                buf.extend_from_slice(&ws_.to_be_bytes());
                buf.extend_from_slice(&cc_.to_be_bytes());
                buf.extend_from_slice(&ib_.to_be_bytes());
                buf.extend_from_slice(&ob_.to_be_bytes());
                buf.extend_from_slice(&ti_.to_be_bytes());
                buf.extend_from_slice(&ta_.to_be_bytes());
                buf.extend_from_slice(&td_.to_be_bytes());
                buf.extend_from_slice(&cid_.to_be_bytes());
            },
        );

        // Immediately transition to Connected and notify accept
        entry.state = PeerState::Connected;
        let rx_for_user = entry.to_user_rx.take().unwrap();

        self.addr_map.insert(from, key);
        self.peers[slot] = Some(entry);

        self.emit_by_key(key, &[ack, verify], now_ms).await;

        // Present to application via accept()
        let _ = self.accept_tx.try_send((key, rx_for_user));
    }

    // ── VERIFY_CONNECT (client side) ──────────────────────────────────────────

    async fn on_verify_connect(
        &mut self,
        key: PeerKey,
        vc: VerifyConnectCmd,
        hdr: CommandHeader,
        sent_time: Option<u16>,
        now_ms: u32,
    ) {
        let peer = match self.peer_mut(key) {
            Some(p) => p,
            None => return,
        };
        if peer.state != PeerState::Connecting || vc.connect_id != peer.connect_id {
            return;
        }

        peer.outgoing_peer_id = vc.outgoing_peer_id;
        peer.incoming_session_id = vc.incoming_session_id;
        peer.outgoing_session_id = vc.outgoing_session_id;
        peer.mtu = vc.mtu;
        peer.window_size = vc.window_size;
        peer.state = PeerState::Connected;
        peer.last_receive_time = now_ms;
        peer.incoming_reliable_seq = hdr.reliable_seq;

        let rx = peer.to_user_rx.take();
        let reply = peer.connect_reply.take();

        // ACK for VERIFY_CONNECT
        let st = sent_time.unwrap_or(0);
        let ack = Self::ack(CHANNEL_ID_CONNECTION, hdr.reliable_seq, st);
        self.emit_by_key(key, &[ack], now_ms).await;

        // Remove CONNECT from the outstanding queue (VERIFY_CONNECT implicitly acks it)
        if let Some(p) = self.peer_mut(key) {
            p.outgoing_reliable.retain(|r| r.channel_id != CHANNEL_ID_CONNECTION || r.reliable_seq != 1);
        }

        if let (Some(rx), Some(tx)) = (rx, reply) {
            let _ = tx.send(Ok((key, rx)));
        }
    }

    // ── ACK processing ────────────────────────────────────────────────────────

    fn on_ack(&mut self, key: PeerKey, ack: AcknowledgeCmd, now_ms: u32) {
        let peer = match self.peer_mut(key) {
            Some(p) => p,
            None => return,
        };
        peer.last_receive_time = now_ms;

        // Reconstruct full 32-bit sent time
        let t16 = ack.received_sent_time;
        let mut t32 = t16 as u32 | (now_ms & 0xFFFF_0000);
        if (t32 & 0x8000) > (now_ms & 0x8000) {
            t32 = t32.wrapping_sub(0x10000);
        }
        if now_ms >= t32 {
            let sample = (now_ms - t32).max(1);
            peer.update_rtt(sample);
        }

        let seq = ack.received_reliable_seq;
        peer.outgoing_reliable.retain(|r| r.reliable_seq != seq);
    }

    // ── Outgoing send ─────────────────────────────────────────────────────────

    async fn send_packet(&mut self, key: PeerKey, pkt: Packet, now_ms: u32) {
        let (mtu, state) = match self.peer(key) {
            Some(p) => (p.mtu, p.state),
            None => return,
        };
        if state != PeerState::Connected {
            return;
        }

        let ck_overhead = if self.use_checksum { 4 } else { 0 };
        // datagram header: 4 bytes (peer_id + sent_time)
        let hdr_overhead = 4 + ck_overhead;

        let ch = pkt.channel;

        match pkt.mode {
            SendMode::Reliable => {
                let cmd_hdr = 4; // command header
                let field_overhead = 2; // dataLength
                let max_payload = mtu as usize - hdr_overhead - cmd_hdr - field_overhead;

                if pkt.data.len() <= max_payload {
                    let data = pkt.data;
                    let data_len = data.len() as u16;
                    let cmd = {
                        let p = self.peer_mut(key).unwrap();
                        Self::make_reliable(p, ch, CMD_SEND_RELIABLE, 0, now_ms, move |buf| {
                            buf.extend_from_slice(&data_len.to_be_bytes());
                            buf.extend_from_slice(&data);
                        })
                    };
                    self.emit_by_key(key, &[cmd], now_ms).await;
                } else {
                    // Fragment (reliable)
                    let frag_fields = 20; // startSeq+dataLen+count+num+total+offset
                    let frag_payload = mtu as usize - hdr_overhead - cmd_hdr - frag_fields;
                    let total = pkt.data.len();
                    let frag_count = (total + frag_payload - 1) / frag_payload;
                    let data = pkt.data;

                    let start_seq = {
                        let p = self.peer_mut(key).unwrap();
                        p.channels[ch as usize].next_reliable_seq()
                    };

                    let mut cmds = Vec::with_capacity(frag_count);
                    for i in 0..frag_count {
                        let offset = i * frag_payload;
                        let end = (offset + frag_payload).min(total);
                        let frag = data.slice(offset..end);
                        let frag_len = frag.len() as u16;

                        let seq = if i == 0 {
                            start_seq
                        } else {
                            let p = self.peer_mut(key).unwrap();
                            p.channels[ch as usize].next_reliable_seq()
                        };

                        let s_seq = start_seq;
                        let fc = frag_count as u32;
                        let fi = i as u32;
                        let tot = total as u32;
                        let fo = offset as u32;
                        let frag2 = frag.clone();

                        let p = self.peer_mut(key).unwrap();
                        let timeout = p.initial_rtt_timeout();
                        let cmd = encode_command(CMD_SEND_FRAGMENT, COMMAND_FLAG_ACKNOWLEDGE, ch, seq, move |buf| {
                            buf.extend_from_slice(&s_seq.to_be_bytes());
                            buf.extend_from_slice(&frag_len.to_be_bytes());
                            buf.extend_from_slice(&fc.to_be_bytes());
                            buf.extend_from_slice(&fi.to_be_bytes());
                            buf.extend_from_slice(&tot.to_be_bytes());
                            buf.extend_from_slice(&fo.to_be_bytes());
                            buf.extend_from_slice(&frag2);
                        });
                        p.outgoing_reliable.push(OutstandingReliable {
                            reliable_seq: seq,
                            channel_id: ch,
                            sent_time: now_ms,
                            rtt_timeout: timeout,
                            send_attempts: 1,
                            packet_bytes: cmd.clone(),
                        });
                        cmds.push(cmd);
                    }
                    self.flush_batch(key, cmds, now_ms).await;
                }
            }

            SendMode::Unreliable => {
                let cmd_hdr = 4;
                let field_overhead = 4; // unreliableSeq + dataLength
                let max_payload = mtu as usize - hdr_overhead - cmd_hdr - field_overhead;

                if pkt.data.len() <= max_payload {
                    let data = pkt.data;
                    let data_len = data.len() as u16;
                    let seq = {
                        let p = self.peer_mut(key).unwrap();
                        p.channels[ch as usize].next_unreliable_seq()
                    };
                    let cmd = encode_command(CMD_SEND_UNRELIABLE, 0, ch, seq, move |buf| {
                        buf.extend_from_slice(&seq.to_be_bytes());
                        buf.extend_from_slice(&data_len.to_be_bytes());
                        buf.extend_from_slice(&data);
                    });
                    self.emit_by_key(key, &[cmd], now_ms).await;
                } else {
                    let frag_fields = 20;
                    let frag_payload = mtu as usize - hdr_overhead - cmd_hdr - frag_fields;
                    let total = pkt.data.len();
                    let frag_count = (total + frag_payload - 1) / frag_payload;
                    let data = pkt.data;
                    let start_seq = {
                        let p = self.peer_mut(key).unwrap();
                        p.channels[ch as usize].next_unreliable_seq()
                    };

                    let mut cmds = Vec::with_capacity(frag_count);
                    for i in 0..frag_count {
                        let offset = i * frag_payload;
                        let end = (offset + frag_payload).min(total);
                        let frag = data.slice(offset..end);
                        let frag_len = frag.len() as u16;
                        let s_seq = start_seq;
                        let fc = frag_count as u32;
                        let fi = i as u32;
                        let tot = total as u32;
                        let fo = offset as u32;
                        let frag2 = frag.clone();
                        let seq = if i == 0 {
                            start_seq
                        } else {
                            let p = self.peer_mut(key).unwrap();
                            p.channels[ch as usize].next_unreliable_seq()
                        };
                        let cmd = encode_command(CMD_SEND_UNRELIABLE_FRAGMENT, 0, ch, seq, move |buf| {
                            buf.extend_from_slice(&s_seq.to_be_bytes());
                            buf.extend_from_slice(&frag_len.to_be_bytes());
                            buf.extend_from_slice(&fc.to_be_bytes());
                            buf.extend_from_slice(&fi.to_be_bytes());
                            buf.extend_from_slice(&tot.to_be_bytes());
                            buf.extend_from_slice(&fo.to_be_bytes());
                            buf.extend_from_slice(&frag2);
                        });
                        cmds.push(cmd);
                    }
                    self.flush_batch(key, cmds, now_ms).await;
                }
            }

            SendMode::Unsequenced => {
                let data = pkt.data;
                let data_len = data.len() as u16;
                let group = {
                    let p = self.peer_mut(key).unwrap();
                    p.channels
                        .get_mut(ch as usize)
                        .map(|c| c.next_unsequenced_group())
                        .unwrap_or(0)
                };
                let cmd = encode_command(CMD_SEND_UNSEQUENCED, COMMAND_FLAG_UNSEQUENCED, ch, 0, move |buf| {
                    buf.extend_from_slice(&group.to_be_bytes());
                    buf.extend_from_slice(&data_len.to_be_bytes());
                    buf.extend_from_slice(&data);
                });
                self.emit_by_key(key, &[cmd], now_ms).await;
            }
        }
    }

    async fn flush_batch(&mut self, key: PeerKey, cmds: Vec<Bytes>, now_ms: u32) {
        let mtu = match self.peer(key) { Some(p) => p.mtu as usize, None => return };
        let hdr = 4 + if self.use_checksum { 4 } else { 0 };
        let mut batch = Vec::new();
        let mut sz = hdr;
        for cmd in cmds {
            if sz + cmd.len() > mtu && !batch.is_empty() {
                self.emit_by_key(key, &batch, now_ms).await;
                batch.clear();
                sz = hdr;
            }
            sz += cmd.len();
            batch.push(cmd);
        }
        if !batch.is_empty() {
            self.emit_by_key(key, &batch, now_ms).await;
        }
    }

    // ── Disconnect ────────────────────────────────────────────────────────────

    async fn send_disconnect(&mut self, key: PeerKey, data: u32, now_ms: u32) {
        let cmd = {
            let p = match self.peer_mut(key) { Some(p) => p, None => return };
            p.state = PeerState::Disconnecting;
            Self::make_reliable(p, CHANNEL_ID_CONNECTION, CMD_DISCONNECT, 0, now_ms, move |buf| {
                buf.extend_from_slice(&data.to_be_bytes());
            })
        };
        self.emit_by_key(key, &[cmd], now_ms).await;
    }

    fn remove_peer(&mut self, key: PeerKey) {
        if let Some(p) = self.peer(key) {
            let addr = p.addr;
            self.addr_map.remove(&addr);
        }
        if let Some(slot) = self.peers.get_mut(key.0) {
            *slot = None;
        }
    }

    // ── Periodic service ──────────────────────────────────────────────────────

    async fn service_all(&mut self, now_ms: u32) {
        let keys: Vec<PeerKey> = self
            .peers
            .iter()
            .enumerate()
            .filter_map(|(i, p)| p.as_ref().map(|_| PeerKey(i)))
            .collect();

        for key in keys {
            self.service_one(key, now_ms).await;
        }
    }

    async fn service_one(&mut self, key: PeerKey, now_ms: u32) {
        let (timed_out, retransmits, ping_needed) = {
            let p = match self.peer_mut(key) { Some(p) => p, None => return };

            let mut timed_out = false;
            let mut retransmits = Vec::new();

            for r in &mut p.outgoing_reliable {
                let elapsed = now_ms.wrapping_sub(r.sent_time);
                if elapsed < r.rtt_timeout {
                    continue;
                }
                let attempts_over = (1u32 << r.send_attempts.saturating_sub(1)) >= TIMEOUT_LIMIT;
                if elapsed >= TIMEOUT_MAXIMUM_MS || (attempts_over && elapsed >= TIMEOUT_MINIMUM_MS) {
                    timed_out = true;
                    break;
                }
                r.send_attempts += 1;
                r.rtt_timeout *= 2;
                r.sent_time = now_ms;
                retransmits.push(r.packet_bytes.clone());
            }

            let ping_needed = p.state == PeerState::Connected
                && p.outgoing_reliable.is_empty()
                && now_ms.wrapping_sub(p.last_send_time) >= DEFAULT_PING_INTERVAL_MS as u32;

            (timed_out, retransmits, ping_needed)
        };

        if timed_out {
            self.remove_peer(key);
            return;
        }

        if !retransmits.is_empty() {
            self.emit_by_key(key, &retransmits, now_ms).await;
        }

        if ping_needed {
            let ping = {
                let p = self.peer_mut(key).unwrap();
                let seq = p.next_connect_seq();
                encode_command(CMD_PING, COMMAND_FLAG_ACKNOWLEDGE, CHANNEL_ID_CONNECTION, seq, |_| {})
            };
            self.emit_by_key(key, &[ping], now_ms).await;
        }
    }

    // ── Main loop ─────────────────────────────────────────────────────────────

    pub async fn run(mut self) {
        let mut buf = vec![0u8; 65536];
        let mut ticker = time::interval(Duration::from_millis(20));

        loop {
            tokio::select! {
                res = self.socket.recv_from(&mut buf) => {
                    match res {
                        Ok((len, from)) => {
                            let now_ms = self.now_ms();
                            // Copy to avoid borrow issues
                            let data = buf[..len].to_vec();
                            self.on_datagram(&data, from, now_ms).await;
                        }
                        Err(e) => warn!("recv_from: {}", e),
                    }
                }

                msg = self.user_rx.recv() => {
                    match msg {
                        None => break,
                        Some(ToTask::Send { peer_key, packet }) => {
                            let now_ms = self.now_ms();
                            self.send_packet(peer_key, packet, now_ms).await;
                        }
                        Some(ToTask::Disconnect { peer_key, data }) => {
                            let now_ms = self.now_ms();
                            self.send_disconnect(peer_key, data, now_ms).await;
                        }
                        Some(ToTask::Connect { addr, channel_count, data, reply }) => {
                            let now_ms = self.now_ms();
                            match self.begin_connect(addr, channel_count, data, now_ms, reply) {
                                Some((key, cmd)) => {
                                    self.emit_by_key(key, &[cmd], now_ms).await;
                                }
                                None => {} // reply already sent Err
                            }
                        }
                    }
                }

                _ = ticker.tick() => {
                    let now_ms = self.now_ms();
                    self.service_all(now_ms).await;
                }
            }
        }
    }
}
