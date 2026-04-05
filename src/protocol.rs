//! ENet protocol constants and packet parsing/serialization.
// Protocol constants and helper methods are public API surface; many are unused
// within this crate but document the protocol and may be used by callers.
#![allow(dead_code)]
use bytes::{BufMut, Bytes, BytesMut};

// ── Header flags / masks ──────────────────────────────────────────────────────
pub const HEADER_FLAG_SENT_TIME: u16 = 0x8000;
pub const HEADER_FLAG_COMPRESSED: u16 = 0x4000;
pub const HEADER_SESSION_MASK: u16 = 0x3000;
pub const HEADER_SESSION_SHIFT: u16 = 12;
pub const HEADER_PEER_ID_MAX: u16 = 0x0FFF;
pub const PEER_ID_UNASSIGNED: u16 = 0xFFFF;

// ── Command flags ─────────────────────────────────────────────────────────────
pub const COMMAND_FLAG_ACKNOWLEDGE: u8 = 0x80;
pub const COMMAND_FLAG_UNSEQUENCED: u8 = 0x40;
pub const COMMAND_MASK: u8 = 0x0F;

// ── Command type IDs ──────────────────────────────────────────────────────────
pub const CMD_NONE: u8 = 0;
pub const CMD_ACKNOWLEDGE: u8 = 1;
pub const CMD_CONNECT: u8 = 2;
pub const CMD_VERIFY_CONNECT: u8 = 3;
pub const CMD_DISCONNECT: u8 = 4;
pub const CMD_PING: u8 = 5;
pub const CMD_SEND_RELIABLE: u8 = 6;
pub const CMD_SEND_UNRELIABLE: u8 = 7;
pub const CMD_SEND_FRAGMENT: u8 = 8;
pub const CMD_SEND_UNSEQUENCED: u8 = 9;
pub const CMD_BANDWIDTH_LIMIT: u8 = 10;
pub const CMD_THROTTLE_CONFIGURE: u8 = 11;
pub const CMD_SEND_UNRELIABLE_FRAGMENT: u8 = 12;

pub const CHANNEL_ID_CONNECTION: u8 = 0xFF;

// ── Protocol limits ───────────────────────────────────────────────────────────
pub const MTU_MIN: u32 = 576;
pub const MTU_MAX: u32 = 4096;
pub const MTU_DEFAULT: u32 = 1392;
pub const WINDOW_SIZE_MIN: u32 = 4096;
pub const WINDOW_SIZE_MAX: u32 = 65536;
pub const MAX_CHANNELS: u32 = 255;
pub const MAX_PEERS: usize = 4095;
pub const MAX_COMMANDS_PER_PACKET: usize = 32;
pub const MAX_FRAGMENT_COUNT: u32 = 1_048_576;
pub const MAX_PACKET_SIZE: usize = 32 * 1024 * 1024; // 32 MiB – hard cap on reassembled size
pub const RELIABLE_WINDOW_SIZE: u16 = 4096;
pub const RELIABLE_WINDOWS: u16 = 16;
pub const FREE_RELIABLE_WINDOWS: u16 = 8;
pub const UNSEQUENCED_WINDOW_SIZE: u32 = 1024;
pub const UNSEQUENCED_WINDOWS: u32 = 64;
pub const FREE_UNSEQUENCED_WINDOWS: u32 = 32;

pub const DEFAULT_PING_INTERVAL_MS: u64 = 500;
pub const DEFAULT_RTT_MS: u32 = 500;
pub const TIMEOUT_MINIMUM_MS: u32 = 5_000;
pub const TIMEOUT_MAXIMUM_MS: u32 = 30_000;
pub const TIMEOUT_LIMIT: u32 = 32;
pub const DEFAULT_PACKET_THROTTLE: u32 = 32;
pub const PACKET_THROTTLE_SCALE: u32 = 32;
pub const THROTTLE_INTERVAL_MS: u32 = 5_000;
pub const THROTTLE_ACCELERATION: u32 = 2;
pub const THROTTLE_DECELERATION: u32 = 2;
pub const BANDWIDTH_THROTTLE_INTERVAL_MS: u64 = 1_000;

// ── Sizes ─────────────────────────────────────────────────────────────────────
pub const HEADER_SIZE: usize = 2;
pub const HEADER_SIZE_WITH_SENT_TIME: usize = 4;
pub const CHECKSUM_SIZE: usize = 4;
pub const COMMAND_HEADER_SIZE: usize = 4;

// ── Protocol header ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProtocolHeader {
    /// Encodes: flags(2) | session(2) | peer_id(12)
    pub peer_id_raw: u16,
    /// Present when HEADER_FLAG_SENT_TIME is set
    pub sent_time: Option<u16>,
}

impl ProtocolHeader {
    pub fn new(peer_id: u16, session_id: u8, flags: u16, sent_time: Option<u16>) -> Self {
        let raw = flags
            | ((session_id as u16 & 0x3) << HEADER_SESSION_SHIFT)
            | (peer_id & HEADER_PEER_ID_MAX);
        Self {
            peer_id_raw: raw,
            sent_time,
        }
    }

    pub fn peer_id(&self) -> u16 {
        self.peer_id_raw & HEADER_PEER_ID_MAX
    }

    pub fn session_id(&self) -> u8 {
        ((self.peer_id_raw & HEADER_SESSION_MASK) >> HEADER_SESSION_SHIFT) as u8
    }

    pub fn is_compressed(&self) -> bool {
        self.peer_id_raw & HEADER_FLAG_COMPRESSED != 0
    }

    pub fn has_sent_time(&self) -> bool {
        self.peer_id_raw & HEADER_FLAG_SENT_TIME != 0
    }

    pub fn byte_size(&self) -> usize {
        if self.sent_time.is_some() {
            HEADER_SIZE_WITH_SENT_TIME
        } else {
            HEADER_SIZE
        }
    }

    pub fn parse(buf: &mut &[u8]) -> Option<Self> {
        if buf.len() < HEADER_SIZE {
            return None;
        }
        let peer_id_raw = u16::from_be_bytes([buf[0], buf[1]]);
        *buf = &buf[2..];

        let sent_time = if peer_id_raw & HEADER_FLAG_SENT_TIME != 0 {
            if buf.len() < 2 {
                return None;
            }
            let t = u16::from_be_bytes([buf[0], buf[1]]);
            *buf = &buf[2..];
            Some(t)
        } else {
            None
        };

        Some(Self {
            peer_id_raw,
            sent_time,
        })
    }

    pub fn write(&self, out: &mut BytesMut) {
        out.put_u16(self.peer_id_raw);
        if let Some(t) = self.sent_time {
            out.put_u16(t);
        }
    }
}

// ── Command header ────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CommandHeader {
    /// Low 4 bits = type; bits 6-7 = flags
    pub command: u8,
    pub channel_id: u8,
    pub reliable_seq: u16,
}

impl CommandHeader {
    pub fn command_type(&self) -> u8 {
        self.command & COMMAND_MASK
    }

    pub fn has_ack_flag(&self) -> bool {
        self.command & COMMAND_FLAG_ACKNOWLEDGE != 0
    }

    pub fn has_unsequenced_flag(&self) -> bool {
        self.command & COMMAND_FLAG_UNSEQUENCED != 0
    }

    pub fn parse(buf: &mut &[u8]) -> Option<Self> {
        if buf.len() < COMMAND_HEADER_SIZE {
            return None;
        }
        let hdr = Self {
            command: buf[0],
            channel_id: buf[1],
            reliable_seq: u16::from_be_bytes([buf[2], buf[3]]),
        };
        *buf = &buf[COMMAND_HEADER_SIZE..];
        Some(hdr)
    }

    pub fn write(&self, out: &mut BytesMut) {
        out.put_u8(self.command);
        out.put_u8(self.channel_id);
        out.put_u16(self.reliable_seq);
    }
}

// ── Full commands ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AcknowledgeCmd {
    pub received_reliable_seq: u16,
    pub received_sent_time: u16,
}

#[derive(Debug, Clone)]
pub struct ConnectCmd {
    pub outgoing_peer_id: u16,
    pub incoming_session_id: u8,
    pub outgoing_session_id: u8,
    pub mtu: u32,
    pub window_size: u32,
    pub channel_count: u32,
    pub incoming_bandwidth: u32,
    pub outgoing_bandwidth: u32,
    pub throttle_interval: u32,
    pub throttle_acceleration: u32,
    pub throttle_deceleration: u32,
    pub connect_id: u32,
    pub data: u32,
}

#[derive(Debug, Clone)]
pub struct VerifyConnectCmd {
    pub outgoing_peer_id: u16,
    pub incoming_session_id: u8,
    pub outgoing_session_id: u8,
    pub mtu: u32,
    pub window_size: u32,
    pub channel_count: u32,
    pub incoming_bandwidth: u32,
    pub outgoing_bandwidth: u32,
    pub throttle_interval: u32,
    pub throttle_acceleration: u32,
    pub throttle_deceleration: u32,
    pub connect_id: u32,
}

#[derive(Debug, Clone)]
pub struct DisconnectCmd {
    pub data: u32,
}

#[derive(Debug, Clone)]
pub struct SendReliableCmd {
    pub data: Bytes,
}

#[derive(Debug, Clone)]
pub struct SendUnreliableCmd {
    pub unreliable_seq: u16,
    pub data: Bytes,
}

#[derive(Debug, Clone)]
pub struct SendFragmentCmd {
    pub start_seq: u16,
    pub fragment_count: u32,
    pub fragment_number: u32,
    pub total_length: u32,
    pub fragment_offset: u32,
    pub data: Bytes,
}

#[derive(Debug, Clone)]
pub struct SendUnsequencedCmd {
    pub unsequenced_group: u16,
    pub data: Bytes,
}

#[derive(Debug, Clone)]
pub struct BandwidthLimitCmd {
    pub incoming_bandwidth: u32,
    pub outgoing_bandwidth: u32,
}

#[derive(Debug, Clone)]
pub struct ThrottleConfigureCmd {
    pub throttle_interval: u32,
    pub throttle_acceleration: u32,
    pub throttle_deceleration: u32,
}

#[derive(Debug, Clone)]
pub enum CommandBody {
    Acknowledge(AcknowledgeCmd),
    Connect(ConnectCmd),
    VerifyConnect(VerifyConnectCmd),
    Disconnect(DisconnectCmd),
    Ping,
    SendReliable(SendReliableCmd),
    SendUnreliable(SendUnreliableCmd),
    SendFragment(SendFragmentCmd),
    SendUnsequenced(SendUnsequencedCmd),
    BandwidthLimit(BandwidthLimitCmd),
    ThrottleConfigure(ThrottleConfigureCmd),
    SendUnreliableFragment(SendFragmentCmd),
}

#[derive(Debug, Clone)]
pub struct Command {
    pub header: CommandHeader,
    pub body: CommandBody,
}

impl Command {
    /// Total serialized size of this command (header + body).
    pub fn byte_size(&self) -> usize {
        COMMAND_HEADER_SIZE
            + match &self.body {
                CommandBody::Acknowledge(_) => 4,
                CommandBody::Connect(_) => 44,
                CommandBody::VerifyConnect(_) => 40,
                CommandBody::Disconnect(_) => 4,
                CommandBody::Ping => 0,
                CommandBody::SendReliable(c) => 2 + c.data.len(),
                CommandBody::SendUnreliable(c) => 4 + c.data.len(),
                CommandBody::SendFragment(c) | CommandBody::SendUnreliableFragment(c) => {
                    20 + c.data.len()
                }
                CommandBody::SendUnsequenced(c) => 4 + c.data.len(),
                CommandBody::BandwidthLimit(_) => 8,
                CommandBody::ThrottleConfigure(_) => 12,
            }
    }

    pub fn parse(buf: &mut &[u8]) -> Option<Self> {
        let hdr = CommandHeader::parse(buf)?;
        let body = match hdr.command_type() {
            CMD_ACKNOWLEDGE => {
                if buf.len() < 4 {
                    return None;
                }
                let c = AcknowledgeCmd {
                    received_reliable_seq: u16::from_be_bytes([buf[0], buf[1]]),
                    received_sent_time: u16::from_be_bytes([buf[2], buf[3]]),
                };
                *buf = &buf[4..];
                CommandBody::Acknowledge(c)
            }
            CMD_CONNECT => {
                if buf.len() < 44 {
                    return None;
                }
                let c = ConnectCmd {
                    outgoing_peer_id: u16::from_be_bytes([buf[0], buf[1]]),
                    incoming_session_id: buf[2],
                    outgoing_session_id: buf[3],
                    mtu: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
                    window_size: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
                    channel_count: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
                    incoming_bandwidth: u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]),
                    outgoing_bandwidth: u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]),
                    throttle_interval: u32::from_be_bytes([buf[24], buf[25], buf[26], buf[27]]),
                    throttle_acceleration: u32::from_be_bytes([buf[28], buf[29], buf[30], buf[31]]),
                    throttle_deceleration: u32::from_be_bytes([buf[32], buf[33], buf[34], buf[35]]),
                    connect_id: u32::from_be_bytes([buf[36], buf[37], buf[38], buf[39]]),
                    data: u32::from_be_bytes([buf[40], buf[41], buf[42], buf[43]]),
                };
                *buf = &buf[44..];
                CommandBody::Connect(c)
            }
            CMD_VERIFY_CONNECT => {
                if buf.len() < 40 {
                    return None;
                }
                let c = VerifyConnectCmd {
                    outgoing_peer_id: u16::from_be_bytes([buf[0], buf[1]]),
                    incoming_session_id: buf[2],
                    outgoing_session_id: buf[3],
                    mtu: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
                    window_size: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
                    channel_count: u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]),
                    incoming_bandwidth: u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]),
                    outgoing_bandwidth: u32::from_be_bytes([buf[20], buf[21], buf[22], buf[23]]),
                    throttle_interval: u32::from_be_bytes([buf[24], buf[25], buf[26], buf[27]]),
                    throttle_acceleration: u32::from_be_bytes([buf[28], buf[29], buf[30], buf[31]]),
                    throttle_deceleration: u32::from_be_bytes([buf[32], buf[33], buf[34], buf[35]]),
                    connect_id: u32::from_be_bytes([buf[36], buf[37], buf[38], buf[39]]),
                };
                *buf = &buf[40..];
                CommandBody::VerifyConnect(c)
            }
            CMD_DISCONNECT => {
                if buf.len() < 4 {
                    return None;
                }
                let c = DisconnectCmd {
                    data: u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
                };
                *buf = &buf[4..];
                CommandBody::Disconnect(c)
            }
            CMD_PING => CommandBody::Ping,
            CMD_SEND_RELIABLE => {
                if buf.len() < 2 {
                    return None;
                }
                let data_length = u16::from_be_bytes([buf[0], buf[1]]) as usize;
                *buf = &buf[2..];
                if buf.len() < data_length {
                    return None;
                }
                let data = Bytes::copy_from_slice(&buf[..data_length]);
                *buf = &buf[data_length..];
                CommandBody::SendReliable(SendReliableCmd { data })
            }
            CMD_SEND_UNRELIABLE => {
                if buf.len() < 4 {
                    return None;
                }
                let unreliable_seq = u16::from_be_bytes([buf[0], buf[1]]);
                let data_length = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                *buf = &buf[4..];
                if buf.len() < data_length {
                    return None;
                }
                let data = Bytes::copy_from_slice(&buf[..data_length]);
                *buf = &buf[data_length..];
                CommandBody::SendUnreliable(SendUnreliableCmd { unreliable_seq, data })
            }
            CMD_SEND_FRAGMENT | CMD_SEND_UNRELIABLE_FRAGMENT => {
                if buf.len() < 20 {
                    return None;
                }
                let start_seq = u16::from_be_bytes([buf[0], buf[1]]);
                let data_length = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                let fragment_count = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                let fragment_number = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let total_length = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
                let fragment_offset = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]);
                *buf = &buf[20..];
                if buf.len() < data_length {
                    return None;
                }
                let data = Bytes::copy_from_slice(&buf[..data_length]);
                *buf = &buf[data_length..];
                let frag = SendFragmentCmd {
                    start_seq,
                    fragment_count,
                    fragment_number,
                    total_length,
                    fragment_offset,
                    data,
                };
                if hdr.command_type() == CMD_SEND_FRAGMENT {
                    CommandBody::SendFragment(frag)
                } else {
                    CommandBody::SendUnreliableFragment(frag)
                }
            }
            CMD_SEND_UNSEQUENCED => {
                if buf.len() < 4 {
                    return None;
                }
                let unsequenced_group = u16::from_be_bytes([buf[0], buf[1]]);
                let data_length = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                *buf = &buf[4..];
                if buf.len() < data_length {
                    return None;
                }
                let data = Bytes::copy_from_slice(&buf[..data_length]);
                *buf = &buf[data_length..];
                CommandBody::SendUnsequenced(SendUnsequencedCmd { unsequenced_group, data })
            }
            CMD_BANDWIDTH_LIMIT => {
                if buf.len() < 8 {
                    return None;
                }
                let c = BandwidthLimitCmd {
                    incoming_bandwidth: u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
                    outgoing_bandwidth: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
                };
                *buf = &buf[8..];
                CommandBody::BandwidthLimit(c)
            }
            CMD_THROTTLE_CONFIGURE => {
                if buf.len() < 12 {
                    return None;
                }
                let c = ThrottleConfigureCmd {
                    throttle_interval: u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
                    throttle_acceleration: u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]),
                    throttle_deceleration: u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]),
                };
                *buf = &buf[12..];
                CommandBody::ThrottleConfigure(c)
            }
            _ => return None,
        };

        Some(Command { header: hdr, body })
    }

    pub fn write(&self, out: &mut BytesMut) {
        self.header.write(out);
        match &self.body {
            CommandBody::Acknowledge(c) => {
                out.put_u16(c.received_reliable_seq);
                out.put_u16(c.received_sent_time);
            }
            CommandBody::Connect(c) => {
                out.put_u16(c.outgoing_peer_id);
                out.put_u8(c.incoming_session_id);
                out.put_u8(c.outgoing_session_id);
                out.put_u32(c.mtu);
                out.put_u32(c.window_size);
                out.put_u32(c.channel_count);
                out.put_u32(c.incoming_bandwidth);
                out.put_u32(c.outgoing_bandwidth);
                out.put_u32(c.throttle_interval);
                out.put_u32(c.throttle_acceleration);
                out.put_u32(c.throttle_deceleration);
                out.put_u32(c.connect_id);
                out.put_u32(c.data);
            }
            CommandBody::VerifyConnect(c) => {
                out.put_u16(c.outgoing_peer_id);
                out.put_u8(c.incoming_session_id);
                out.put_u8(c.outgoing_session_id);
                out.put_u32(c.mtu);
                out.put_u32(c.window_size);
                out.put_u32(c.channel_count);
                out.put_u32(c.incoming_bandwidth);
                out.put_u32(c.outgoing_bandwidth);
                out.put_u32(c.throttle_interval);
                out.put_u32(c.throttle_acceleration);
                out.put_u32(c.throttle_deceleration);
                out.put_u32(c.connect_id);
            }
            CommandBody::Disconnect(c) => {
                out.put_u32(c.data);
            }
            CommandBody::Ping => {}
            CommandBody::SendReliable(c) => {
                out.put_u16(c.data.len() as u16);
                out.put_slice(&c.data);
            }
            CommandBody::SendUnreliable(c) => {
                out.put_u16(c.unreliable_seq);
                out.put_u16(c.data.len() as u16);
                out.put_slice(&c.data);
            }
            CommandBody::SendFragment(c) | CommandBody::SendUnreliableFragment(c) => {
                out.put_u16(c.start_seq);
                out.put_u16(c.data.len() as u16);
                out.put_u32(c.fragment_count);
                out.put_u32(c.fragment_number);
                out.put_u32(c.total_length);
                out.put_u32(c.fragment_offset);
                out.put_slice(&c.data);
            }
            CommandBody::SendUnsequenced(c) => {
                out.put_u16(c.unsequenced_group);
                out.put_u16(c.data.len() as u16);
                out.put_slice(&c.data);
            }
            CommandBody::BandwidthLimit(c) => {
                out.put_u32(c.incoming_bandwidth);
                out.put_u32(c.outgoing_bandwidth);
            }
            CommandBody::ThrottleConfigure(c) => {
                out.put_u32(c.throttle_interval);
                out.put_u32(c.throttle_acceleration);
                out.put_u32(c.throttle_deceleration);
            }
        }
    }
}

// ── CRC-32 ────────────────────────────────────────────────────────────────────

const CRC32_TABLE: [u32; 256] = {
    let poly: u32 = 0xEDB88320;
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ poly;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc = (crc >> 8) ^ CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize];
    }
    !crc
}

/// Parse all commands from a datagram's command payload region.
pub fn parse_commands(mut buf: &[u8]) -> Vec<Command> {
    let mut cmds = Vec::new();
    while !buf.is_empty() {
        if cmds.len() >= MAX_COMMANDS_PER_PACKET {
            break;
        }
        match Command::parse(&mut buf) {
            Some(cmd) => cmds.push(cmd),
            None => break,
        }
    }
    cmds
}

// ── Sequence number helpers ───────────────────────────────────────────────────

/// Returns true if `a` comes before `b` in wrapped 16-bit sequence space.
#[inline]
pub fn seq_lt(a: u16, b: u16) -> bool {
    (b.wrapping_sub(a) as i16) > 0
}
