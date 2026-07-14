# Tetrys FEC transport framing (U2)

**Status**: P1 구현 완료 (2026-07-14) — streamer 송신부
(`transport/webrtc/fec_wire.rs`, `fec_sender.rs`, mod.rs 채널, video.rs 탭)
+ 웹 수신부(`web/stream/video/fec.ts` 코덱 미러, `fec_wire.ts`,
`fec_decode_pipe.ts`, 전송/설정 배선) + Rust↔TS 교차 벡터
(`tests/fixtures/fec_vectors.json`). §8 P1 구현 노트 참조. 라이브 손실
복구 검증(P2)과 적응 비율(P3)은 게이트 순서대로. **Date**: 2026-07-14.
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

1. [resolved P1] Symbol/generation guard for stream re-setup — implemented
   day one: `fec_generation: AtomicU32`을 `WebRtcVideo::setup`이 bump하고,
   sender task는 매 송신 전 자기 세대와 비교해 stale이면 종료 (cc-wiring.md
   ghost-writer와 동일 계열; 테스트 핀).
2. [resolved P1] needs-IDR 메시지: `video_fec_ack` 채널의 1-byte `0x00`
   (아래 §8). 수신 시 호스트는 기존 `WebRtcVideo::needs_idr` AtomicBool을
   세운다 — RTP 경로의 PLI 응답 메커니즘을 그대로 재사용.
3. `video_fec` on Safari: DataChannel maxRetransmits support matrix — verify
   before advertising the path (pipeline negotiation already falls back to
   `videotrack`).
4. Web worker placement: FEC decode belongs in the existing worker pipes
   (`WorkerVideoDataSendPipe` family) to keep GF math off the main thread.
   P1은 main-thread `FecDecodePipe`로 출하(파이프 레지스트리에 등록돼 있어
   worker 합성 슬롯은 준비됨).
5. Rust `FecDecoder`의 `seen_repair_keys`는 무한 성장(테스트/M6 전용 —
   프로덕션 디코더는 현재 TS뿐). TS 쪽은 256-엔트리 lazy pruning을 넣었다.
   M6 cdylib이 fec.rs 디코더를 프로덕션 투입할 때 같은 pruning을 이식할 것.

## 8-A. P2-lite 라이브 손실 검증 결과 (2026-07-14, 루프백 + clumsy)

Gate B 리그 부재로 P2를 로컬 근사(P2-lite)로 축소해 실검증했다. 셋업:
릴리스 서버 루프백 스트림, `enableVideoFec=true`(URL 파라미터), clumsy
(WinDivert)로 루프백 UDP 50000–50100에 드랍 주입("loopback은 outbound로만
필터" — clumsy config.txt).

| 관찰 | 결과 |
|---|---|
| FEC-only 렌더 경로 | 라이브 동작 — 이 모드의 유일한 렌더 경로가 video_fec이므로 영상 표시 자체가 체인 전체(SUBSCRIBE→청킹→비신뢰 채널→재조립→디코드)의 증거 |
| 5% 드랍 | 대체로 매끈, 간헐 키프레임 리프레시 |
| 20% 드랍 (중복률 12.5% 초과) | 끊김 빈발하나 **화면 깨짐 0** — 불완전 프레임은 제출되지 않고 폐기+IDR 요청되는 설계가 라이브로 성립 |
| needs-IDR 복구 루프 | 서버 로그 "Requesting IDR frame on behalf of DR" — FEC 세션 2개에서 39+36회 순환 (디코더 백로그 IDR과 합산 수치; 분리 계수는 리그 몫) |
| 손실 중 재접속 | clumsy 활성 상태에서 새 세션 ICE ~1.1 s 연결 |

**남은 P2(리그 필요)**: 복구율 vs 중복률 정량화(Recovered/LossSpan 카운터),
재정렬 하 ACK 윈도 슬라이드 검증, 적응 비율(P3). 대외 성능 주장 근거로는
사용 불가 — 메커니즘 검증(로드맵 원칙)이다.

## 8. P1 구현 노트 (2026-07-14)

- **활성화 프로토콜(설계 추가분)**: 서버는 채널을 항상 만들되 sender task는
  휴면. `video_fec_ack`의 1-byte 메시지로 제어 — `0x01` SUBSCRIBE(활성화),
  `0x00` NEEDS_IDR, 4-byte LE u32 = ACK(`highest_fully_decoded`). 클라 플래그
  `enableVideoFec`(기본 false, `web/default_settings.ts`)가 SUBSCRIBE 송신을
  게이트하므로 스트리머 쪽 권한/설정 변경 없이 기본-off가 성립한다. 휴면 시
  호스트 오버헤드는 프레임당 AtomicBool 로드 1회.
- **메시지 크기**: source 메시지 ≤ 1200 B (chunk 헤더 13 B + 단편 ≤ 1182 B).
  repair 메시지는 내부 길이 프리픽스 때문에 수 바이트 초과 가능 — 허용.
- **P1 모드의 클라 렌더링**: `enableVideoFec=true`면 FEC 파이프라인이
  videotrack 파이프라인을 **대체**한다(RTP 트랙은 서버에서 계속 흐르지만
  렌더러에 붙지 않음 — P1 검증 모드의 의도적 이중 송신). P3에서 기본 경로
  판정 시 재검토.
- **경계 가드(리뷰 산출)**: 디코더는 window 길이 > 128인 repair를 양쪽
  언어 모두 거부(디덥 키 기록 전 — 테스트 핀). `window_end` 계산과 디코더
  window 순회는 u32 wrap-safe (`wrapping_add`/`seq != end` 순회, 핀 테스트
  2건). ACK 50 ms 암은 심볼 도착과 독립인 타이머로 발화. sink 송신 실패 시
  sender task 즉시 종료(무한 스핀 방지).
- **교차 벡터**: `tests/fixtures/fec_vectors.json` (LCG 시드 0x00C0FFEE,
  redundancy 1/4, 프레임 [500, 2500, 1183, 0] B) — Rust 테스트가 재생성
  일치를 핀하고, TS 테스트가 (a) 동일 시나리오 인코딩의 바이트 동일성,
  (b) drop된 source를 repair로 복구해 4프레임 바이트 동일 복원을 검증.
- **큐 의미론**: FEC sender 큐(용량 4)는 sender.rs와 동일하게 key frame이
  큐 전체를 대체한다 — 최대 소실 4 프레임(전부 IDR로 대체되므로 화면
  일관성 훼손 없음), 상한은 컴파일타임 상수.
