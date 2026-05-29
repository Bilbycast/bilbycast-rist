# bilbycast-rist review + fix plan (2026-05-29)

Driven by: RIST receiver PCR-egress jitter ~10× SRT, and "fix all RIST issues to top quality." 14 issues found. Implementation order below.

## Tier 1 — core (SRT parity + PLL lock + key correctness)
- **#2 arrival_us ≈ 0** (`receiver.rs` ~135): `now.elapsed()` right after `Instant::now()` → RFC3550 jitter (RR + stats.jitter_us) is garbage. Fix: stable monotonic epoch, `arrival_us = now.duration_since(epoch)`. Trivial.
- **#10 SO_RCVBUF 2 MB** (`channel.rs` ~22,82): below the 32 MB hygiene standard; kernel drops at high bitrate during stalls → false NACKs. Fix: default 16–32 MB, log actual after set.
- **#1 jitter → SRT parity** (`receiver.rs` drain): tokio timer-wheel (~1 ms) + single-`select!`-task contention vs libsrt dedicated TSBPD thread. Fix: dedicated SCHED_FIFO thread owning the drain, `clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME)` (mirror `wire_emit.rs`), `Arc<parking_lot::Mutex<ReorderBuffer>>` + Condvar; recv task inserts+notifies, drain thread delivers. Expect p99 ~50–250 µs. Folds in **#4** (next_drain_time purity), **#12** (remove redundant per-arrival/pump drains), **#13** (shutdown join the thread).
- **#3 RIST can't feed PLL a clean rate ref** (`input_rist.rs` 297/301 `recv_time_us=now_us()` post-reorder, `sender_timestamp_us=None`): PLL sees jittery post-1s-hold timing → can't lock source_pcr_pll (SRT surfaces srctime → locks). Fix: stamp `recv_time_us` at TRUE UDP arrival in `receiver.rs` (pre-reorder) and carry it through to `input_rist`. Channel payload `Bytes` → `RistDelivered { data, arrival_us, rtp_seq }`.
- **#7 2022-7 redundant RIST broken** (`input_rist.rs` synthetic per-leg seqs into HitlessMerger): legs use independent counters, so cross-leg dedup/gap-fill can't work. Fix: surface real RTP seq (same `RistDelivered` change) → pass real shared seq to merger.

## Tier 2 — loss-path hardening (validate with RIST_ARQ_TEST.md)
- **#5 reorder overflow drops fresh tail** (`reorder.rs` 141-145): far-future pkt dropped as `stale` when head stuck; should flush head (TLPKTDROP oldest), separate `reorder_overflow` stat.
- **#6 NACK storm + silent loss > 1000-gap** (`nack_tracker.rs` 146-165): gaps > cap never NACKed (expected_seq jumps); large gap → NACK burst. Fix: discontinuity resync on huge gap; per-pump NACK budget.
- **#8 RFC3550 seq accounting** (`rtcp_state.rs` 186-199): duplicates inflate packets_received → wrong fraction_lost/cumulative_lost. Fix: follow A.1 update_seq; don't count dup retransmits (feed InsertOutcome.duplicate).
- **#9 sender retransmit starves media** (`sender.rs` 153-291): N awaited send_to inline; cap per-iteration, batch.

## Tier 3 — minor
- **#11** recvmmsg/pool (throughput only, defer).
- **#14** RTT echo `processing_delay_us: 0` (trivial bias fix).

## Disposition (2026-05-29)
- **DONE + tested:** #1 (dedicated drain thread → SRT parity), #2 (arrival_us), #3 (true-arrival recv_time → PLL parity), #6 (NACK >1000-gap resync), #7 (real-seq 2022-7), #10 (SO_RCVBUF 32 MB). Folded into #1: #4 (next_drain purity — drain_ready handles stale first), #12 (redundant drains removed), #13 (drain thread joined on shutdown).
- **Validated:** 60 rist unit tests; new `tests/loss_recovery.rs` (real sockets + lossy relay → NACK recovery: 799/800 in-order, 41 recovered); PLL re-probe (RIST feed 1000→566 µs); cell12 multi-source pcr_trust.
- **Done (round 2, all formerly-deferred now implemented):**
  - **#5** reorder-overflow → TLPKTDROP: flush the oldest (minimal-flush keeps the recent tail; full reset only on >1-ring jumps) so fresh media keeps flowing instead of dropping the fresh packet and stalling up to buffer_time. New `InsertOutcome.overflow_flushed` counted as loss. Unit tests `overflow_flushes_oldest_not_freshest`, `overflow_huge_jump_resets`.
  - **#8** RFC3550 dup accounting: `on_packet_received` takes `is_duplicate` (fed from the reorder insert outcome) and skips the received-count + jitter EWMA for duplicate retransmits, so RR fraction_lost/cumulative_lost no longer under-report. Unit test `duplicates_do_not_inflate_received_or_jitter`. Recv path reordered to insert-before-rtcp so the duplicate outcome is known.
  - **#9** sender retransmit cap (MAX_RETX_PER_NACK = 512) so a huge loss-burst NACK can't block the sender select! on hundreds of awaited send_to and starve live media; excess re-NACKed next round.
  - **#11** pure-Rust batched recv: drain a bounded burst (≤32, via try_recv_from) and insert under ONE buffer lock + ONE delivery-thread wake — cuts recv↔drain-thread lock contention at high packet rates. NOT recvmmsg (that needs libc and would break the crate's zero-C-deps principle, for no measured bottleneck).
  - **#14** RTT echo `processing_delay_us`: report the real receive→respond turnaround (was hard-coded 0, biasing measured RTT / NACK-retry high). Both sender + receiver echo responders.
  - Re-validated: 63 unit tests + loss_recovery integration test still pass; edge compiles; target-full rebuilt; cell12 SRT-parity re-confirmed.

## Verification (top quality, multi-source)
Rebuild target-full; unit tests; then end-to-end with MULTIPLE real sources (sync-test clean + ABC/Seven H.264 broadcast + HEVC MPTS + 4K):
1. cell12 RIST receiver pcr_trust p99 → SRT band (~220 µs), per source.
2. PLL lock re-probe: RIST input now locks source_pcr_pll (before: fallback).
3. RIST_ARQ_TEST.md loss-recovery interop (librist + impairment) — no regression.
4. Gates 1/2/4/5 per source.
