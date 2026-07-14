# Frame-delay CC transport wiring

**Status**: Implemented (streamer-side; live behavior pending a paired-host
session). **Date**: 2026-07-14.
**Source**: `streamer/src/cc.rs` (pure controller, 26 tests, pre-existing),
this wiring change (+8 tests, W01–W08).

## What was wired

`CcController` (Pudica-style frame-delay controller, pure logic) now runs in
the video sample-sender loop. Composition with ABR happens in the runtime
bitrate apply task, exactly as the module header specified:

```
effective_kbps = effective_target_kbps(abr_target, cc_target)   // min, 0 = no signal
```

### Signal source (and its honest limitation)

`on_frame(send_start_us, send_done_us, …)` is fed with **send-side service
time**: the monotonic time from the first RTP packet write of a frame to the
last write completing, measured inside `sample_sender`
(`transport/webrtc/sender.rs`). This is the contract the module documents.

Caveat recorded up front: UDP track writes rarely exert backpressure on an
unsaturated path, so send-side service time may show little dynamic range on a
clean LAN — in that regime CC probes to its ceiling and the composition
degenerates to the ABR target (harmless, but also not additive). Whether the
signal has real range under contention is a **benchmark question** (Gate C/D
of the 2026-07-13 audit). The planned upgrade path — TWCC/receiver-side frame
timestamps — feeds the *same* `on_frame` API without changing this wiring.
Queue-sojourn timing (`enqueued_at → last write`) is a cheaper intermediate
alternative; rejected for now because it deviates from the module's documented
timestamp contract, and signal-source selection should be decided by benchmark
data, not by which measurement was convenient.

## Data flow

```
sample_sender (per frame, hot path)
  ├─ measures send_start/send_done (Instant, monotonic epoch per task)
  ├─ drains CcShared loss mailbox → on_loss_report   (immediate MD on spike)
  ├─ on_frame(...) → publish_target(CcShared)
RTCP reader (per RR, ~1/s)
  └─ posts max fraction_lost into CcShared mailbox (latest-wins, single slot)
runtime bitrate task (500 ms tick, main.rs)
  └─ target = effective_target_kbps(abr_atomic, cc_shared.target_kbps())
     → BitrateApplyMachine::poll → 0x5506 (unchanged)
```

`CcShared` (four atomics; `cc.rs` "Transport wiring state" section):

| field | writer | reader | empty/inactive value |
|---|---|---|---|
| `generation` | video setup (`begin_generation`) | all writers + readers | starts 0; first setup → 1 |
| `target_packed` (gen, kbps) | sender loop | apply task | kbps 0, or any stale-generation tag |
| `loss_packed` (gen, fraction) | RTCP reader | sender loop (drain) | `u64::MAX` sentinel |
| `frame_interval_us` | video setup | sender loop | 0 (disables budget trigger) |

**Generations (ghost-writer defence).** `setup` can run more than once per
transport (stream replacement), and `create_track` spawns a new sample-sender
task without stopping the previous one — both tasks share the queue and the
`CcShared`. Found in adversarial review: without a guard, the ghost task keeps
publishing its stale target (and, after a `bitrate>0 → 0` re-setup, silently
bypasses the CC disable). Every write is therefore tagged with the writer's
generation and validated on the read side (target and loss are packed with
their generation into one atomic word, so the check is race-free). A ghost
write can at worst mask the fresh target for one frame interval, in which case
readers see "inactive" and the composition falls back to ABR-only — never to a
stale CC value. Pinned by tests W09–W11. The underlying two-tasks-one-queue
overlap is pre-existing (audit P1-3) and out of scope here.

## Decisions

- **Enable gate = ABR's gate.** A configured bitrate ceiling
  (`configured_bitrate_kbps > 0`) enables both. No ceiling → no controller →
  `cc_target` stays 0 → composition passes ABR through unchanged. Behavior
  with CC disabled is bit-identical to the pre-change code.
- **Skipped frames are not evidence.** Frames whose every track write was
  skipped (track not ready / paused) do not feed `on_frame`: their ~0 service
  time would read as sustained deflate and probe the target to max while
  nothing is actually being sent.
- **Loss mailbox is a single latest-wins slot.** Between two sender-loop
  drains only the newest RR report survives (older unread one is overwritten
  — deliberate, matches abr.rs "latest observation" semantics; a fresh RR
  arrives every ~1 s so a superseded report is regenerated, not lost).
  Blast radius is bounded at one unread `u8` report by construction.
- **One publish per frame.** `on_loss_report`'s intermediate target is not
  published separately; `on_frame` immediately follows in the same iteration
  and always returns the current target (loss decrease included, even on a
  `Skipped` verdict).
- **Stale-state clear at setup.** Each `WebRtcVideo::setup` calls
  `begin_generation()` (unconditionally, even when CC stays disabled): resets
  the published target, discards unread loss, and invalidates every writer
  from a previous setup (see Generations above).
- **Protocol unchanged.** `StreamerStatsUpdate::RuntimeBitrateControl` keeps
  its schema; `target_kbps` now carries the composed target (same meaning:
  the target the client decided to request). ABR/CC components are visible in
  the streamer log (`abr_target_kbps`, `cc_target_kbps` fields on the
  `sent_unacknowledged` line). Exposing `cc_target_kbps` in the stats schema
  (and benchmark schema v2) is follow-up work, deliberately deferred until
  the signal proves itself in a benchmark.

## Review findings (adversarially verified, all fixed)

1. **Ghost sender task publishes stale CC target after re-setup**
   (`create_track` never stops the previous sample-sender; `bitrate>0 → 0`
   re-setup had its CC disable silently bypassed) — fixed by generations;
   pinned by W09.
2. **Ghost RTCP reader posts stale loss into the new stream's mailbox**
   (old reader loop lives until its track read errors; `remove_track` is
   never called) — fixed by generation-tagged posts/drains; pinned by W10.
   The underlying task-lifecycle overlap is pre-existing (audit P1-3).
3. **`on_loss_report` did not reset `deflate_count`** — with deflate momentum
   at `deflate_frames-1`, the next under-budget frame probed the target back
   up in the same sender-loop iteration, reversing the loss decrease.
   Reproduced with W12 pre-fix (verdict `Increase`, 17 000 → 17 500), then
   fixed in `on_loss_report`; W12 pins the corrected behavior.

## Verification state

- `cargo test -p streamer`: 140 passed / 0 failed (128 pre-existing +
  W01–W12 wiring/generation/loss-momentum tests).
- `cargo clippy --all-targets`: zero streamer warnings.
- Adversarial multi-lens review (concurrency, control-law, hot-path,
  integration) with per-finding refuter verification: 3 findings confirmed,
  all fixed above.
- **Not yet verified live**: real congestion producing a CC decrease that
  composes below ABR. Requires a paired-host stream under a shaped link —
  same status as the ABR apply path (`sent_unacknowledged` tier).

## Follow-ups (not in this change)

1. Benchmark cell measuring cc_target trajectory under the 20→8→15 Mbps
   trace; decide whether send-side service time has usable dynamic range.
2. TWCC / receiver-timestamp feed into the same `on_frame` API if not.
3. `cc_target_kbps` in stats + benchmark schema v2.
4. FEC (`fec.rs`) transport attachment — separate design: needs a framing
   decision (custom RTP vs DataChannel) that this wiring does not prejudge.
