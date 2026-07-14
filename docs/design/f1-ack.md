# `0x5507` ACK protocol design

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

**Implementation is deferred until Foundation Sunshine source access confirms
R-1 and R-2 (sections 4 and 5 below).** No host patch is written. Client-side
state machine stubs and the capability bit constant may be added speculatively,
but the `PendingAck` path must stay inactive until the host side is verified.

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

## 2. `0x5507` packet layout

**Direction**: host → client, ENet reliable, same encrypted control channel
(`CTRL_CHANNEL_GENERIC`).

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
// Host advertises this bit when it will send 0x5507 ACK replies to 0x5506
// requests.  Requires LI_FF_DYNAMIC_BITRATE (0x40) to also be set.
#define LI_FF_DYNAMIC_BITRATE_ACK  0x80
```

### Why 0x40 alone is unsafe for ACK detection

Foundation `e110872d` already ships in paired deployments advertising 0x40. That
build does not send `0x5507`. If a patched client inferred ACK support from 0x40
alone, it would enter `PendingAck` state and wait forever, producing
`AckTimeout` → retry loops every 3 seconds. A separate bit is required so that
the pre-ACK Foundation build remains safe.

### Mixed-version matrix

| Client | Host | Behaviour |
|--------|------|-----------|
| Patched (understands 0x40 + 0x80) | Advertises 0x40 only, no 0x5507 | Client uses `SentUnacknowledged` fallback path; ACK tracking inactive; identical to existing behaviour |
| Patched | Advertises 0x40 + 0x80, sends 0x5507 | `PendingAck` → `Applied` / `ApplyFailed` path active |
| Unpatched client | Advertises 0x40 + 0x80 | Unpatched client ignores 0x80; receives 0x5507 via ENet reliable but its parser treats it as an unknown opcode; no stream corruption |
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
                                                  // for 0x5507
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

When `0x5507` arrives on the control channel the existing `control_rx` task
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

## 5. Host patch sketch (Foundation Sunshine)

The existing `foundation-sunshine-dynamic-bitrate-capability.patch` modifies two
files:

1. `src/platform/common.h` — adds `platform_caps::dynamic_bitrate = 0x40`.
2. `src/rtsp.cpp` — sets `caps |= platf::platform_caps::dynamic_bitrate`.

A patch adding `0x5507` ACK support would modify three files:

**`src/platform/common.h`** (~3 LOC)
```diff
+    constexpr caps_t dynamic_bitrate_ack = 0x80;
```

**`src/rtsp.cpp`** (~3 LOC)
```diff
+    caps |= platf::platform_caps::dynamic_bitrate_ack;
```

**0x5506 handler file** (~30–50 LOC, file path UNVERIFIED — see R-1 below)
```cpp
// After validation passes and encoder event is queued:
if (caps_advertised_dynamic_bitrate_ack()) {
    uint32_t ack_payload[4] = {
        LE32(2),                      // parameter_type = BITRATE
        LE32(0),                      // request_seq: reserved, always 0
        LE32((uint32_t)bitrateKbps),  // applied_kbps (Tier A: = requested)
        LE32(0),                      // status = DISPATCHED
    };
    sendControlReply(0x5507, sizeof(ack_payload), ack_payload);
}
// On validation failure:
uint32_t fail_payload[4] = { LE32(2), LE32(0), LE32(0), LE32(1) };
sendControlReply(0x5507, sizeof(fail_payload), fail_payload);
```

**Estimated change size**

| File | Expected delta |
|------|----------------|
| `src/platform/common.h` | +3 LOC |
| `src/rtsp.cpp` | +3 LOC |
| 0x5506 handler (path UNVERIFIED) | +30–50 LOC |
| `moonlight-common-c.patch` | no change (0x5506 payload stays 8 bytes) |
| `moonlight-common-rust` (0x5507 parser) | +60–80 LOC |
| `streamer/src/bitrate_apply.rs` (state extension) | +80–120 LOC |
| New tests | +100–150 LOC |

---

## 6. Unverifiable risks (without live Foundation source access)

The following items cannot be confirmed from the patched binaries and commit
diffs available to this project. The design is believed to be correct, but each
risk must be resolved before the host patch is written.

**[R-1] host→client control packet delivery (HIGH)**
The file containing Foundation `e110872d`'s `0x5506` handler is not known.
Whether the handler context has access to the ENet peer handle — and whether a
`sendReply` or equivalent function exists for sending host→client control
replies — is unconfirmed. If the control channel is implemented as client→host
only and the ENet peer handle is not accessible inside the handler, a separate
reply channel design is required. This is the primary blocker for the host
patch.

**[R-2] `sendMessageAndDiscardReply` semantics (MEDIUM)**
`patches/moonlight-common-c.patch` uses `sendMessageAndDiscardReply` for
`LiChangeBitrate`. If this function drops incoming replies at the
moonlight-common-c layer, an arriving `0x5507` would be silently discarded
before BetterParsec can process it. A separate receive hook or callback
registration would then be required in the moonlight-common-c patch.

**[R-3] Synchronous/asynchronous boundary in the Foundation handler (MEDIUM)**
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

**[R-5] Capability bit collision (LOW)**
`moonlight-common-c.patch` comments indicate Foundation `e110872d` uses bits
`0x01` through `0x20`. Whether `0x40` and `0x80` are free in the full
Foundation `platform_caps` list must be confirmed from source.

**[R-6] ENet message ordering (LOW)**
ENet reliable guarantees delivery but not global ordering across channels. With
a 900 ms `ApplyGate` interval limiting the client to one in-flight request,
interleaving is unlikely. The state machine's "discard if not in `PendingAck`"
defence covers the remaining edge case.

---

## Implementation order (post-source-access)

1. Define `LI_FF_DYNAMIC_BITRATE_ACK = 0x80` in `moonlight-common-c.patch`
   (capability bit only; no handler yet).
2. Add client-side `0x5507` parser and `BitrateApplyStatus` extension with full
   unit tests (no live host required).
3. Obtain Foundation source access; confirm R-1 (host→client reply path) and
   R-2 (`sendMessageAndDiscardReply` receive behaviour).
4. Write and apply the host patch; build a paired local Foundation host.
5. Measure `ack_latency_ms` in the benchmark runner; correlate with host-log
   NVENC apply records.
6. Decide whether Tier B is required based on step 5 correlation quality.

---

## Source references

- `AlkaidLab/foundation-sunshine@e110872d` — `0x5506` handler, async encoder
  event dispatch
- `docs/design/f1-apply-path.md` — ACK absence documented, live validation
  status
- `docs/host-patches/foundation-sunshine-dynamic-bitrate-capability.patch` —
  existing patch structure
- `streamer/src/bitrate_apply.rs` — current `BitrateApplyMachine` state machine
- `patches/moonlight-common-c.patch` — `LiChangeBitrate`,
  `sendMessageAndDiscardReply`
- `patches/moonlight-common-rust.patch` — `MoonlightStream::change_bitrate`
  Rust wrapper
