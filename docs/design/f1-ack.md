# `0x5509` ACK protocol design

> **2026-07-14 source-confirmation update.** The original revision of this
> design assigned opcode `0x5507`. Foundation Sunshine source inspection at
> `e110872d` shows **`0x5507` is already taken** (host→client resolution-change
> notification, `send_resolution_change`, `stream.cpp`) and `0x5508` is taken
> by clipboard sync; the Foundation extension range `0x5500–0x5508` is fully
> allocated. **The bitrate ACK therefore uses `0x5509`.** R-1/R-2/R-5/R-6 in §6
> are now resolved from source, and the host patch exists at
> `docs/host-patches/foundation-sunshine-dynamic-bitrate-ack.patch`
> (apply-verified on `e110872d` on top of the capability patch; not yet
> compile-verified — that requires the Foundation build environment).

## Background

`f1-apply-path.md` documents that the current `0x5506` path is one-directional.
The Foundation Sunshine handler at `e110872d` validates `1..=800000 kbps`,
dispatches an asynchronous encoder event, and returns immediately. NVENC
reconfiguration success is visible only in host-side logs. `LiChangeBitrate`
returns ENet reliable-queue success, recorded as
`BitrateApplyOutcome::SentUnacknowledged`.

The only truthful client states today are `unsupported`, `send_failed`, and
`sent_unacknowledged`. `f1-apply-path.md` leaves an explicit open item: "add a
request-id ACK if the product must expose `applied` without host-log
correlation."

This document specifies the minimum protocol to close that gap.

**R-1 and R-2 are now confirmed from source (2026-07-14, §6).** The host patch
is written and apply-verified. The client-side state machine (step 2) is
committed and disarmed; the remaining work is the moonlight-common-c receive
hook (§6 R-2 resolution), the Rust wrapper binding, arming the state machine
behind the `0x80` capability bit, and a live paired build to compile-verify
the host patch and measure `ack_latency_ms`.

---

## 1. ACK tier selection

Two tiers are possible given the Foundation handler structure.

| Tier | When the host sends ACK | `applied_kbps` meaning | Foundation change size |
|------|------------------------|------------------------|------------------------|
| **A — dispatch ACK** | After validation passes and the encoder event is queued (synchronous) | Value queued (= `requested_kbps`) | Small (~20 LOC) |
| **B — encoder ACK** | After NVENC reconfiguration completes (asynchronous callback) | Value actually applied by NVENC | Large (async completion channel + new callback hook) |

**This design targets Tier A.** Tier B requires adding a completion channel to
the Foundation encoder loop, which is a separate and larger undertaking.
`e110872d` stops at "dispatches an asynchronous encoder event", so the handler
cannot know the NVENC result synchronously.

Tier A's honest interpretation: the host handler received the request, passed
validation, and forwarded it to the encoder event queue. This is one step
stronger than `sent_unacknowledged`, but it is not equivalent to NVENC apply
success. UI labels and benchmark schema entries must reflect this distinction.

---

## 2. `0x5509` packet layout

**Direction**: host → client, ENet reliable, same encrypted control channel
(`CTRL_CHANNEL_GENERIC`). Opcode `0x5509` — see the header note for why not
`0x5507`/`0x5508`.

**The `0x5506` request payload stays 8 bytes, unchanged.** Adding a
`request_seq` field to the outbound payload would require changes to
`moonlight-common-c.patch`, the Rust wrapper, and all Foundation handlers.
Because the `ApplyGate` enforces a 900 ms minimum send interval and the client
tracks at most one in-flight request at any moment, `request_seq` in the ACK
reply is unnecessary: the ACK always applies to the latest request. The field is
therefore reserved (always 0) in this revision.

### 16-byte payload, little-endian

```
offset  size  field           notes
0       4     parameter_type  always 2 (video::dynamic_param_type_e::BITRATE),
                              mirroring the 0x5506 request
4       4     request_seq     RESERVED — always 0 in this revision; kept for
                              future extensibility without layout change
8       4     applied_kbps    Tier A: echoes requested_kbps; Tier B: actual
                              NVENC-applied value (not used in this revision)
12      4     status          AckStatus enum (u32 LE, see below)
```

### `AckStatus` enum

```
0  DISPATCHED          Validation passed, encoder event queued (Tier A success)
1  VALIDATION_FAILED   Handler validation failed (out-of-range kbps, etc.)
2  ENCODER_APPLIED     NVENC reconfiguration confirmed (Tier B only; a Tier A
                       host never emits this value)
3  ENCODER_FAILED      NVENC reconfiguration failed (Tier B only)
4  UNSUPPORTED_PARAM   parameter_type not handled by this host
```

A Tier A host emits only `DISPATCHED` or `VALIDATION_FAILED`.

---

## 3. Capability negotiation

### Existing bit

```c
#define LI_FF_DYNAMIC_BITRATE  0x40   // host accepts 0x5506 requests
```

### New bit

```c
// Provisional BetterParsec/Foundation extension.
// Host advertises this bit when it will send 0x5509 ACK replies to 0x5506
// requests.  Requires LI_FF_DYNAMIC_BITRATE (0x40) to also be set.
#define LI_FF_DYNAMIC_BITRATE_ACK  0x80
```

### Why 0x40 alone is unsafe for ACK detection

Foundation `e110872d` already ships in paired deployments advertising 0x40. That
build does not send `0x5509`. If a patched client inferred ACK support from 0x40
alone, it would enter `PendingAck` state and wait forever, producing
`AckTimeout` → retry loops every 3 seconds. A separate bit is required so that
the pre-ACK Foundation build remains safe.

### Mixed-version matrix

| Client | Host | Behaviour |
|--------|------|-----------|
| Patched (understands 0x40 + 0x80) | Advertises 0x40 only, no 0x5509 | Client uses `SentUnacknowledged` fallback path; ACK tracking inactive; identical to existing behaviour |
| Patched | Advertises 0x40 + 0x80, sends 0x5509 | `PendingAck` → `Applied` / `ApplyFailed` path active |
| Unpatched client | Advertises 0x40 + 0x80 | Unpatched client ignores 0x80; receives 0x5509 via ENet reliable but moonlight-common-c's receive loop silently frees unknown control types (`ControlStream.c` fallthrough, confirmed §6 R-2); no stream corruption |
| Patched | Stock Sunshine (no 0x40) | 0x5506 is not sent at all; falls through to existing `Unsupported` state |

---

## 4. Client state machine extension

### Current states (`BitrateApplyStatus`)

```
Idle
SentUnacknowledged { kbps }
Unsupported { requested_kbps, reason }
Failed { requested_kbps, error }
```

### Proposed extension (active only after ACK capability is negotiated)

```rust
pub(crate) enum BitrateApplyStatus {
    Idle,
    SentUnacknowledged { kbps: u32 },            // legacy path: host does not
                                                  // advertise 0x80
    PendingAck { kbps: u32, sent_at_ms: u64 },   // ACK-capable host: waiting
                                                  // for 0x5509
    Applied { requested_kbps: u32, applied_kbps: u32, tier: AckTier },
    ApplyFailed { requested_kbps: u32, status: AckStatus },
    AckTimeout { kbps: u32 },                     // ACK-capable host did not
                                                  // respond within 3000 ms
    Unsupported { requested_kbps: u32, reason: &'static str },
    Failed { requested_kbps: u32, error: String },
}
```

`AckTier` — `Dispatched` (Tier A) | `EncoderConfirmed` (Tier B).

### ACK receive path

When `0x5509` arrives on the control channel the existing `control_rx` task
parses it and delivers it to the apply machine via
`Arc<Mutex<BitrateApplyStatus>>` or an mpsc channel.

`BitrateApplyMachine::handle_ack(applied_kbps, status, now_ms)`:
- Current state is `PendingAck { kbps }` → transition to `Applied` or
  `ApplyFailed`.
- Current state is anything else → discard silently with a trace-level log (late
  ACK or unexpected delivery).

### ACK timeout

**Value: 3 000 ms.**

Rationale: ENet reliable round-trip on LAN is measured in single-digit
milliseconds. Allowing 2 000 ms for encoder event queue processing latency and
500 ms × 2 for control-loop polling jitter yields approximately 3 seconds. A
Tier A handler that has not replied within this window is either unresponsive or
has encountered a host-side fault.

`BitrateApplyMachine::poll` transitions from `PendingAck { sent_at_ms }` to
`AckTimeout` when `now_ms − sent_at_ms >= 3000`. `AckTimeout` is not terminal:
the next poll cycle may retry with the same target.

### Benchmark schema v2 additions

```jsonc
{
  "bitrate_apply_events": [
    {
      "t_ms": 12340,
      "requested_kbps": 4229,
      "status": "pending_ack" | "applied_dispatched" | "applied_encoder"
               | "apply_failed" | "ack_timeout" | "sent_unacknowledged",
      "applied_kbps": 4229,    // present when status = applied_*
      "ack_latency_ms": 8,     // PendingAck → Applied/ApplyFailed elapsed time;
                               // null on sent_unacknowledged path
      "seq": 5
    }
  ]
}
```

`ack_latency_ms` is the only client-observable measure of Tier A handler
response time without host-log correlation.

---

## 5. Host patch (Foundation Sunshine) — WRITTEN, apply-verified

The patch exists: `docs/host-patches/foundation-sunshine-dynamic-bitrate-ack.patch`.
It applies on top of `foundation-sunshine-dynamic-bitrate-capability.patch`
(sequence verified with `git apply --check` on a pristine `e110872d` checkout,
2026-07-14). It is **not compile-verified** — that requires the Foundation
build environment (fork-build session).

Confirmed source facts it is built on (all `AlkaidLab/foundation-sunshine@e110872d`):

- The `0x5506` handler is a lambda in `controlBroadcastThread()`
  (`src/stream.cpp`, registered via
  `server->map(packetTypes[IDX_DYNAMIC_PARAM_CHANGE], ...)`). It has `session`
  and the control server in scope.
- The host→client send path is `encode_control(session, ...)` +
  `session->broadcast_ref->control_server.send(payload, session->control.peer)`
  — the exact pattern used by `send_hdr_mode`, `send_resolution_change`, and
  `send_clipboard` in the same file. AES-GCM encryption is applied by
  `encode_control`; the wire packet is the standard encrypted control envelope.
- `packetTypes[]` is append-only; the patch adds `IDX_BITRATE_ACK 21` /
  `0x5509`.

What the patch changes:

| File | Change |
|------|--------|
| `src/platform/common.h` | `platform_caps::dynamic_bitrate_ack = 0x80` |
| `src/rtsp.cpp` | `caps \|= platf::platform_caps::dynamic_bitrate_ack` |
| `src/stream.cpp` | `IDX_BITRATE_ACK` + `0x5509` table entry; `control_bitrate_ack_t` (16-byte LE payload per §2); `send_bitrate_ack()` mirroring `send_resolution_change`; BITRATE case calls it with `DISPATCHED` (echoing the **capped** bitrate — note the handler caps via `clamp_total_bitrate_to_host_cap`, so `applied_kbps` can be lower than requested) or `VALIDATION_FAILED` |

Client-side remaining work (unchanged estimates):

| File | Expected delta |
|------|----------------|
| `patches/moonlight-common-c.patch` | +~30 LOC: `LI_FF_DYNAMIC_BITRATE_ACK 0x80`; `0x5509` branch in `controlReceiveThreadFunc` before the unknown-type `free()` fallthrough; callback registration (see R-2) |
| `patches/moonlight-common-rust.patch` (`0x5509` parser + binding) | +60–80 LOC |
| `streamer/src/bitrate_apply.rs` (state machine) | **done** (commit `3774524`, disarmed) — arming + receive wiring remains |
| New tests | +100–150 LOC |

One semantic addition over the original sketch: because the Foundation handler
clamps the requested bitrate to a host cap before dispatching, Tier A
`applied_kbps` echoes the **clamped** value, not the raw request. The client
`Applied { requested_kbps, applied_kbps }` state already carries both, so a
clamp is directly observable client-side — an improvement over the original
"echoes requested" assumption in §2, which remains the layout but not always
the value.

---

## 6. Risks — resolution status (2026-07-14 source inspection, `e110872d`)

**[R-1] host→client control packet delivery — RESOLVED: CONFIRMED-SAFE**
The `0x5506` handler lives in `src/stream.cpp` inside `controlBroadcastThread()`
(lambda registered via `server->map(packetTypes[IDX_DYNAMIC_PARAM_CHANGE], …)`).
The handler has `session_t *session` in scope; `session->control.peer` and
`session->broadcast_ref->control_server.send(...)` are exactly how the three
existing host→client senders (`send_hdr_mode`, `send_resolution_change`,
`send_clipboard`) transmit, all via `encode_control` AES-GCM framing. No
separate reply channel is needed. **Consequence discovered during
confirmation: `0x5507` and `0x5508` are already allocated (resolution change,
clipboard) — the ACK opcode moved to `0x5509`.**

**[R-2] `sendMessageAndDiscardReply` semantics — RESOLVED: CONFIRMED-SAFE, hook required as designed**
On the ENet path (`AppVersionQuad[0] >= 5`, always true for Sunshine),
`sendMessageAndDiscardReply` (`ControlStream.c`) calls `sendMessageEnet` and
returns — it reads no reply; "discard reply" applies only to the legacy TCP
path. Incoming control packets are dispatched in `controlReceiveThreadFunc`:
six known async-callback types, the termination type, and **an unconditional
`free(ctlHdr)` fallthrough for everything else — an arriving `0x5509` is
silently discarded with no log and no corruption**. So: no interference from
the send helper, but the client cannot see the ACK until
`patches/moonlight-common-c.patch` adds a `0x5509` branch before that
fallthrough (plus a callback registration surfaced to the Rust wrapper).

**[R-3] Synchronous/asynchronous boundary in the Foundation handler (MEDIUM — unchanged, inherent to Tier A)**
`f1-apply-path.md` states "dispatches an asynchronous encoder event." If the
handler returns before any encoder work is done, Tier A ACK reflects only that
the event was queued — not that NVENC accepted or processed it. This is
documented honestly in the `DISPATCHED` status value, but benchmark consumers
and UI labels must not present `DISPATCHED` as "encoder applied".

**[R-4] NVENC behaviour for codecs other than H.264 (LOW for ACK)**
`f1-apply-path.md` notes that Foundation forces reset + IDR only for HEVC
bitrate increases. Tier A ACK is independent of this behaviour — `DISPATCHED`
means the event was queued regardless of codec. However, benchmark collection
must correlate `applied_dispatched` events with measured wire-bitrate changes to
confirm NVENC honoured the request for each codec.

**[R-5] Capability bit collision — RESOLVED: CONFIRMED-SAFE**
`src/platform/common.h` `platform_caps` at `e110872d` allocates `0x01`–`0x20`
(pen_touch, controller_touch, clipboard_text, clipboard_image, touchpad,
touchpad_frame). `0x40` (used by the existing capability patch) and `0x80` are
free.

**[R-6] ENet message ordering — RESOLVED: CONFIRMED-SAFE**
Both directions use ENet channel 0 (`CTRL_CHANNEL_GENERIC`): `LiChangeBitrate`
sends `0x5506` on it, and `control_server_t::send` transmits host→client
replies with `enet_peer_send(peer, 0, …)`. Reliable + same channel = ordered
within the connection; with the 900 ms `ApplyGate` single-in-flight rule, no
interleaving is possible. The "discard if not in `PendingAck`" defence remains
for the residual multi-request edge.

---

## Implementation order — status (2026-07-14)

1. [x] Define `LI_FF_DYNAMIC_BITRATE_ACK = 0x80` in `moonlight-common-c.patch`
   **plus** the `0x5509` receive branch in `controlReceiveThreadFunc` and a
   callback registration (R-2 resolution showed the bit alone is not enough --
   without the branch the ACK is freed before any client code sees it).
   Hook = `controlReceiveThreadFunc` 0x5509 branch + `LiRegisterBitrateAckListener`
   listener API; `moonlight-common-rust` Rust binding + trampoline wired;
   streamer arming behind 0x80 capability wired; 199 tests pass (5 new A09-A13).
   End-to-end ACK pending Foundation fork build (step 4).
2. [x] Client-side `BitrateApplyStatus` extension with unit tests -- commit
   `3774524` (disarmed). The `0x5509` parser in `moonlight-common-rust` remains
   with step 1's callback.
3. [x] Foundation source access; R-1/R-2/R-5/R-6 confirmed (§6). Opcode moved
   to `0x5509`.
4. [~] Host patch written, apply-verified, and **compile-verified** — full
   Foundation build (e110872d + capability + ack patches, MSYS2 UCRT64,
   gcc 16.1.0) succeeded 2026-07-14; staged binary launches with stock
   pairing identity and advertises 0x40|0x80
   (`docs/host-patches/foundation-sunshine-dynamic-bitrate-ack.patch`,
   stage/swap/watchdog-restore machinery validated 3×). Remaining: live
   paired stream with a 0x5506→0x5509 round-trip observed in the streamer
   log (client logs receipt at info since 005496c) — needs one user browser
   session against the swapped host.
5. [ ] Measure `ack_latency_ms` in the benchmark runner; correlate with
   host-log NVENC apply records.
6. [ ] Decide whether Tier B is required based on step 5 correlation quality.

---

## Source references

- `AlkaidLab/foundation-sunshine@e110872d` — `0x5506` handler
  (`src/stream.cpp` `controlBroadcastThread`), `control_server_t::send`,
  `encode_control`, `packetTypes[]` (0x5500–0x5508 allocated), async encoder
  event dispatch
- `docs/host-patches/foundation-sunshine-dynamic-bitrate-ack.patch` — the
  0x5509 Tier A host patch (this design, host side)
- `docs/design/f1-apply-path.md` — ACK absence documented, live validation
  status
- `docs/host-patches/foundation-sunshine-dynamic-bitrate-capability.patch` —
  existing patch structure
- `streamer/src/bitrate_apply.rs` — current `BitrateApplyMachine` state machine
- `patches/moonlight-common-c.patch` — `LiChangeBitrate`,
  `sendMessageAndDiscardReply`
- `patches/moonlight-common-rust.patch` — `MoonlightStream::change_bitrate`
  Rust wrapper
