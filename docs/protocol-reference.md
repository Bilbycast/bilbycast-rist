# Protocol Reference

Technical details of the bilbycast-rist implementation, covering wire formats, state machines, and design decisions.

## Specifications

| Spec | Title | Status |
|------|-------|--------|
| RFC 3550 | RTP: A Transport Protocol for Real-Time Applications | Implemented |
| RFC 4585 | Extended RTP Profile for RTCP-Based Feedback (AVPF) | Implemented (Generic NACK) |
| TR-06-1:2020 | RIST Simple Profile | Implemented |
| TR-06-2:2024 | RIST Main Profile | Types stubbed |

## Packet Formats

### RTP (RFC 3550)

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|V=2|P|X|  CC   |M|     PT      |       sequence number         |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                           timestamp                           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|                             SSRC                              |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
```

- **V** = 2 (always)
- **PT** = 33 (MPEG-2 TS, per SMPTE 2022-1)
- **Timestamp** = NTP-aligned, 90 kHz clock (NTP_seconds x 90000, truncated to 32 bits)
- **Payload** = 1316 bytes (7 x 188-byte TS packets, standard for broadcast)

### RTCP Sender Report (PT=200)

Sent by the sender every 100 ms. Always RC=0 (no report blocks) per TR-06-1.

```
 Header: V=2, P=0, RC=0, PT=200, length=6
 Body:   SSRC, NTP_MSW, NTP_LSW, RTP_timestamp, packet_count, octet_count
```

The NTP and RTP timestamps use the same wall-clock source. The receiver uses this mapping to compute output timing.

### RTCP Receiver Report (PT=201)

Sent by the receiver every 100 ms. RC=1 with one report block, or RC=0 for keepalive.

```
 Header: V=2, P=0, RC=0|1, PT=201, length=1|7
 Body:   SSRC, [ReportBlock: SSRC, fraction_lost, cumulative_lost, ext_highest_seq, jitter, LSR, DLSR]
```

### RTCP SDES (PT=202)

Sent with every compound RTCP. One chunk, one CNAME item, 32-bit aligned.

```
 Header: V=2, P=0, SC=1, PT=202
 Body:   SSRC, CNAME_type(1), length, name_bytes, zero_padding
```

### RTCP NACK (PT=205)

Two formats, both as Transport-layer Feedback (RTPFB). These are not the whole
story — librist's **default** NACK carriage is the PT=204 APP form documented in
the next section, and an interop implementer will meet that one first.

**Bitmask NACK (FMT=1, RFC 4585)**:
```
 Header: V=2, P=0, FMT=1, PT=205
 Body:   sender_SSRC, media_SSRC, [PID(16), BLP(16)]*
```
PID identifies one lost packet. BLP is a bitmask covering packets PID+1 through PID+16.

**Range NACK (FMT=15, TR-06-1)**:
```
 Header: V=2, P=0, FMT=15, PT=205
 Body:   sender_SSRC, media_SSRC, [start(16), extra(16)]*
```
Identifies a contiguous range: start through start+extra (inclusive).

### RTCP APP "RIST" (PT=204)

The APP envelope carries two unrelated mechanisms, distinguished by the subtype
field (the 5 bits where RC normally sits). All of them put the four ASCII bytes
`RIST` in the name field; an APP packet with any other name is preserved as
unknown rather than tearing the compound down.

**Range NACK (subtype=0)**:
```
 Header: V=2, P=0, subtype=0, PT=204
 Body:   SSRC (media source), "RIST", [start(16), extra(16)]*
```
Each entry covers `start` through `start + extra` inclusive, so one entry can
name up to 65 536 sequence numbers. This is librist 0.2.11's **default** NACK
format: a librist receiver emits this, not either PT=205 RTPFB form, unless the
application explicitly opts in. bilbycast-rist parses it and serves the
retransmits.

Two implementation bounds worth knowing for interop. Entries are capped at 512
per packet — librist caps *itself* at 200 entries per NACK packet, and a 2048-byte
RTCP datagram can physically hold at most `(2048 - 12) / 4 = 509`, so no NACK
that can reach the parser intact is ever truncated; surplus entries are
discarded rather than rejected, keeping the compound parse lenient. And the
retransmit work one *received datagram* can force out of the sender is bounded
per datagram, not per sub-packet (`rist_transport::guard::NackWorkBudget`) — a
per-sub-packet cap is no bound at all when a minimal APP Range NACK is 16 bytes
and a datagram holds many of them.

The `SSRC` names the media source whose packets are missing. It is matched
against our own SSRC **ignoring the low bit**, which RIST uses as the
retransmit flag; `SSRC = 0` is accepted as unspecified. A NACK naming any other
flow is ignored.

**RTT Echo** (TR-06-1 Section 5.2.6) is the other mechanism — an optional
round-trip-time measurement.

**Request (subtype=2)**:
```
 Header: V=2, P=0, subtype=2, PT=204, length=4
 Body:   SSRC, "RIST", timestamp_MSW, timestamp_LSW
```

**Response (subtype=3)**:
```
 Header: V=2, P=0, subtype=3, PT=204, length=5
 Body:   SSRC, "RIST", timestamp_MSW, timestamp_LSW, processing_delay_us
```

### Compound RTCP

All RTCP is sent as compound packets (RFC 3550 Section 6.1). First packet must be SR or RR.

Sender compound: `[SR, SDES, RTT_echo_request?]`

Receiver compound: `[RR, SDES, NACK?, RTT_echo_request?]`

RTT Echo Response compound: `[RR(empty), RTT_echo_response]`

## State Machines

### NACK Scheduler (receiver side)

O(1) gap detection using a flat ring buffer indexed by `seq % 4096`.

1. **Gap detection**: When packet N arrives and expected was M (M < N), mark M..N-1 as missing
2. **NACK timing**: First NACK after `base_delay` (20 ms). Subsequent retries at `RTT` intervals if RTT is known
3. **Max retries**: After `max_nack_retries` (default 10) attempts, give up on the packet
4. **Recovery**: When an out-of-order or retransmitted packet arrives, deactivate its NACK slot

### Retransmit Buffer (sender side)

O(1) insert and lookup using a flat ring buffer indexed by `seq % capacity`.

- Default capacity: 2048 packets (power of two for fast modulo via bitmask)
- Each slot stores `Bytes` (reference-counted, zero-copy on retransmit)
- Stale detection: slot stores the sequence number; lookup verifies it matches

### RTT Estimator

TCP-style EWMA (RFC 6298):
- `SRTT = 7/8 * SRTT + 1/8 * sample`
- `RTTVAR = 3/4 * RTTVAR + 1/4 * |SRTT - sample|`
- NACK delay = SRTT / 2

### Bonding Merger (SMPTE 2022-7)

Deduplicates packets arriving on multiple network paths:
- Maintains a window of recently seen sequence numbers
- First arrival wins; duplicates are dropped
- Handles wraparound at the 16-bit sequence boundary

## Timestamp Design

RTP timestamps are NTP-aligned at 90 kHz:

```
rtp_timestamp = (NTP_time_in_microseconds * 90 / 1000) as u32
```

Where NTP time = Unix time + 2,208,988,800 seconds.

This ensures the SR's NTP-to-RTP mapping is exact, which is required for correct output timing in receivers. Both RTP data packets and RTCP SR packets derive timestamps from the same wall-clock epoch captured at sender start (monotonic `Instant` paired with `SystemTime` for NTP conversion).

## Port Convention

RIST Simple Profile uses adjacent UDP port pairs:
- **RTP data**: even port P
- **RTCP control**: port P + 1

Both sender and receiver bind dual-port channels. The port must be even; odd ports are rejected with `ChannelError::OddPort`.

## Socket Configuration

Sockets are created with:
- `SO_REUSEADDR` -- allows quick restart after crash
- 32 MB receive buffer -- prevents kernel drops at high bitrates
- 32 MB send buffer -- prevents send blocking during retransmit bursts
- Non-blocking mode -- for tokio async I/O

## Concurrency Design

The sender and receiver tasks own all mutable state, with exactly one exception on the receiver side:
- Sender task: sequence counter, RTCP state, retransmit buffer, RTT estimator -- no `Mutex` or `RwLock` anywhere
- Receiver task: RTCP state, NACK scheduler, RTT estimator, plus the reorder/jitter buffer behind the single lock described below
- Communication: `mpsc` channels for application data in/out

That single lock is a `Mutex<ReorderBuffer>` shared between the RTP receive path and the delivery side. It is taken once per arrival *burst* -- one guard covering a single `insert` for every packet drained from the socket in that wake-up -- or once per drain for a `drain_ready` pop, and it is always released before the delivery `try_send`, so neither side can stall the other. The drain side is not a tokio task at all: it is a dedicated OS thread (`rist-drain`) parked on a `Condvar`, woken either by the receive path's `notify_one` or by the head slot's release deadline.

The `tokio::select!` loop handles all I/O multiplexing without blocking.
