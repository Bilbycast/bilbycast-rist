# Interop Testing Guide

How to test bilbycast-rist against third-party RIST implementations.

## Prerequisites

- **librist 0.2.11+** -- install via `brew install librist` (macOS) or your package manager
- **ffmpeg** -- for generating test MPEG-TS streams (optional, can use any TS source)
- **cargo** -- Rust toolchain

Build the examples:

```bash
cargo build --examples
```

## Test 1: librist sender -> bilbycast receiver

This tests our receiver's ability to accept RIST streams from a standard sender.

```
ffmpeg (TS source) --> UDP:5000 --> ristsender --> RIST:6000 --> bilbycast --> UDP:7000 --> player
```

Terminal 1 -- our receiver:
```bash
cargo run --example interop_receiver -- --bind 0.0.0.0:6000 --output udp://127.0.0.1:7000
```

Terminal 2 -- librist sender (Simple Profile):
```bash
ristsender -i "udp://127.0.0.1:5000" -o "rist://127.0.0.1:6000?buffer=1000" -p 0 -v 6 -S 1000
```

Terminal 3 -- TS source:
```bash
ffmpeg -re -f lavfi -i testsrc=size=320x240:rate=25 -f lavfi -i sine=frequency=1000 \
    -c:v libx264 -preset ultrafast -b:v 1M -c:a aac -f mpegts \
    "udp://127.0.0.1:5000?pkt_size=1316"
```

Terminal 4 -- verify output:
```bash
ffplay udp://127.0.0.1:7000
```

**Expected**: ristsender reports `quality: 100`, video plays smoothly.

## Test 2: bilbycast sender -> librist receiver

This tests our sender's ability to produce RIST streams that standard receivers accept.

```
ffmpeg (TS source) --> UDP:5100 --> bilbycast --> RIST:6100 --> ristreceiver --> UDP:7100 --> player
```

Terminal 1 -- librist receiver (Simple Profile, **listen mode** -- note the `@`):
```bash
ristreceiver -i "rist://@:6100?buffer=1000" -o "udp://127.0.0.1:7100" -p 0 -v 6 -S 1000
```

> **Important**: The `@` before the port puts ristreceiver in **listen mode**. Without it, ristreceiver tries to connect outward and will fail.

Terminal 2 -- our sender:
```bash
cargo run --example interop_sender -- --input udp://0.0.0.0:5100 --output 127.0.0.1:6100
```

Terminal 3 -- TS source:
```bash
ffmpeg -re -f lavfi -i testsrc=size=320x240:rate=25 -f lavfi -i sine=frequency=1000 \
    -c:v libx264 -preset ultrafast -b:v 1M -c:a aac -f mpegts \
    "udp://127.0.0.1:5100?pkt_size=1316"
```

Terminal 4 -- verify output:
```bash
ffplay udp://127.0.0.1:7100
```

**Expected**: ristreceiver stats show `quality: 100`, `received` count incrementing, video plays.

## Test 3: Self-loop (bilbycast sender -> bilbycast receiver)

Useful for development testing without librist.

Terminal 1 -- receiver:
```bash
cargo run --example receiver
```

Terminal 2 -- sender:
```bash
cargo run --example sender
```

## Verifying stats

ristsender and ristreceiver print JSON stats at the interval specified by `-S` (milliseconds).

Key fields in sender stats:
- `quality` -- 100 means no loss detected
- `sent` -- packets sent in this interval
- `retransmitted` -- packets retransmitted (NACK responses)
- `rtt` / `avg_rtt` -- round-trip time in ms

Key fields in receiver stats:
- `received` -- packets received
- `recovered_total` -- packets recovered via retransmission
- `lost` -- packets permanently lost
- `avg_buffer_time` -- current buffer depth in ms

## Troubleshooting

### ristreceiver shows "Send failed: errno=65"

This occurs when ristreceiver is in **connect mode** (no `@` in the URL). Use `rist://@:PORT` for listen mode.

### No data output from ristreceiver

Check the ristreceiver stats (`-S 1000 -v 7`). If `received` is incrementing but no UDP output appears, the issue is with the output socket, not the RIST connection. Verify the output destination is reachable.

### Connection takes several seconds

RIST Simple Profile requires an RTCP handshake before data flows. The RTCP peer authenticates first (via SR/RR exchange), then the RTP peer authenticates when the first data packet arrives. Allow 2-3 seconds for the full handshake.

### Packet loss at very high rates

Increase the UDP receive buffer. bilbycast-rist uses 2 MB buffers by default. For rates above 100 Mbps, you may need to increase the OS-level maximum:

```bash
# macOS
sudo sysctl -w net.inet.udp.recvspace=4194304

# Linux
sudo sysctl -w net.core.rmem_max=4194304
```

## Supported librist flags

For Simple Profile interop, use these librist flags:

| Flag | Value | Purpose |
|------|-------|---------|
| `-p` | `0` | Simple Profile (required) |
| `-v` | `6` | Info-level logging |
| `-S` | `1000` | Stats every 1000 ms |
| `-b` | `1000` | Buffer size in ms |
| `@` in URL | -- | Listen mode for ristreceiver |
