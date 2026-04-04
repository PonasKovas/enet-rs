use std::collections::{BTreeMap, VecDeque};

use bytes::{Bytes, BytesMut};

use crate::protocol::{FREE_RELIABLE_WINDOWS, RELIABLE_WINDOW_SIZE, seq_gt};

// ── Outgoing reliable tracking ────────────────────────────────────────────────

#[derive(Debug)]
pub struct OutgoingReliable {
    pub reliable_seq: u16,
    pub sent_time: u32,
    pub rtt_timeout: u32,
    pub send_attempts: u32,
    /// Serialized command bytes (header+body, no datagram header).
    pub packet: Bytes,
}

// ── Fragment reassembly ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct FragmentBuffer {
    pub total_length: usize,
    pub fragment_count: u32,
    pub received: u32,
    pub buf: Vec<u8>,
    /// Bitmask: bit i set = fragment i arrived.
    pub mask: Vec<u64>,
}

impl FragmentBuffer {
    pub fn new(total_length: usize, fragment_count: u32) -> Self {
        let mask_len = ((fragment_count as usize) + 63) / 64;
        Self {
            total_length,
            fragment_count,
            received: 0,
            buf: vec![0u8; total_length],
            mask: vec![0u64; mask_len],
        }
    }

    /// Returns true if this was the last fragment needed.
    pub fn receive(&mut self, fragment_number: u32, offset: usize, data: &[u8]) -> bool {
        let word = fragment_number as usize / 64;
        let bit = fragment_number as usize % 64;
        if word >= self.mask.len() {
            return false;
        }
        if self.mask[word] & (1 << bit) != 0 {
            // duplicate
            return false;
        }
        self.mask[word] |= 1 << bit;
        self.received += 1;

        let end = offset + data.len();
        if end <= self.buf.len() {
            self.buf[offset..end].copy_from_slice(data);
        }

        self.received == self.fragment_count
    }

    pub fn finish(self) -> Bytes {
        Bytes::from(self.buf)
    }
}

// ── Channel ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct Channel {
    // Outgoing
    pub outgoing_reliable_seq: u16,
    pub outgoing_unreliable_seq: u16,
    pub outgoing_unsequenced_group: u16,
    pub used_reliable_windows: [u16; 16],

    // Incoming reliable
    pub incoming_reliable_seq: u16,
    /// Reliable commands held waiting for an earlier seq to arrive.
    pub incoming_reliable_queue: BTreeMap<u16, Bytes>,

    // Incoming unreliable
    pub incoming_unreliable_seq: u16,
    pub incoming_unreliable_queue: VecDeque<(u16, Bytes)>,

    // Reliable fragment reassembly: start_seq -> buffer
    pub fragment_buffers: BTreeMap<u16, FragmentBuffer>,
}

impl Channel {
    pub fn new() -> Self {
        Self {
            outgoing_reliable_seq: 0,
            outgoing_unreliable_seq: 0,
            outgoing_unsequenced_group: 0,
            used_reliable_windows: [0u16; 16],
            incoming_reliable_seq: 0,
            incoming_reliable_queue: BTreeMap::new(),
            incoming_unreliable_seq: 0,
            incoming_unreliable_queue: VecDeque::new(),
            fragment_buffers: BTreeMap::new(),
        }
    }

    /// Advance and return the next outgoing reliable sequence number.
    pub fn next_reliable_seq(&mut self) -> u16 {
        self.outgoing_reliable_seq = self.outgoing_reliable_seq.wrapping_add(1);
        self.outgoing_reliable_seq
    }

    pub fn next_unreliable_seq(&mut self) -> u16 {
        self.outgoing_unreliable_seq = self.outgoing_unreliable_seq.wrapping_add(1);
        self.outgoing_unreliable_seq
    }

    pub fn next_unsequenced_group(&mut self) -> u16 {
        self.outgoing_unsequenced_group = self.outgoing_unsequenced_group.wrapping_add(1);
        self.outgoing_unsequenced_group
    }

    /// Returns true if the reliable window is open for sending.
    pub fn reliable_window_open(&self, in_flight: usize) -> bool {
        in_flight < (FREE_RELIABLE_WINDOWS as usize * RELIABLE_WINDOW_SIZE as usize)
    }

    /// Receive a reliable in-order delivery. Returns in-order messages that are now deliverable.
    pub fn receive_reliable(&mut self, seq: u16, data: Bytes) -> Vec<Bytes> {
        let expected = self.incoming_reliable_seq.wrapping_add(1);

        if seq == expected {
            self.incoming_reliable_seq = seq;
            let mut out = vec![data];
            // Drain any buffered follow-on sequences
            loop {
                let next = self.incoming_reliable_seq.wrapping_add(1);
                if let Some(d) = self.incoming_reliable_queue.remove(&next) {
                    self.incoming_reliable_seq = next;
                    out.push(d);
                } else {
                    break;
                }
            }
            out
        } else if seq_gt(expected, seq) {
            // Buffer only if within the receive window (FREE_RELIABLE_WINDOWS * RELIABLE_WINDOW_SIZE = 32 768)
            let window: u16 = FREE_RELIABLE_WINDOWS * RELIABLE_WINDOW_SIZE;
            if seq.wrapping_sub(expected) <= window {
                self.incoming_reliable_queue.insert(seq, data);
            }
            vec![]
        } else {
            // Old duplicate — discard
            vec![]
        }
    }

    /// Receive an unreliable sequenced message. Returns Some(data) if accepted.
    pub fn receive_unreliable(&mut self, seq: u16, data: Bytes) -> Option<Bytes> {
        // Accept if seq comes strictly after current incoming seq
        if seq_gt(self.incoming_unreliable_seq, seq) {
            self.incoming_unreliable_seq = seq;
            Some(data)
        } else {
            None // late/duplicate, discard
        }
    }
}

// ── Unsequenced duplicate detection ──────────────────────────────────────────

pub struct UnsequencedWindow {
    pub window: [u32; 32],
    pub group: u16,
}

impl UnsequencedWindow {
    pub fn new() -> Self {
        Self {
            window: [0u32; 32],
            group: 0,
        }
    }

    /// Returns true if the packet with this group is new (not a duplicate).
    pub fn check_and_set(&mut self, group: u16) -> bool {
        // Compute index relative to current window base
        let diff = group.wrapping_sub(self.group) as i16;
        if diff >= 0 {
            // Advance window if needed
            let advance = diff as u16;
            if advance >= 64 {
                // Fully outside old window — reset
                self.window = [0u32; 32];
            } else if advance > 0 {
                // Clear bits for the newly valid positions
                for i in 0..advance {
                    let idx = (self.group.wrapping_add(i + 1)) as usize % 1024;
                    self.window[idx / 32] &= !(1 << (idx % 32));
                }
            }
            if diff as u16 >= 64 {
                self.group = group.wrapping_sub(63);
            } else {
                // self.group stays
            }
        } else if diff < -64 {
            return false; // too old
        }

        let idx = group as usize % 1024;
        let word = idx / 32;
        let bit = 1u32 << (idx % 32);
        if self.window[word] & bit != 0 {
            return false; // duplicate
        }
        self.window[word] |= bit;
        true
    }
}

/// Encode a command into bytes (without the datagram header).
pub fn encode_command(
    cmd_type: u8,
    flags: u8,
    channel_id: u8,
    reliable_seq: u16,
    body_writer: impl FnOnce(&mut BytesMut),
) -> Bytes {
    let mut buf = BytesMut::with_capacity(64);
    buf.extend_from_slice(&[cmd_type | flags, channel_id]);
    buf.extend_from_slice(&reliable_seq.to_be_bytes());
    body_writer(&mut buf);
    buf.freeze()
}
