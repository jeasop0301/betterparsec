# Tetrys FEC transport framing (U2)

**Status**: Design — attachment point decided, wire format specified,
implementation phased. **Date**: 2026-07-14.
**Source**: `streamer/src/fec.rs` (pure codec, 47 tests), transport survey of
`streamer/src/transport/{web_socket,webrtc}`, web client
`web/stream/video/pipeline.ts`.

## 1. Attachment point decision

Four candidate paths were evaluated against one hard constraint: **the browser
exposes no raw-RTP access to JavaScript**, so any FEC scheme the client must
decode in JS cannot ride the WebRTC media track.

| Path | Verdict | Reason |
|---|---|---|
| A. WebRTC RTP video track | **rejected for Tetrys** | Repair symbols are invisible to JS; recovery would have to be browser-native (ULPFEC/FlexFEC via SDP), which is a different codec with browser-controlled recovery, no elastic window, no ACK feedback. The RTP tier keeps its existing NACK/RTX. |
| B. **Unreliable DataChannel + existing "data" pipeline** | **chosen (web tier)** | `web/stream/video/pipeline.ts` already ships a full `"data"` input family (`DepacketizeVideoPipe → VideoDecoderPipe(WebCodecs) / OpenH264 / MediaSource → renderer`), today fed by the TCP WebSocket transport. Feeding the same pipeline from an unordered/unreliable SCTP DataChannel gives UDP semantics end-to-end, a place for Tetrys symbols, and jitter-buffer bypass — converging with M5's WebCodecs direction (DCV proved this shape with WebSocket+WebCodecs; we upgrade the wire to lossy-UDP-like + FEC). |
| C. WebSocket transport | no-op | TCP already retransmits; FEC adds pure overhead. |
| D. Native client custom UDP (U5) | **chosen (ultra tier, later)** | `fec.rs` runs on both ends natively; shares the symbol wire format below. |

## 2. Channel and wire format

Two new DataChannels on the existing WebRTC transport (creation pattern:
`transport/webrtc/mod.rs` input-channel table):

| channel | ordered | reliability | direction | carries |
|---|---|---|---|---|
| `video_fec` | false | maxRetransmits: 0 | host → client | FEC symbols |
| `video_fec_ack` | true | reliable | client → host | window ACKs |

A dedicated raw ACK channel avoids touching the `general` protocol enum
(`common/src/api_bindings.rs` + TS bindings) in phase 1.

### Symbol messages (`video_fec`, binary, little-endian)

```
Source symbol:
  offset 0  u8   kind = 0
  offset 1  u32  seq            (fec.rs Symbol::Source.seq)
  offset 5  ...  chunk payload (see chunk layer)

Repair symbol:
  offset 0  u8   kind = 1
  offset 1  u16  repair_seq     (Symbol::Repair.repair_seq)
  offset 3  u32  window_base
  offset 7  u32  window_end
  offset 11 ...  combination payload (length-prefixed internally by fec.rs)
```

Coefficients are derived from `(repair_seq, src_seq)` on both ends
(`gf_coeff`), so nothing else ships — this is why the repair header is 11
bytes total.

### Chunk layer (source-symbol payload)

Encoded frames reach hundreds of KB (IDR at 20 Mbps); SCTP messages should
stay well under fragmentation pain and loss granularity should stay fine. Each
encoded frame is chunked to **≤ 1200 B** (mirrors `RTP_OUTBOUND_MTU`):

```
  offset 0  u32  frame_id       (monotonic)
  offset 4  u16  chunk_index
  offset 6  u16  chunk_count
  offset 8  u8   frame_type     (0 = delta, 1 = key — matches DepacketizeVideoPipe)
  offset 9  u32  timestamp_us   (truncated, matches existing 5-byte data header)
  offset 13 ...  Annex-B fragment
```

One chunk = one FEC source symbol. Client order: FEC decode → chunk
reassembly (complete `frame_id`) → `submitDecodeUnit` into the existing
pipeline. `DepacketizeVideoPipe`'s current 5-byte header is subsumed by the
chunk header; the new `FecReassemblyPipe` replaces it for this input.

### ACK messages (`video_fec_ack`)

```
  offset 0  u32  highest_fully_decoded   (fec.rs FecDecoder::highest_fully_decoded)
```

Sent every 32 delivered source symbols or 50 ms, whichever first. Host side
feeds `FecEncoder::acknowledge(seq)` to slide the elastic window. ACK loss is
tolerable (window just stays larger until the next ACK); the channel is
reliable mainly for simplicity.

## 3. Parameters and adaptivity

- Initial: `FecConfig::default_streaming()` — 1/8 redundancy (~12.5%),
  window 64 symbols / 512 KiB. Window is hard-capped at 128 symbols by the
  Cauchy-coefficient constraint (`fec.rs` fix #2) — at 1200 B chunks that is
  ~150 KB of in-flight protection, several frames at 1080p60 mid-bitrate.
- **Adaptive ratio is Gate B/D work** (ROADMAP M2 item), not phase 1: the RR
  loss fraction already reaches the transport (RTCP callback); a second
  consumer alongside the CC mailbox can drive `set_redundancy` once we have
  replayable traces to tune against. Fixed-20%-style overhead is exactly what
  Parsec BUD does; beating it is the point of adaptivity — but only measured.
- Sunshine-side FEC stays as is in phase 1; the roadmap's "고정 20% 탈피"
  applies to this channel's ratio first (host FEC applies to the Moonlight
  RTP leg, which this path replaces for video payload delivery).

## 4. Loss semantics and recovery

- `FecDecoder::push_symbol` events map as: `Recovered` → reassembly as if
  received; `LossSpan { from, to }` → chunks permanently lost. If a lost span
  intersects a frame, that `frame_id` is undecodable: the client marks the
  stream dirty and requests a keyframe through the same needs-IDR control the
  data transport already uses (open item #2 confirms the exact message).
- Key frames: no special-casing in the FEC layer — a key frame's chunks are
  ordinary source symbols. The existing IDR-supersedes-queue logic stays in
  the sender queue, upstream of chunking, so a superseded frame is simply
  never chunked.
- Decoder bounds: `FecDecoder::new(max_symbols=128, max_bytes)` caps memory;
  stale repairs outside the window are ignored (pinned by fec.rs tests).

## 5. CC interaction

Repair bytes are real wire bytes: they ride the same task that writes source
chunks, so a per-frame send-timing feed for this path (equivalent to the
`CcContext` hook in `sender.rs`) keeps `on_frame` service time honest, and
repair overhead automatically counts toward the congestion signal. Wiring the
data-video sender into CC re-uses `CcShared` unchanged (`publish_target_for`
etc. are transport-agnostic); this is part of phase 2, not a new mechanism.

## 6. Phasing

1. **P1 — correctness, no loss**: streamer `video_fec` sender behind a
   feature flag (default off; RTP track stays the default video path), JS
   `FecDecodePipe` + `FecReassemblyPipe` (pure-JS GF(256); ≤128-column
   Gaussian elimination is small — WASM only if profiling demands), loopback
   E2E: identical frames, zero recovery events.
2. **P2 — loss recovery**: `tc netem` loss on the benchmark rig; verify
   recovery rate vs ratio, LossSpan→IDR path, ACK window slide under
   reordering; CC feed for the data sender.
3. **P3 — adaptivity + default flip decision**: ratio driven by measured
   loss; A/B vs RTP-track path in Gate B cells; only then decide the default.

## 7. Open items

1. Symbol/generation guard for stream re-setup mirrors the CC ghost-writer
   issue (cc-wiring.md): the `video_fec` sender task needs the same
   generation-tag treatment from day one.
2. Confirm the data-transport needs-IDR message shape (WS transport already
   has one; reuse for the DataChannel path).
3. `video_fec` on Safari: DataChannel maxRetransmits support matrix — verify
   before advertising the path (pipeline negotiation already falls back to
   `videotrack`).
4. Web worker placement: FEC decode belongs in the existing worker pipes
   (`WorkerVideoDataSendPipe` family) to keep GF math off the main thread.
