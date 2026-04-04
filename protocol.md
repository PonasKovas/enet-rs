# ENet Protocol Specification

ENet is a reliable UDP networking library. It multiplexes multiple ordered channels over UDP, providing optional reliable delivery, fragmentation, sequencing, and flow control. All multi-byte integers are **big-endian (network byte order)** unless noted.

---

## UDP Datagram Layout

Each UDP datagram contains exactly one **packet header** followed by one or more **commands**. If the host is configured with a compressor, the command portion may be compressed (see flags below).

```
[ ENetProtocolHeader ]
[ checksum (uint32) ]    -- only if host has a checksum callback
[ compressed or raw command data ]
```

---

## ENetProtocolHeader

Always present. **4 bytes** normally, **6 bytes** when `SENT_TIME` flag is set.

| Offset | Size | Field      | Notes |
|--------|------|------------|-------|
| 0      | 2    | `peerID`   | Encodes peer ID, session ID, and flags (see below) |
| 2      | 2    | `sentTime` | Present only when `HEADER_FLAG_SENT_TIME` is set. Low 16 bits of millisecond send timestamp. |

### peerID encoding

```
bits 15-14  headerFlags   (COMPRESSED=bit14, SENT_TIME=bit15)
bits 13-12  sessionID     (2-bit session counter, 0-3)
bits 11-0   peerID        (the recipient peer's array index, 0-4094; 0xFFFF = no peer/not yet assigned)
```

- `HEADER_FLAG_SENT_TIME   = 0x8000` — `sentTime` field is present
- `HEADER_FLAG_COMPRESSED  = 0x4000` — command payload is compressed
- `HEADER_SESSION_MASK     = 0x3000`
- `HEADER_SESSION_SHIFT    = 12`

When no peer is yet associated (e.g. incoming CONNECT before assignment), `peerID` is set to `0xFFFF`.

### Checksum

If the host has a checksum function configured, a **uint32** checksum follows the header (after `sentTime` if present, before commands). It is computed as **CRC-32** (ISO 3309 / ITU-T V.42, reflected polynomial `0xEDB88320`) over the entire datagram, with the checksum field temporarily set to the peer's `connectID` (or `0` if no peer is known yet) during calculation. The result is stored in **network byte order**.

```
crc = 0xFFFFFFFF
for each byte b in datagram:
    crc = (crc >> 8) ^ table[(crc & 0xFF) ^ b]
stored_checksum = htonl(~crc)
```

---

## Command Header

Every command begins with a 4-byte header:

| Offset | Size | Field                    | Notes |
|--------|------|--------------------------|-------|
| 0      | 1    | `command`                | Low 4 bits = command type; bits 6-7 = flags |
| 1      | 1    | `channelID`              | Target channel (0-based); `0xFF` for connection-level commands |
| 2      | 2    | `reliableSequenceNumber` | Per-channel reliable sequence number |

### Command flags (in `command` byte)

- `COMMAND_FLAG_ACKNOWLEDGE = 0x80` — receiver must send an ACK
- `COMMAND_FLAG_UNSEQUENCED = 0x40` — unsequenced delivery

### Command types

| Value | Name |
|-------|------|
| 0  | NONE |
| 1  | ACKNOWLEDGE |
| 2  | CONNECT |
| 3  | VERIFY_CONNECT |
| 4  | DISCONNECT |
| 5  | PING |
| 6  | SEND_RELIABLE |
| 7  | SEND_UNRELIABLE |
| 8  | SEND_FRAGMENT |
| 9  | SEND_UNSEQUENCED |
| 10 | BANDWIDTH_LIMIT |
| 11 | THROTTLE_CONFIGURE |
| 12 | SEND_UNRELIABLE_FRAGMENT |

---

## Command Formats

All offsets are from the start of the command (i.e. including the 4-byte command header).

### ACKNOWLEDGE (1)

Sent in response to any command with `COMMAND_FLAG_ACKNOWLEDGE`.

| Offset | Size | Field                          |
|--------|------|--------------------------------|
| 4      | 2    | `receivedReliableSequenceNumber` |
| 6      | 2    | `receivedSentTime`             |

`receivedSentTime` is the low 16 bits of the `sentTime` from the header of the packet being acknowledged.

### CONNECT (2)

Sent by the initiating peer. `channelID = 0xFF`. Must have `COMMAND_FLAG_ACKNOWLEDGE`.

| Offset | Size | Field                        |
|--------|------|------------------------------|
| 4      | 2    | `outgoingPeerID`             |
| 6      | 1    | `incomingSessionID`          |
| 7      | 1    | `outgoingSessionID`          |
| 8      | 4    | `mtu`                        |
| 12     | 4    | `windowSize`                 |
| 16     | 4    | `channelCount`               |
| 20     | 4    | `incomingBandwidth`          |
| 24     | 4    | `outgoingBandwidth`          |
| 28     | 4    | `packetThrottleInterval`     |
| 32     | 4    | `packetThrottleAcceleration` |
| 36     | 4    | `packetThrottleDeceleration` |
| 40     | 4    | `connectID`                  |
| 44     | 4    | `data`                       |

- `outgoingPeerID`: the slot index this peer uses locally (echoed back by server as `outgoingPeerID` in VERIFY_CONNECT so the client can route replies correctly)
- `incomingSessionID` / `outgoingSessionID`: set to `0xFF` on initial connect to request fresh session IDs
- `connectID`: random 32-bit nonce generated per connection attempt
- `data`: application-defined connection data

### VERIFY_CONNECT (3)

Server's response to CONNECT. `channelID = 0xFF`. Must have `COMMAND_FLAG_ACKNOWLEDGE`.

| Offset | Size | Field                        |
|--------|------|------------------------------|
| 4      | 2    | `outgoingPeerID`             |
| 6      | 1    | `incomingSessionID`          |
| 7      | 1    | `outgoingSessionID`          |
| 8      | 4    | `mtu`                        |
| 12     | 4    | `windowSize`                 |
| 16     | 4    | `channelCount`               |
| 20     | 4    | `incomingBandwidth`          |
| 24     | 4    | `outgoingBandwidth`          |
| 28     | 4    | `packetThrottleInterval`     |
| 32     | 4    | `packetThrottleAcceleration` |
| 36     | 4    | `packetThrottleDeceleration` |
| 40     | 4    | `connectID`                  |

- `outgoingPeerID`: the index the server assigned to this client (client uses this in the peerID header field of all subsequent packets)
- `connectID`: must echo the client's `connectID` from CONNECT
- Session IDs: server assigns and echoes negotiated session IDs

### DISCONNECT (4)

| Offset | Size | Field  |
|--------|------|--------|
| 4      | 4    | `data` |

Graceful disconnect uses `COMMAND_FLAG_ACKNOWLEDGE`; forced disconnect does not.

### PING (5)

No fields beyond the 4-byte command header.

### SEND_RELIABLE (6)

Must have `COMMAND_FLAG_ACKNOWLEDGE`. Payload follows immediately after the command.

| Offset | Size | Field        |
|--------|------|--------------|
| 4      | 2    | `dataLength` |
| 6      | var  | payload      |

### SEND_UNRELIABLE (7)

| Offset | Size | Field                       |
|--------|------|-----------------------------|
| 4      | 2    | `unreliableSequenceNumber`  |
| 6      | 2    | `dataLength`                |
| 8      | var  | payload                     |

### SEND_FRAGMENT (8)

Reliable fragment. Must have `COMMAND_FLAG_ACKNOWLEDGE`.

| Offset | Size | Field                  |
|--------|------|------------------------|
| 4      | 2    | `startSequenceNumber`  |
| 6      | 2    | `dataLength`           |
| 8      | 4    | `fragmentCount`        |
| 12     | 4    | `fragmentNumber`       |
| 16     | 4    | `totalLength`          |
| 20     | 4    | `fragmentOffset`       |
| 24     | var  | fragment payload       |

- `startSequenceNumber`: reliable sequence number of the first fragment (same for all fragments of the same message)
- `fragmentNumber`: 0-based index of this fragment
- `totalLength`: byte length of the fully reassembled message
- `fragmentOffset`: byte offset into the reassembled buffer where this fragment's payload goes
- `dataLength`: byte length of this fragment's payload

### SEND_UNSEQUENCED (9)

Must have `COMMAND_FLAG_UNSEQUENCED`.

| Offset | Size | Field               |
|--------|------|---------------------|
| 4      | 2    | `unsequencedGroup`  |
| 6      | 2    | `dataLength`        |
| 8      | var  | payload             |

### BANDWIDTH_LIMIT (10)

| Offset | Size | Field                |
|--------|------|----------------------|
| 4      | 4    | `incomingBandwidth`  |
| 8      | 4    | `outgoingBandwidth`  |

Bandwidth in bytes per second. 0 = unlimited.

### THROTTLE_CONFIGURE (11)

| Offset | Size | Field                        |
|--------|------|------------------------------|
| 4      | 4    | `packetThrottleInterval`     |
| 8      | 4    | `packetThrottleAcceleration` |
| 12     | 4    | `packetThrottleDeceleration` |

### SEND_UNRELIABLE_FRAGMENT (12)

Same layout as SEND_FRAGMENT. Unreliable; fragments may arrive out of order and are grouped by `startSequenceNumber`.

---

## Connection Handshake

```
Client                            Server
  |                                  |
  |-- CONNECT (relSeq=1) ----------->|
  |                                  |
  |<- VERIFY_CONNECT (relSeq=1) -----|
  |<- ACK for CONNECT ---------------|
  |                                  |
  |-- ACK for VERIFY_CONNECT ------->|
  |                                  |
  |         [CONNECTED]              |
```

1. Client sends CONNECT with `reliableSequenceNumber = 1`, `channelID = 0xFF`, flag `ACKNOWLEDGE`.
2. Server allocates a peer slot, assigns session IDs, responds with VERIFY_CONNECT (also `reliableSequenceNumber = 1`, flag `ACKNOWLEDGE`), and sends an ACK for the CONNECT.
3. Client verifies that `connectID` in VERIFY_CONNECT matches the one it sent. Client sends ACK for VERIFY_CONNECT.
4. Both sides are now CONNECTED.

**Session ID assignment**: when `incomingSessionID` or `outgoingSessionID` in CONNECT is `0xFF`, the server picks a new value as `(previous + 1) & 3`, ensuring it differs from the peer's declared outgoing session ID. The negotiated pair is echoed in VERIFY_CONNECT.

---

## Reliable Delivery

- Reliable commands carry `COMMAND_FLAG_ACKNOWLEDGE` and a `reliableSequenceNumber`.
- Receiver sends ACKNOWLEDGE with the echoed sequence number and the `sentTime` from the packet header. **ACK is only queued if the incoming datagram had the `HEADER_FLAG_SENT_TIME` flag set** — if `sentTime` is absent the receiver cannot compute RTT and the ACK is silently skipped.
- The initial `roundTripTimeout` for a reliable command is set on first send: `rtt + 4 * rttVar`. On each retransmit it doubles. There is no cap on the per-command timeout; the peer-level timeout bounds (below) govern disconnection.
- Sender retransmits if no ACK arrives within the command's `roundTripTimeout`.
- The peer is disconnected when, for the oldest outstanding timed-out command (`earliestTimeout = its sentTime`):
  - `serviceTime - earliestTimeout >= timeoutMaximum` (30 000 ms), **or**
  - `(1 << (sendAttempts - 1)) >= timeoutLimit` (32) **and** `serviceTime - earliestTimeout >= timeoutMinimum` (5 000 ms)
- Delivery is window-controlled: up to 8 × 4096 = 32 768 sequence numbers of unacknowledged data may be in flight per channel at once.

**RTT sample** (computed on ACK receipt):

The 16-bit `receivedSentTime` in the ACK is extended to a full 32-bit timestamp:
```
receivedSentTime = (uint16 from ACK) | (serviceTime & 0xFFFF0000)
if (receivedSentTime & 0x8000) > (serviceTime & 0x8000):
    receivedSentTime -= 0x10000
```
`sample = max(serviceTime - receivedSentTime, 1)`. If `serviceTime < receivedSentTime` the packet is discarded.

**RTT update** (asymmetric EWMA):
```
// First sample ever:
rtt    = sample
rttVar = (sample + 1) / 2

// Subsequent samples:
rttVar -= rttVar / 4
if sample >= rtt:
    diff = sample - rtt
    rttVar += diff / 4
    rtt    += diff / 8
else:
    diff = rtt - sample
    rttVar += diff / 4
    rtt    -= diff / 8
```
Initial RTT = 500 ms.

---

## Sequencing

| Delivery type | Command | Ordered? | Reliable? |
|---------------|---------|----------|-----------|
| Reliable | SEND_RELIABLE / SEND_FRAGMENT | Yes, per channel | Yes |
| Unreliable sequenced | SEND_UNRELIABLE / SEND_UNRELIABLE_FRAGMENT | Yes (late arrivals discarded) | No |
| Unsequenced | SEND_UNSEQUENCED | No | No |

Each channel maintains independent reliable and unreliable sequence number spaces (both uint16, wrapping at 65 536).

---

## Fragmentation

When a message exceeds the negotiated MTU, it is split into fragments:

```
fragmentLength = mtu - sizeof(header) - sizeof(SEND_FRAGMENT command fields)
                 (subtract 4 more if checksum is enabled)
fragmentCount  = ceil(totalLength / fragmentLength)
```

Each fragment is a SEND_FRAGMENT (or SEND_UNRELIABLE_FRAGMENT) command sharing the same `startSequenceNumber`. The receiver allocates a buffer of `totalLength` bytes and copies each fragment payload at `fragmentOffset`. A bitmask tracks received fragments; delivery occurs when all `fragmentCount` fragments have arrived.

Maximum fragment count: 1 048 576 (1 M).

---

## Flow Control and Throttling

**Window**: effective send window = `(packetThrottle / 32) * windowSize`. A command is not sent if `reliableDataInTransit + commandSize > window`.

**Window size negotiation**

The server independently computes two window sizes and uses the smaller:

*Peer window* (based on server's `outgoingBandwidth` vs client's `incomingBandwidth`):
```
if both are 0:    peerWindow = 65536
elif one is 0:    peerWindow = (max(serverOut, clientIn) / 65536) * 4096
else:             peerWindow = (min(serverOut, clientIn) / 65536) * 4096
```

*Server window* (based on server's `incomingBandwidth` alone):
```
if serverIn == 0: serverWindow = 65536
else:             serverWindow = (serverIn / 65536) * 4096
```

`windowSize` sent in VERIFY_CONNECT = `min(serverWindow, client's requested windowSize)`, clamped to [4 096, 65 536].

**MTU negotiation**: Server clamps the client's requested MTU to [576, 4096], then takes `min(clamped, hostMtu)`. This final value is sent in VERIFY_CONNECT.

**Throttle** (`packetThrottle`): integer in [0, 32], starting at 32 (full speed). Adjusted every `packetThrottleInterval` (default 5 000 ms):
- Increase by `packetThrottleAcceleration` (default 2) when round-trips are short.
- Decrease by `packetThrottleDeceleration` (default 2) when round-trips are long.

---

## Unsequenced Window

Unsequenced packets use a 1 024-bit sliding window (32 × uint32) to detect duplicates. `unsequencedGroup` (uint16) identifies the window; packets outside the 64-window (65 536 sequence number) history are dropped.

---

## Peer States

```
DISCONNECTED (0)
CONNECTING (1)          -- CONNECT sent
ACKNOWLEDGING_CONNECT (2) -- VERIFY_CONNECT received, awaiting ACK from app
CONNECTION_PENDING (3)
CONNECTION_SUCCEEDED (4)
CONNECTED (5)
DISCONNECT_LATER (6)    -- waiting for outgoing queue to drain
DISCONNECTING (7)       -- DISCONNECT sent
ACKNOWLEDGING_DISCONNECT (8)
ZOMBIE (9)              -- dead, awaiting reset
```

---

## Keep-Alive

If no outgoing data has been sent within `pingInterval` (default 500 ms), a PING command is sent to prevent the connection from timing out.

---

## Compression

If both sides negotiate a compressor, the payload after the packet header (all commands) may be compressed. The `HEADER_FLAG_COMPRESSED` bit in `peerID` signals compression. The built-in optional compressor is an order-2 PPM adaptive range coder.

---

## Protocol Constants

| Constant | Value |
|----------|-------|
| Minimum MTU | 576 bytes |
| Maximum MTU | 4 096 bytes |
| Default host MTU | 1 392 bytes |
| Minimum window size | 4 096 |
| Maximum window size | 65 536 |
| Maximum channels per peer | 255 |
| Maximum peers per host | 4 095 (`0xFFF`) |
| Max commands per packet | 32 |
| Max fragment count | 1 048 576 |
| Reliable window size | 4 096 (sequences) |
| Reliable windows count | 16 |
| Free reliable windows | 8 |
| Unsequenced window size | 1 024 bits |
| Unsequenced windows | 64 |
| Free unsequenced windows | 32 |
| Default ping interval | 500 ms |
| Default RTT | 500 ms |
| Timeout minimum | 5 000 ms |
| Timeout maximum | 30 000 ms |
| Timeout attempt limit | 32 |
| Default packet throttle | 32 (= full) |
| Throttle scale | 32 |
| Throttle interval | 5 000 ms |
| Bandwidth throttle interval | 1 000 ms |
