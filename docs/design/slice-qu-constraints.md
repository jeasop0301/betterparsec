# Subframe slice pipelining and QU constraints inventory

**Status**: Constraints inventory feeding the future perf-core design.
**Date**: 2026-07-14.
**Source**: `docs/research/05-native-client-dcv-composite.md` and codebase
tracing (`streamer/src/video.rs`, `streamer/src/transport/webrtc/`).

This document records what the current codebase does, where the concrete
invariants break for slice pipelining and quality-upgrade (QU) passes, and
which blockers require a Sunshine host fork versus those that can be resolved
streamer-side alone. It does not propose solutions.

---

## 1. Current frame path (concrete types and file:line)

**Encoding**: Sunshine host only. The streamer has no encoder. The
`StreamVideoDecoder` implementation of moonlight-common's `VideoDecoder` trait
receives callbacks from Sunshine with already-encoded data.

**Entry point**: `streamer/src/video.rs:55` —
`fn submit_decode_unit(&mut self, unit: VideoDecodeUnit<&[u8]>) -> DecodeResult`

`unit.buffers` is a list of `&[u8]` slices constituting a single NAL stream for
one encoded frame. The callback passes it to the transport layer via
`sender.send_video_unit(unit.as_ref()).await` at `video.rs:67`.

**WebRTC send path**: `streamer/src/transport/webrtc/video.rs:291` —
`send_decode_unit`

- `video.rs:294-297`: all buffers are concatenated into a single
  `full_frame: Vec<u8>`.
- H.264 / H.265: Annex-B NAL reader parses `full_frame` into NAL units, which
  are accumulated in `self.samples`, then flushed by `send_single_frame`
  (`video.rs:312-348`).
- AV1: `full_frame` pushed as a single `BytesMut` into samples, then
  `send_single_frame` (`video.rs:389-401`).
- `send_single_frame` at `video.rs:435`: drains `samples`, RTP-packetises each
  NAL into `Vec<Packet>` at MTU = `RTP_OUTBOUND_MTU` (~1200 B), passes the
  result to `sender.send_samples(frame_samples, important)`.

**Summary**: one `submit_decode_unit` call = one complete encoded frame. The
streamer processes only complete frames; NAL/OBU splitting is done only for RTP
packetisation.

---

## 2. Slice pipelining constraints

### Where slice boundaries originate

NVENC slice mode (`NV_ENC_CONFIG_H264.sliceMode`,
`NV_ENC_CONFIG_HEVC.numSlicesPerFrame`) is an encoder configuration parameter
accessible only inside the Sunshine host. The streamer has no encoder access.

When slice encoding is enabled, Sunshine produces multiple VCL NAL units per
frame. Whether the Moonlight protocol delivers these as a single
`VideoDecodeUnit` callback or as separate per-slice callbacks is not
determinable from this codebase; it depends on moonlight-common internals that
require the forked source to inspect.

### 1 buffer = 1 frame assumption in current code

`video.rs:294-297` concatenates all buffers before any processing. Even if
slices arrive as separate buffers they are merged before the NAL reader runs.
There is no pipelining: the first slice is not sent until all slices have been
received, concatenated, and processed.

`send_single_frame` at `video.rs:435` drains the entire `samples` list in a
single call to `send_samples`. Per-slice dispatch as slices arrive is not
possible in the current structure.

### Four broken invariants under slice pipelining

1. **IDR detection** (`video.rs:299`, `unit.frame_type`): IDR/key-frame status
   is determined from the complete `VideoDecodeUnit`. A per-slice send path
   would need IDR detection before the full frame is assembled.
2. **PLI / needs-idr response** (`video.rs:407-413`): the compare-exchange that
   arms an IDR request runs at the end of each full frame. Responding to a PLI
   mid-slice is not possible without restructuring this path.
3. **IDR queue clear** (`video.rs:443-445`): `sender.clear_queue(false)` is
   called inside `send_single_frame` when an IDR is detected. Clearing the
   queue mid-slice transmission would leave a partial frame in the queue.
4. **RTP marker bit** (`video.rs:457`, `end_has_marker` parameter to
   `packetize`): the marker bit is set only on the last RTP packet of the last
   NAL in the frame. Sending slices individually changes the frame boundary
   semantics; the marker bit placement would need to be redesigned.

---

## 3. QU (quality-upgrade / build-to-lossless) second-pass constraints

### Reliable dataChannel availability

`streamer/src/transport/webrtc/mod.rs:158-159` already creates a
`general_channel` (reliable=true, ordered=true) and a `stats_channel`
(reliable=true). Adding a QU tile delivery channel follows the same
`RTCDataChannel` pattern at `mod.rs:196-239`. The `send` method at `mod.rs:660`
routes only `GENERAL` and `STATS` channels today; a new channel ID entry is
needed. **This part is streamer-side only.**

### Dirty-rect and idle detection

`VideoDecodeUnit` carries `frame_processing_latency` (used at `video.rs:159`)
and `frame_type` (used at `video.rs:299`). No dirty-rect metadata, slice index,
or static-region flag is present in any field visible in this codebase.

`FrameType::Idr` is the only special frame classification. P-frames and static
frames (no pixel change) are indistinguishable. **Dirty-rect and idle detection
are both absent from the current streamer.**

### QU lossless tile encode path

The QU second pass encodes static screen regions pixel-perfectly from the GPU
framebuffer after motion ceases. This requires Sunshine to:

- read DXGI dirty rects from its capture layer;
- run a separate lossless encode path (NVENC lossless preset or CUDA memcpy
  route);
- deliver the resulting tiles to the streamer, which forwards them over the QU
  reliable DataChannel.

The streamer's role in QU is limited to channel routing. The encode path itself
is entirely inside Sunshine.

---

## 4. Hard blocker classification

### Requires Sunshine host fork

| Blocker | Reason |
|---------|--------|
| NVENC slice mode activation | Encoder config parameter; host-only access |
| Per-slice `submit_decode_unit` callbacks | Callback granularity is determined by the Moonlight protocol layer in Sunshine and moonlight-common; currently 1 callback = 1 frame |
| Dirty-rect / idle metadata in `VideoDecodeUnit` | DXGI dirty rects are accessible only inside the Sunshine capture layer; no field exists in the current callback struct |
| QU lossless tile encode path | Requires a new NVENC lossless preset or CUDA path inside Sunshine |

### Streamer-side only (no host fork required)

| Item | File:line basis |
|------|----------------|
| QU dedicated reliable DataChannel creation | `mod.rs:196-239` pattern |
| QU channel routing in `send` | `mod.rs:660` |
| Per-slice immediate send (after host delivers per-slice callbacks) | `send_single_frame` decomposition, `video.rs:435` |
| PLI / IDR response at slice granularity (after host delivers per-slice) | `video.rs:407-413` |
| RTP marker bit redesign for slice boundaries | `packetize` `end_has_marker`, `video.rs:457` |

---

## 5. Resolved open questions (2026-07-14 vendored-source investigation)

Investigated against `vendor/moonlight-common-rust` (patches applied).

### 5.1 Callback granularity: 1 callback = 1 COMPLETE frame, always

`RtpVideoQueue.c` tracks per-frame multi-FEC blocks
(`multiFecCurrentBlockNumber`/`multiFecLastBlockNumber`, i.e. slices) and only
calls `submitCompletedFrame` after the LAST block
(`RtpVideoQueue.c:783-800`). The depacketizer accumulates every NAL across
all blocks into one chain and calls `reassembleFrame()` exactly once per
frame (`VideoDepacketizer.c:1124`, DU assembly at `469-551`). `FLAG_SOF`/
`FLAG_EOF` (`Video.h:21-23`) are frame boundaries — no slice-boundary flags
exist. **Per-slice delivery is architecturally absent from the protocol
layer**; enabling it means patching the depacketizer to treat FEC-block
boundaries as decode-unit boundaries. Note: we already carry
`patches/moonlight-common-c.patch`, so this is a patch extension, not a new
hard fork.

### 5.2 `CAPABILITY_SLICES_PER_FRAME` controls the ENCODER only

`Limelight.h:279-282` packs a slice count into capability bits 24–31;
`SdpGenerator.c:421-431` forwards it as
`x-nv-video[0].videoEncoderSlicesPerFrame=N`. Sunshine then configures NVENC
with N slices — **encoder-side parallelization (encode-latency win) without
any change to callback granularity**. This is a cheap, host-fork-free lever.

### 5.3 Wrapper gap found: `slices_per_frame` silently ignored on the C path

The Rust C-stream wrapper never translates `VideoCapabilities.
slices_per_frame` into the C capabilities integer
(`src/stream/c/video.rs:167-195`, bitflags at `src/stream/c/bindings.rs:77-88`)
— the C-stream path always advertises 1 slice regardless of the field. The
proto path forwards it correctly (`src/stream/proto/mod.rs:1015`,
`src/stream/proto/sdp/client.rs:706-710`). Fixing the C path = small
extension to `patches/moonlight-common-rust.patch`.

### 5.4 `VideoDecodeUnit` metadata inventory

Rust struct carries frame_number/frame_type/frame_processing_latency/
timestamp/color_space/buffers (`src/stream/video.rs:255-292`). The wrapper
DROPS C fields `receiveTimeUs`, `enqueueTimeUs`, `rtpTimestamp`, `hdrActive`,
`fullLength`. No slice index, slice count, or dirty-rect exists anywhere in
the protocol; `multiFecLastBlockNumber` (`RtpVideoQueue.h:45`) is the closest
client-side observable of "slices actually sent" but is not exposed above the
RTP queue layer.

### 5.5 Revised blocker classification

| Item | Cost | Where |
|---|---|---|
| Encoder-side slicing (encode-latency win, no pipelining) | Small: wrapper patch (5.3) + advertise `CAPABILITY_SLICES_PER_FRAME(n)` | patches/moonlight-common-rust.patch + streamer capability |
| Per-slice decode-unit delivery | Medium: depacketizer patch treating FEC-block boundaries as DU boundaries + DU slice metadata | patches/moonlight-common-c.patch |
| Timing fields for latency attribution (`receiveTimeUs`, `rtpTimestamp`) | Small: wrapper field pass-through | patches/moonlight-common-rust.patch |
| Dirty-rect / QU metadata | Large: no protocol carrier exists — needs a new extension (or the QU DataChannel path from fec-framing.md, which bypasses this protocol entirely) | Sunshine fork + protocol extension |

## 6. Per-slice DU wire contract (2026-07-15 Sunshine-source verification)

The premise of 5.1 — "FEC-block boundary = slice boundary" — was verified
against upstream Sunshine `src/stream.cpp` (master, fetched 2026-07-15;
local copy `server/sunshine_stream_upstream.cpp`): **it does not hold for
our host.**

### 6.1 What Sunshine actually does

- `stream.cpp:1552-1599`: FEC blocks split a frame **by size only** —
  `max_data_per_fec_block` derives from the shard limit, the frame payload is
  divided into equal `aligned_size` chunks, and boundaries land mid-NAL.
  Slice boundaries play no role.
- `MAX_FEC_BLOCKS = 4` (`stream.cpp:1553`): the protocol carries block
  index/count in 2 bits each (`multiFecBlocks = (blockIndex << 4) |
  ((count-1) << 6)`, mirrored by `RtpVideoQueue.c:584/709`). **Per-slice
  blocks therefore cap slice mode at 4 slices per frame.**
- `multiFecFlags` is constant `0x10` on the wire (`stream.cpp:1634`) and the
  client never parses it (`RtpVideoQueue.c:369` — literal `TODO`). Free bits
  are available for a backward-compatible extension signal.

Consequence: a client-side depacketizer patch keyed on FEC-block boundaries
would deliver size-split fragments cut mid-NAL — useless as decode units.
**The Sunshine fork must move first** (or land together): per-slice DU is a
two-sided change with a wire contract, not an independent client patch.

### 6.2 Proposed contract (pin before either side is written)

| Side | Change |
|---|---|
| Host (fork) | When NVENC slice mode is active with N in 2..4 slices and per-slice send is enabled: split the frame payload at VCL-NAL (slice) boundaries into N FEC blocks instead of `aligned_size` chunks; keep per-block blocksize alignment/zero-pad exactly as today (H.264/HEVC tolerate trailing zeros; AV1 is excluded — no slices). Set `multiFecFlags \|= 0x20` ("blocks are slice-aligned"). N>4 or missing boundaries: fall back to stock size-split without 0x20. |
| Client (moonlight-common-c patch) | Parse `multiFecFlags & 0x20`. When set: `RtpVideoQueue` submits each completed FEC block to the depacketizer immediately (today blocks accumulate until the last — `RtpVideoQueue.c:783-800`), and the depacketizer reassembles a DU per block end (new flush point; `FLAG_EOF` still only marks the true frame end). DU gains `sliceIndex`/`sliceCount` metadata (ripples into the Rust wrapper). Flag absent: byte-identical stock behavior. |
| Loss semantics | Unchanged: blocks are already order-enforced (`RtpVideoQueue.c:586` rejects behind-current blocks); a lost block still drops the whole frame through the existing RFI/IDR machinery. Slice DUs only accelerate the happy path. |
| Streamer consumption | Separate opt-in (per-slice send decomposition of `send_single_frame`, section 2's four invariants). Until then the streamer may simply reassemble slice DUs back into frames — correctness-neutral. |

### 6.3 Revised U3 ordering

1. Pin this contract (done — this section).
2. Sunshine fork: slice-aligned FEC blocks + `0x20` flag (MSYS2 UCRT64 build
   session; same worktree as the ACK patches).
3. moonlight-common-c depacketizer patch against the flag (client side,
   testable against fork traffic on loopback).
4. Streamer per-slice send path (section 2 invariants) — Gate C measurement
   decides whether the overlap win pays for the added encoder slice overhead.

## Open questions (remaining)

- How should QU tiles be composited on the browser client (separate `VideoFrame`
  overlay vs. canvas blending)? Client-side constraints have not been
  investigated.
- Is the Sunshine fork within the current project scope (betterparsec-native
  worktree), and which branch should receive slice-mode and QU encode PRs?
