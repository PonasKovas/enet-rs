/// Background I/O task: owns the UDP socket and all peer state machines.
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
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
        CHANNEL_ID_CONNECTION, CMD_ACKNOWLEDGE, CMD_BANDWIDTH_LIMIT, CMD_CONNECT, CMD_DISCONNECT,
        CMD_PING, CMD_SEND_FRAGMENT, CMD_SEND_RELIABLE, CMD_SEND_UNRELIABLE,
        CMD_SEND_UNRELIABLE_FRAGMENT, CMD_SEND_UNSEQUENCED, CMD_THROTTLE_CONFIGURE,
        CMD_VERIFY_CONNECT, COMMAND_FLAG_ACKNOWLEDGE, COMMAND_FLAG_UNSEQUENCED,
        BANDWIDTH_THROTTLE_INTERVAL_MS, DEFAULT_PACKET_THROTTLE, DEFAULT_PING_INTERVAL_MS,
        DEFAULT_RTT_MS, FREE_RELIABLE_WINDOWS, HEADER_FLAG_SENT_TIME, HEADER_PEER_ID_MAX,
        HEADER_SESSION_SHIFT, MAX_FRAGMENT_COUNT, MTU_DEFAULT, MTU_MAX, MTU_MIN,
        PACKET_THROTTLE_SCALE, PEER_ID_UNASSIGNED, RELIABLE_WINDOW_SIZE,
        THROTTLE_ACCELERATION, THROTTLE_DECELERATION, THROTTLE_INTERVAL_MS,
        TIMEOUT_LIMIT, TIMEOUT_MAXIMUM_MS, TIMEOUT_MINIMUM_MS, WINDOW_SIZE_MAX, WINDOW_SIZE_MIN,
    },
};

/// Maximum concurrent in-progress fragment reassemblies per channel.
/// Caps per-channel memory use and prevents fragment-buffer exhaustion (DoS).
const MAX_FRAGMENT_BUFFERS_PER_CHANNEL: usize = 64;

/// Maximum total bytes held in in-progress fragment buffers across all channels
/// for a single peer. Prevents a malicious peer from exhausting host memory by
/// opening many small-fragment reassemblies each claiming a large `total_length`.
const MAX_FRAGMENT_BYTES_PER_PEER: usize = 16 * 1024 * 1024; // 16 MiB

type ConnectReplySender = tokio::sync::oneshot::Sender<Result<(PeerKey, mpsc::Receiver<Packet>), Error>>;

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
        reply: ConnectReplySender,
    },
}

/// Opaque key identifying a peer slot in the task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PeerKey(pub usize);

// ── Peer state ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerState {
    Connecting,
    /// Server side: VERIFY_CONNECT sent, waiting for ACK before exposing to application.
    AcknowledgingConnect,
    Connected,
    /// User called disconnect; draining the reliable-send queue before sending CMD_DISCONNECT.
    DisconnectLater(u32),
    Disconnecting,
}

struct OutstandingReliable {
    reliable_seq: u16,
    channel_id: u8,
    /// Time of the original (first) transmission — used for absolute timeout checks.
    original_sent_time: u32,
    /// Time of the most recent transmission — reset on each retransmit, used for RTT.
    sent_time: u32,
    rtt_timeout: u32,
    send_attempts: u32,
    packet_bytes: Bytes,
}

struct PeerEntry {
    addr: SocketAddr,
    state: PeerState,

    outgoing_peer_id: u16,
    connect_id: u32,
    incoming_session_id: u8,
    outgoing_session_id: u8,

    mtu: u32,
    window_size: u32,
    /// ENet packet throttle (0..=PACKET_THROTTLE_SCALE). Controls how much of window_size
    /// is usable: effective_window = (packet_throttle / scale) * window_size.
    packet_throttle: u32,

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

    /// In-flight reliable packets keyed by (channel_id, reliable_seq).
    /// BTreeMap gives O(log N) ACK removal vs O(N) for Vec::retain.
    outgoing_reliable: BTreeMap<(u8, u16), OutstandingReliable>,

    /// Packets queued because the reliable send window was full at send time.
    /// Drained periodically in service_one once window space frees up.
    pending_send: VecDeque<Packet>,

    /// Total bytes currently committed to in-progress fragment reassembly buffers.
    /// Checked before allocating new buffers to prevent per-peer memory exhaustion.
    fragment_bytes_in_flight: usize,

    /// Timestamp of the start of the current throttle measurement interval.
    packet_throttle_epoch: u32,
    /// RTT snapshot taken at the start of the current throttle interval.
    /// Used to determine if the link has improved or degraded.
    rtt_at_throttle_epoch: u32,

    /// Delivers decoded packets to the user-facing Peer.
    to_user: mpsc::Sender<Packet>,
    /// Held until the connection is established, then given to the user.
    to_user_rx: Option<mpsc::Receiver<Packet>>,

    /// Client-side: reply channel for Host::connect().
    connect_reply: Option<ConnectReplySender>,
    // Connection-level sequence number tracking
    incoming_reliable_seq: u16,
    outgoing_reliable_seq: u16,
}

impl PeerEntry {
    fn update_rtt(&mut self, sample: u32) {
        if !self.rtt_initialized {
            self.rtt = sample;
            self.rtt_var = sample.div_ceil(2);
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

    host_mtu: u32,
    incoming_bandwidth: u32,
    outgoing_bandwidth: u32,
    use_checksum: bool,
    epoch: Instant,
    /// Timestamp of the start of the current bandwidth-throttle measurement window.
    bandwidth_throttle_epoch: u32,
}

impl HostTask {
    pub fn new(
        socket: std::sync::Arc<UdpSocket>,
        accept_tx: mpsc::Sender<(PeerKey, mpsc::Receiver<Packet>)>,
        user_rx: mpsc::Receiver<ToTask>,
        max_peers: usize,
        use_checksum: bool,
        incoming_bandwidth: u32,
        outgoing_bandwidth: u32,
    ) -> Self {
        let mut peers = Vec::with_capacity(max_peers);
        peers.resize_with(max_peers, || None);
        Self {
            socket,
            peers,
            addr_map: HashMap::new(),
            accept_tx,
            user_rx,
            host_mtu: MTU_DEFAULT,
            incoming_bandwidth,
            outgoing_bandwidth,
            use_checksum,
            epoch: Instant::now(),
            bandwidth_throttle_epoch: 0,
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
        peer.outgoing_reliable.insert((channel_id, seq), OutstandingReliable {
            reliable_seq: seq,
            channel_id,
            original_sent_time: now_ms,
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
            addr,
            state: PeerState::Connecting,
            outgoing_peer_id: PEER_ID_UNASSIGNED,
            connect_id,
            incoming_session_id: 0xFF,
            outgoing_session_id: 0xFF,
            mtu: self.host_mtu,
            window_size: WINDOW_SIZE_MAX,
            packet_throttle: DEFAULT_PACKET_THROTTLE,
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
            outgoing_reliable: BTreeMap::new(),
            pending_send: VecDeque::new(),
            fragment_bytes_in_flight: 0,
            packet_throttle_epoch: 0,
            rtt_at_throttle_epoch: DEFAULT_RTT_MS,
            to_user: tx,
            to_user_rx: Some(rx),
            connect_reply: Some(reply),
            incoming_reliable_seq: 0,
            outgoing_reliable_seq: 0,
        };

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

        // Compressed datagrams are not supported; drop them to avoid mis-parsing.
        if hdr.is_compressed() {
            return;
        }

        // Resolve peer key. HEADER_PEER_ID_MAX (0x0FFF) is the wire encoding of
        // PEER_ID_UNASSIGNED (0xFFFF masked to 12 bits) — fall back to address lookup.
        let peer_id = hdr.peer_id();
        let key = if peer_id == HEADER_PEER_ID_MAX {
            self.addr_map.get(&from).copied()
        } else if (peer_id as usize) < self.peers.len() {
            Some(PeerKey(peer_id as usize))
        } else {
            None
        };

        // Validate session ID for known peers (0xFF means not yet established).
        if let Some(k) = key {
            if let Some(peer) = self.peer(k) {
                if peer.incoming_session_id != 0xFF
                    && hdr.session_id() != peer.incoming_session_id
                {
                    return;
                }
            }
        }

        if self.use_checksum {
            if slice.len() < 4 {
                return;
            }
            let stored = u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]);
            // CRC is computed over the datagram with the checksum field replaced by connectID.
            let connect_id = key.and_then(|k| self.peer(k)).map(|p| p.connect_id).unwrap_or(0);
            let hdr_len = raw.len() - slice.len(); // bytes consumed by header
            let mut scratch = Vec::with_capacity(raw.len());
            scratch.extend_from_slice(&raw[..hdr_len]);
            scratch.extend_from_slice(&connect_id.to_be_bytes());
            scratch.extend_from_slice(&slice[4..]);
            if protocol::crc32(&scratch) != stored {
                return;
            }
            slice = &slice[4..];
        }

        let sent_time = hdr.sent_time;
        let cmds = protocol::parse_commands(slice);

        // Collect ACKs during command processing; batch-send at the end.
        // All commands in one datagram originate from the same peer, so one batch suffices.
        let mut pending_acks: Vec<Bytes> = Vec::new();

        for cmd in cmds {
            self.on_command(key, &hdr, sent_time, cmd, from, now_ms, &mut pending_acks)
                .await;
        }

        // Send all accumulated ACKs in a single flush (respects MTU).
        if !pending_acks.is_empty() {
            // For new connections key may still be None; look up by address after on_connect ran.
            let ack_key = key.or_else(|| self.addr_map.get(&from).copied());
            if let Some(k) = ack_key {
                self.flush_batch(k, pending_acks, now_ms).await;
            }
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
        pending_acks: &mut Vec<Bytes>,
    ) {
        match cmd.header.command_type() {
            CMD_CONNECT => {
                if let CommandBody::Connect(c) = cmd.body {
                    // on_connect emits its own ACK bundled with VERIFY_CONNECT.
                    self.on_connect(c, cmd.header, sent_time, from, now_ms).await;
                }
            }
            CMD_VERIFY_CONNECT => {
                if let (Some(k), CommandBody::VerifyConnect(vc)) = (key, cmd.body) {
                    self.on_verify_connect(k, vc, cmd.header, sent_time, now_ms, pending_acks)
                        .await;
                }
            }
            CMD_ACKNOWLEDGE => {
                if let (Some(k), CommandBody::Acknowledge(ack)) = (key, cmd.body) {
                    self.on_ack(k, cmd.header.channel_id, ack, now_ms).await;
                }
            }
            CMD_DISCONNECT => {
                if let Some(k) = key {
                    let graceful = cmd.header.has_ack_flag();
                    if graceful {
                        // Emit ACK immediately before remove_peer destroys the peer state.
                        if let Some(st) = sent_time {
                            let a = Self::ack(cmd.header.channel_id, cmd.header.reliable_seq, st);
                            self.emit_by_key(k, &[a], now_ms).await;
                        }
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
                        if let Some(st) = sent_time {
                            pending_acks.push(Self::ack(cmd.header.channel_id, cmd.header.reliable_seq, st));
                        }
                    }
                }
            }
            CMD_SEND_RELIABLE => {
                if let (Some(k), CommandBody::SendReliable(sr)) = (key, cmd.body) {
                    let ch = cmd.header.channel_id;
                    let seq = cmd.header.reliable_seq;
                    if let Some(st) = sent_time {
                        pending_acks.push(Self::ack(ch, seq, st));
                    }
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
                    // The command header carries the sender's current reliable seq for the channel.
                    // Drop if this unreliable belongs to an older reliable epoch than we've received.
                    let reliable_seq_in_cmd = cmd.header.reliable_seq;
                    let accepted = {
                        let p = match self.peer_mut(k) { Some(p) => p, None => return };
                        p.last_receive_time = now_ms;
                        if ch as usize >= p.channels.len() { return; }
                        if protocol::seq_lt(reliable_seq_in_cmd, p.channels[ch as usize].incoming_reliable_seq) {
                            return;
                        }
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
                    if let Some(st) = sent_time {
                        pending_acks.push(Self::ack(ch, seq, st));
                    }
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
            CMD_BANDWIDTH_LIMIT => {
                if let (Some(k), CommandBody::BandwidthLimit(bl)) = (key, cmd.body) {
                    if let Some(p) = self.peer_mut(k) {
                        // Update the window size based on server's bandwidth constraints.
                        // incoming_bandwidth=0 means unlimited; outgoing_bandwidth=0 means unlimited.
                        if bl.incoming_bandwidth != 0 || bl.outgoing_bandwidth != 0 {
                            let throttle = if bl.incoming_bandwidth == 0 {
                                WINDOW_SIZE_MAX
                            } else {
                                ((bl.incoming_bandwidth / WINDOW_SIZE_MAX) * WINDOW_SIZE_MIN)
                                    .max(WINDOW_SIZE_MIN)
                            };
                            p.window_size = throttle.clamp(WINDOW_SIZE_MIN, WINDOW_SIZE_MAX);
                        }
                    }
                }
            }
            CMD_THROTTLE_CONFIGURE => {
                if let (Some(k), CommandBody::ThrottleConfigure(tc)) = (key, cmd.body) {
                    if let Some(p) = self.peer_mut(k) {
                        p.throttle_interval = tc.throttle_interval;
                        p.throttle_acceleration = tc.throttle_acceleration;
                        p.throttle_deceleration = tc.throttle_deceleration;
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
        // Reject obviously malformed fragments before allocating anything.
        if sf.fragment_count == 0 || sf.fragment_count > MAX_FRAGMENT_COUNT {
            return;
        }
        if sf.total_length as usize > protocol::MAX_PACKET_SIZE {
            return;
        }
        // Fragment offset must be within the declared total length.
        let frag_end = (sf.fragment_offset as usize).saturating_add(sf.data.len());
        if frag_end > sf.total_length as usize {
            return;
        }

        let ready = {
            let p = match self.peer_mut(key) { Some(p) => p, None => return };
            p.last_receive_time = now_ms;
            if ch as usize >= p.channels.len() { return; }

            // DoS guards: check limits before touching the fragment buffer map.
            let is_new = !p.channels[ch as usize].fragment_buffers.contains_key(&sf.start_seq);
            if is_new {
                // Per-channel buffer count limit.
                if p.channels[ch as usize].fragment_buffers.len() >= MAX_FRAGMENT_BUFFERS_PER_CHANNEL {
                    return;
                }
                // Per-peer total memory budget. Prevents opening many reassemblies each
                // claiming a large total_length (e.g. 65 536 × 32 MiB = 2 TiB).
                let new_total = p.fragment_bytes_in_flight.saturating_add(sf.total_length as usize);
                if new_total > MAX_FRAGMENT_BYTES_PER_PEER {
                    return;
                }
                p.fragment_bytes_in_flight = new_total;
            }

            // Receive the fragment; the mutable borrow of channels is scoped here.
            let done = {
                let chan = &mut p.channels[ch as usize];
                let buf = chan.fragment_buffers
                    .entry(sf.start_seq)
                    .or_insert_with(|| FragmentBuffer::new(sf.total_length as usize, sf.fragment_count));
                buf.receive(sf.fragment_number, sf.fragment_offset as usize, &sf.data)
            };

            if done {
                // Release the per-peer memory budget.
                p.fragment_bytes_in_flight =
                    p.fragment_bytes_in_flight.saturating_sub(sf.total_length as usize);

                let frag_count = sf.fragment_count;
                let assembled = p.channels[ch as usize]
                    .fragment_buffers.remove(&sf.start_seq).unwrap().finish();

                // Re-borrow channel now that fragment_buffers entry is removed.
                let chan = &mut p.channels[ch as usize];

                if reliable {
                    // receive_reliable advances incoming_reliable_seq to start_seq, but the
                    // sender consumed frag_count sequence numbers (start_seq..start_seq+frag_count-1).
                    // Advance frag_count-1 more so the next expected seq is start_seq+frag_count,
                    // then drain any packets in the queue that are now reachable.
                    let mut ready = chan.receive_reliable(sf.start_seq, assembled);
                    if !ready.is_empty() && frag_count > 1 {
                        chan.incoming_reliable_seq = chan.incoming_reliable_seq
                            .wrapping_add((frag_count - 1) as u16);
                        loop {
                            let next = chan.incoming_reliable_seq.wrapping_add(1);
                            if let Some(d) = chan.incoming_reliable_queue.remove(&next) {
                                chan.incoming_reliable_seq = next;
                                ready.push(d);
                            } else {
                                break;
                            }
                        }
                    }
                    ready
                } else {
                    // Apply the same sequence ordering check as non-fragmented unreliable packets.
                    match chan.receive_unreliable(sf.start_seq, assembled) {
                        Some(data) => vec![data],
                        None => vec![],
                    }
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
        sent_time: Option<u16>,
        from: SocketAddr,
        now_ms: u32,
    ) {
        if self.addr_map.contains_key(&from) {
            return;
        }
        let slot = match self.alloc_slot() {
            Some(s) => s,
            None => {
                warn!("no peer slots available; rejecting CONNECT from {}", from);
                // Inform the connecting client immediately so it does not time-out waiting.
                self.send_raw_disconnect(&connect, from, now_ms).await;
                return;
            }
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

        // Compute effective window size per spec §"Flow Control and Throttling".
        let server_in = self.incoming_bandwidth;
        let client_in = connect.incoming_bandwidth;
        let server_out = self.outgoing_bandwidth;

        // Server window (based on server's own incomingBandwidth).
        let server_window: u32 = if server_in == 0 {
            WINDOW_SIZE_MAX
        } else {
            ((server_in / WINDOW_SIZE_MAX) * WINDOW_SIZE_MIN).max(WINDOW_SIZE_MIN)
        };

        // Peer window (based on min/max of server's outgoing vs client's incoming).
        let peer_window: u32 = if server_out == 0 && client_in == 0 {
            WINDOW_SIZE_MAX
        } else if server_out == 0 || client_in == 0 {
            ((server_out.max(client_in) / WINDOW_SIZE_MAX) * WINDOW_SIZE_MIN).max(WINDOW_SIZE_MIN)
        } else {
            ((server_out.min(client_in) / WINDOW_SIZE_MAX) * WINDOW_SIZE_MIN).max(WINDOW_SIZE_MIN)
        };

        let window_size = server_window
            .min(peer_window)
            .min(connect.window_size)
            .clamp(WINDOW_SIZE_MIN, WINDOW_SIZE_MAX);
        let ch = (connect.channel_count as u8)
            .max(1)
            .min(protocol::MAX_CHANNELS as u8);

        let mut entry = PeerEntry {
            addr: from,
            state: PeerState::AcknowledgingConnect,
            outgoing_peer_id: connect.outgoing_peer_id,
            connect_id: connect.connect_id,
            incoming_session_id: in_sess,
            outgoing_session_id: out_sess,
            mtu,
            window_size,
            packet_throttle: DEFAULT_PACKET_THROTTLE,
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
            outgoing_reliable: BTreeMap::new(),
            pending_send: VecDeque::new(),
            fragment_bytes_in_flight: 0,
            packet_throttle_epoch: 0,
            rtt_at_throttle_epoch: DEFAULT_RTT_MS,
            to_user: tx,
            to_user_rx: Some(rx),
            connect_reply: None,
            incoming_reliable_seq: hdr.reliable_seq,
            outgoing_reliable_seq: 0,
        };

        // ACK for CONNECT — only generate if sentTime was present (per spec).
        let ack_opt = sent_time.map(|st| Self::ack(CHANNEL_ID_CONNECTION, hdr.reliable_seq, st));

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
                // incomingSessionID = session server will use in its outgoing headers
                buf.extend_from_slice(&[out_s]);
                // outgoingSessionID = session server expects from client's headers
                buf.extend_from_slice(&[in_s]);
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

        // Stay in AcknowledgingConnect until the client ACKs VERIFY_CONNECT.
        entry.state = PeerState::AcknowledgingConnect;

        self.addr_map.insert(from, key);
        self.peers[slot] = Some(entry);

        // Send ACK and VERIFY_CONNECT bundled in one datagram.
        let mut cmds: Vec<Bytes> = Vec::new();
        if let Some(a) = ack_opt { cmds.push(a); }
        cmds.push(verify);
        self.emit_by_key(key, &cmds, now_ms).await;
    }

    // ── VERIFY_CONNECT (client side) ──────────────────────────────────────────

    async fn on_verify_connect(
        &mut self,
        key: PeerKey,
        vc: VerifyConnectCmd,
        hdr: CommandHeader,
        sent_time: Option<u16>,
        now_ms: u32,
        pending_acks: &mut Vec<Bytes>,
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
        peer.throttle_interval = vc.throttle_interval;
        peer.throttle_acceleration = vc.throttle_acceleration;
        peer.throttle_deceleration = vc.throttle_deceleration;
        peer.state = PeerState::Connected;
        peer.last_receive_time = now_ms;
        peer.incoming_reliable_seq = hdr.reliable_seq;

        let rx = peer.to_user_rx.take();
        let reply = peer.connect_reply.take();

        // ACK for VERIFY_CONNECT (batched with any other ACKs from this datagram).
        let st = sent_time.unwrap_or(0);
        pending_acks.push(Self::ack(CHANNEL_ID_CONNECTION, hdr.reliable_seq, st));

        // Remove CONNECT from the outstanding queue (VERIFY_CONNECT implicitly acks it).
        if let Some(p) = self.peer_mut(key) {
            p.outgoing_reliable.remove(&(CHANNEL_ID_CONNECTION, 1));
        }

        if let (Some(rx), Some(tx)) = (rx, reply) {
            let _ = tx.send(Ok((key, rx)));
        }
    }

    // ── ACK processing ────────────────────────────────────────────────────────

    async fn on_ack(&mut self, key: PeerKey, channel_id: u8, ack: AcknowledgeCmd, now_ms: u32) {
        let (was_acking_connect, rx_for_user, should_disconnect) = {
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

            // ── Adaptive throttle (ENet spec §"Packet Throttle") ──────────────
            // Once per throttle_interval, compare current RTT to the snapshot taken
            // at the start of the interval. If the link improved, increase the
            // packet throttle; if it degraded significantly, decrease it.
            if peer.packet_throttle_epoch == 0
                || now_ms.wrapping_sub(peer.packet_throttle_epoch) >= peer.throttle_interval
            {
                let prev_rtt = peer.rtt_at_throttle_epoch;
                peer.packet_throttle_epoch = now_ms;
                peer.rtt_at_throttle_epoch = peer.rtt;

                if prev_rtt > 0 && peer.rtt_initialized {
                    if peer.rtt < prev_rtt {
                        // Link improved — accelerate.
                        peer.packet_throttle = (peer.packet_throttle + peer.throttle_acceleration)
                            .min(PACKET_THROTTLE_SCALE);
                    } else if peer.rtt > prev_rtt.saturating_add(2 * peer.rtt_var) {
                        // Link significantly worsened — decelerate.
                        peer.packet_throttle = peer.packet_throttle
                            .saturating_sub(peer.throttle_deceleration);
                    }
                    // Else RTT is stable — leave throttle unchanged.
                }
            }

            let seq = ack.received_reliable_seq;
            // O(log N) removal: BTreeMap keyed by (channel_id, reliable_seq).
            peer.outgoing_reliable.remove(&(channel_id, seq));

            // Detect completion of the server-side 3-way handshake.
            // VERIFY_CONNECT is always on CHANNEL_ID_CONNECTION with seq=1.
            let completing_handshake = peer.state == PeerState::AcknowledgingConnect
                && channel_id == CHANNEL_ID_CONNECTION
                && seq == 1;

            // Detect graceful disconnect completion: CMD_DISCONNECT was ACKed and queue is empty.
            let disconnecting_done = peer.state == PeerState::Disconnecting
                && peer.outgoing_reliable.is_empty();

            if completing_handshake {
                peer.state = PeerState::Connected;
                let rx = peer.to_user_rx.take();
                (true, rx, false)
            } else if disconnecting_done {
                (false, None, true)
            } else {
                (false, None, false)
            }
        };

        if should_disconnect {
            // CMD_DISCONNECT was ACKed; clean up the peer entry.
            self.remove_peer(key);
        } else if was_acking_connect {
            if let Some(rx) = rx_for_user {
                let _ = self.accept_tx.try_send((key, rx));
            }
        }
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
                // Enforce the per-channel reliable send window using sequence-number difference.
                // Counting in-flight packets is wrong under wrap-around: seq 65537 collides with
                // seq 1 if the count check passes but the seq-diff check would not.
                {
                    let p = match self.peer_mut(key) { Some(p) => p, None => return };
                    if ch as usize >= p.channels.len() { return; }

                    let next_seq = p.channels[ch as usize].outgoing_reliable_seq.wrapping_add(1);
                    let oldest = p.outgoing_reliable.values()
                        .filter(|r| r.channel_id == ch)
                        .map(|r| r.reliable_seq)
                        .reduce(|a, b| if protocol::seq_lt(a, b) { a } else { b });

                    // Effective window accounts for the negotiated packet_throttle.
                    let effective_window = ((p.packet_throttle as u64 * p.window_size as u64)
                        / PACKET_THROTTLE_SCALE as u64)
                        .min(FREE_RELIABLE_WINDOWS as u64 * RELIABLE_WINDOW_SIZE as u64)
                        as u32;

                    let window_open = match oldest {
                        None => true,
                        Some(oldest_seq) => {
                            let diff = next_seq.wrapping_sub(oldest_seq) as u32;
                            diff < effective_window
                        }
                    };
                    if !window_open {
                        // Queue for later instead of silently dropping (reliability guarantee).
                        p.pending_send.push_back(pkt);
                        return;
                    }
                }

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
                    let frag_count = total.div_ceil(frag_payload);
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
                        p.outgoing_reliable.insert((ch, seq), OutstandingReliable {
                            reliable_seq: seq,
                            channel_id: ch,
                            original_sent_time: now_ms,
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
                    let (unreliable_seq, reliable_seq) = {
                        let p = self.peer_mut(key).unwrap();
                        let u = p.channels[ch as usize].next_unreliable_seq();
                        let r = p.channels[ch as usize].outgoing_reliable_seq;
                        (u, r)
                    };
                    // Per spec: command header carries the channel's current reliable seq;
                    // the unreliable sequence number goes in the payload.
                    let cmd = encode_command(CMD_SEND_UNRELIABLE, 0, ch, reliable_seq, move |buf| {
                        buf.extend_from_slice(&unreliable_seq.to_be_bytes());
                        buf.extend_from_slice(&data_len.to_be_bytes());
                        buf.extend_from_slice(&data);
                    });
                    self.emit_by_key(key, &[cmd], now_ms).await;
                } else {
                    let frag_fields = 20;
                    let frag_payload = mtu as usize - hdr_overhead - cmd_hdr - frag_fields;
                    let total = pkt.data.len();
                    let frag_count = total.div_ceil(frag_payload);
                    let data = pkt.data;
                    // start_seq is the unreliable sequence of the first fragment (grouping key).
                    let (start_seq, reliable_seq) = {
                        let p = self.peer_mut(key).unwrap();
                        let u = p.channels[ch as usize].next_unreliable_seq();
                        let r = p.channels[ch as usize].outgoing_reliable_seq;
                        (u, r)
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
                        // Header carries the channel reliable seq; payload start_seq is the
                        // first fragment's unreliable seq used as the assembly group key.
                        let cmd = encode_command(CMD_SEND_UNRELIABLE_FRAGMENT, 0, ch, reliable_seq, move |buf| {
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

    /// Send a bare CMD_DISCONNECT datagram to `addr` without needing a peer slot.
    /// Used to reject incoming CONNECT requests when no slot is available.
    async fn send_raw_disconnect(&self, connect: &ConnectCmd, addr: SocketAddr, now_ms: u32) {
        let mut buf = BytesMut::new();
        // Datagram header: use the client's outgoing_peer_id so it recognises the packet.
        let peer_id_raw = HEADER_FLAG_SENT_TIME
            | (connect.outgoing_peer_id & 0x0FFF);
        buf.extend_from_slice(&peer_id_raw.to_be_bytes());
        buf.extend_from_slice(&(now_ms as u16).to_be_bytes());
        // CMD_DISCONNECT, seq 0, no data.
        buf.extend_from_slice(&encode_command(CMD_DISCONNECT, 0, CHANNEL_ID_CONNECTION, 0, |b| {
            b.extend_from_slice(&0u32.to_be_bytes());
        }));
        let _ = self.socket.send_to(&buf, addr).await;
    }

    async fn send_disconnect(&mut self, key: PeerKey, data: u32, now_ms: u32) {
        // If reliable packets are still queued, defer until the queue drains.
        {
            let p = match self.peer_mut(key) { Some(p) => p, None => return };
            // Idempotent: ignore if already in the disconnect process.
            if matches!(p.state, PeerState::Disconnecting | PeerState::DisconnectLater(_)) {
                return;
            }
            if !p.outgoing_reliable.is_empty() {
                p.state = PeerState::DisconnectLater(data);
                return;
            }
            p.state = PeerState::Disconnecting;
        }
        let cmd = {
            let p = match self.peer_mut(key) { Some(p) => p, None => return };
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

        self.bandwidth_throttle(now_ms).await;
    }

    /// Enforce host-wide outgoing/incoming bandwidth limits.
    ///
    /// Runs every [`BANDWIDTH_THROTTLE_INTERVAL_MS`] and:
    /// 1. Proportionally shrinks each peer's effective send window so the sum
    ///    of all peer windows does not exceed the host's `outgoing_bandwidth`.
    /// 2. Sends CMD_BANDWIDTH_LIMIT to every connected peer advertising our
    ///    `incoming_bandwidth` so they throttle how much they send to us.
    async fn bandwidth_throttle(&mut self, now_ms: u32) {
        if now_ms.wrapping_sub(self.bandwidth_throttle_epoch) < BANDWIDTH_THROTTLE_INTERVAL_MS as u32 {
            return;
        }
        self.bandwidth_throttle_epoch = now_ms;

        if self.incoming_bandwidth == 0 && self.outgoing_bandwidth == 0 {
            return;
        }

        let connected_keys: Vec<PeerKey> = self.peers.iter().enumerate()
            .filter_map(|(i, p)| {
                p.as_ref().and_then(|e| {
                    if e.state == PeerState::Connected { Some(PeerKey(i)) } else { None }
                })
            })
            .collect();

        let peer_count = connected_keys.len() as u32;
        if peer_count == 0 {
            return;
        }

        // Adjust per-peer window sizes to distribute our outgoing bandwidth budget.
        if self.outgoing_bandwidth > 0 {
            let per_peer = (self.outgoing_bandwidth / peer_count).clamp(WINDOW_SIZE_MIN, WINDOW_SIZE_MAX);
            for &key in &connected_keys {
                if let Some(p) = self.peer_mut(key) {
                    p.window_size = per_peer;
                }
            }
        }

        // Notify each peer of our bandwidth limits via CMD_BANDWIDTH_LIMIT.
        // incoming_bandwidth tells the peer how fast it may send to us.
        // outgoing_bandwidth / N is what we can send to each peer.
        let ib = self.incoming_bandwidth;
        let ob = if self.outgoing_bandwidth > 0 {
            self.outgoing_bandwidth / peer_count
        } else {
            0
        };

        if ib == 0 && ob == 0 {
            return;
        }

        for key in connected_keys {
            let cmd = {
                let p = match self.peer_mut(key) { Some(p) => p, None => continue };
                Self::make_reliable(p, CHANNEL_ID_CONNECTION, CMD_BANDWIDTH_LIMIT, 0, now_ms, move |buf| {
                    buf.extend_from_slice(&ib.to_be_bytes());
                    buf.extend_from_slice(&ob.to_be_bytes());
                })
            };
            self.emit_by_key(key, &[cmd], now_ms).await;
        }
    }

    async fn service_one(&mut self, key: PeerKey, now_ms: u32) {
        let (timed_out, retransmits, ping_needed, disconnect_later) = {
            let p = match self.peer_mut(key) { Some(p) => p, None => return };

            let mut timed_out = false;
            let mut retransmits = Vec::new();

            for r in p.outgoing_reliable.values_mut() {
                let elapsed_since_last = now_ms.wrapping_sub(r.sent_time);
                if elapsed_since_last < r.rtt_timeout {
                    continue;
                }
                // Use original_sent_time for the hard timeout deadlines so that
                // resetting sent_time on retransmit doesn't prevent 30 s max timeout.
                let elapsed = now_ms.wrapping_sub(r.original_sent_time);
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

            // Fire deferred disconnect once the reliable queue is drained.
            let disconnect_later = if let PeerState::DisconnectLater(data) = p.state {
                if p.outgoing_reliable.is_empty() { Some(data) } else { None }
            } else {
                None
            };

            (timed_out, retransmits, ping_needed, disconnect_later)
        };

        if timed_out {
            self.remove_peer(key);
            return;
        }

        // Use flush_batch for retransmits to respect MTU and avoid EMSGSIZE errors.
        // Previously emit_by_key was used here, which could concatenate dozens of
        // retransmit packets into a single oversized datagram.
        if !retransmits.is_empty() {
            self.flush_batch(key, retransmits, now_ms).await;
        }

        if ping_needed {
            let ping = {
                let p = self.peer_mut(key).unwrap();
                let seq = p.next_connect_seq();
                encode_command(CMD_PING, COMMAND_FLAG_ACKNOWLEDGE, CHANNEL_ID_CONNECTION, seq, |_| {})
            };
            self.emit_by_key(key, &[ping], now_ms).await;
        }

        if let Some(data) = disconnect_later {
            // Queue is now empty — proceed with the actual disconnect.
            {
                let p = match self.peer_mut(key) { Some(p) => p, None => return };
                p.state = PeerState::Disconnecting;
            }
            let cmd = {
                let p = match self.peer_mut(key) { Some(p) => p, None => return };
                Self::make_reliable(p, CHANNEL_ID_CONNECTION, CMD_DISCONNECT, 0, now_ms, move |buf| {
                    buf.extend_from_slice(&data.to_be_bytes());
                })
            };
            self.emit_by_key(key, &[cmd], now_ms).await;
        }

        // Drain packets that were queued because the reliable window was full.
        // Now that some in-flight packets may have been ACKed, try to resend them.
        let pending: Vec<Packet> = match self.peer_mut(key) {
            Some(p) => p.pending_send.drain(..).collect(),
            None => return,
        };
        for pkt in pending {
            self.send_packet(key, pkt, now_ms).await;
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
                            if let Some((key, cmd)) = self.begin_connect(addr, channel_count, data, now_ms, reply) {
                                // reply already sent Err if None
                                self.emit_by_key(key, &[cmd], now_ms).await;
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
