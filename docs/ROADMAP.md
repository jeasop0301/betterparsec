# betterparsec — 로드맵

각 마일스톤은 **실행 가능한 산출물 + 검증 기준**을 갖는다. 순서: 먼저 계측을 믿을 수 있게 → 스트림이 돌게 → WARP 없이 붙게 → 뭉개짐을 잡게 → 화질을 올린다.

호스트: 집 Windows PC(GPU) + Sunshine. 개발/브리지: 이 리포. 프론트: `web/`.

---

## 실행 스파인 — 감사 게이트 × ultra 성능 코어 병합 (2026-07-14)

2026-07-13 성능·보안 감사(docs/audits/2026-07-13-performance-security-audit.md)의
잠금 실행 순서(Gate A–E)와 리서치 04/05의 ultra 성능 코어(프레임딜레이 CC ·
Tetrys FEC · 서브프레임 슬라이스 · QU · 네이티브 클라이언트)를 하나의 스파인으로
병합한다. **ultra 성능 코어는 must-do다(owner, 2026-07-14).** 두 로드맵은
충돌하지 않고 역할이 다르다:

- **감사 게이트 = 진실·주장 순서.** 성능 주장, 튜닝 결정, 파라미터 선택은 게이트
  순서를 따른다. 신뢰 가능한 측정 전에 수행한 최적화는 대외 주장하지 않는다.
- **ultra 코어 = 빌드 트랙.** 순수 로직·설계·클라이언트측 배선처럼 게이트 없이
  단위검증 가능한 기계는 게이트를 기다리지 않고 병렬로 만든다. 단 **활성화
  기본값·튜닝·대외 주장**은 해당 게이트 통과 후에만.

### 게이트 진행 상태

| 게이트 | 내용 | 상태 |
|---|---|---|
| A. 보안·진실 기반 | TLS pin 통합, paired 회귀, analyzer 통합, strict profile | TLS pin [x] · analyzer 통합 [x] · strict profiles [x] · **stock/Foundation paired 라이브 회귀 [x]** (2026-07-14: stock 상대 capability 게이트 `unsupported` 정상 + Foundation ACK 빌드 상대 paired H.264 스트림·0x5509 왕복 5회, 재페어링 없음) · 워킹트리 커밋 [x] |
| B. 첫 신뢰 베이스라인 | 분리된 host/client/router, H.264 1080p60 direct UDP, 결정적 트레이스, clean LAN + 20→8→15 Mbps, ABBA 5회, 무효 run 거부 | [ ] 전부 (2-machine 셋업 필요) |
| C. 지연 분리 | sender/PCAP/browser/decode/present 상관, 렌더 경로 비교, 지터버퍼 제어, 외부 input-to-photon | [ ] |
| D. 컨트롤러·인코더 최적화 | ABR에 queue/write/freshness 피드백, TWCC(재생 가능 트레이스 후), NVENC LL 매트릭스, Pareto 선택 | CC 기계는 완성(U1) — 튜닝·판정은 Gate B/C 이후 |
| E. 코덱·콘텐츠 효율 | HEVC 4:4:4 / AV1 truth table, 콘텐츠 인지 배분 | [ ] (M3과 동일) |

### 현장 이슈 백로그 (2026-07-14 라이브 세션, owner 보고)

1. **무증상 스트림 스톨 → 정지화면** — 인천 원격 + 로컬 세션에서 재현.
   스트림이 조용히 멈추고 마지막 프레임이 정지화면으로 남는다: 클라 측
   스톨 감지(프레임 수신 워치독)·자동 복구 사다리(IDR 요청 → ICE
   restart → 전체 재접속)·사용자 가시 상태 표시가 전부 부재.
   → M4 재접속 안정성 + hard gate 복원력(freeze/min, 15분 disconnect 0)
   직결. 원인 분리는 Gate C 상관 계측 전이라도 클라 수신 타임스탬프
   워치독만으로 감지/복구 가능 — 우선 구현 후보.
2. **커서 미해결 + immersive 모드** — 설계는 완료
   (docs/design/cursor-channel.md: Parsec=모양 채널+클라 렌더,
   DCV=호스트 권위 자동 lock/unlock. P1=`cursor` DataChannel 자동 모드
   전환, P2=모양 채널 zero-latency 커서). 구현 착수 필요. 추가로
   **immersive 모드**(전체화면 + pointer lock + Keyboard Lock
   Win/Alt-Tab 캡처 일괄 토글) 요구 — cursor P1과 같은 클라 상태머신에
   함께 배선.
3. **UDP-only 접속 — TCP fallback 미동작** — 현재 UDP가 막힌 망에서는
   접속 자체가 안 되고 WARP(1.1.1.1)로만 우회 가능. 목표는 UDP 기본 +
   실패 시 자동 fallback = **M1 그 자체**. 소스 확인(2026-07-14):
   WebSocket 전송 자체는 구현돼 있음(`SetTransport(WebSocket)`,
   `permissions.allow_transport_websockets` 게이트, stream/index.ts 전송
   폴백 체인) — WebRTC 실패 → WS 자동 강등이 실동작하는지·권한 기본값이
   막는지부터 진단. TURN-over-TCP(443) coturn 배포는 별도 잔여.

### ultra 성능 코어 트랙 (must-do)

| 트랙 | 순수 로직/설계 | 배선/구현 | 활성·판정 게이트 |
|---|---|---|---|
| U1. 프레임딜레이 CC (Pudica류) | [x] cc.rs 26 tests | [x] 송신루프 배선 + `min(abr, cc)` 합성 + 세대 가드/손실 모멘텀 수정 (2026-07-14, docs/design/cc-wiring.md, +12 tests) | Gate B 벤치 셀에서 송신측 신호 대역폭 판정 → 부족 시 TWCC/수신측 피드(Gate D) |
| U2. Tetrys 슬라이딩윈도우 FEC | [x] fec.rs 47 tests (GF(256), MDS sweep) + u32 wrap/윈도캡 가드 3핀 | [x] 프레이밍 설계(video_fec DataChannel) + **P1 배선 완료** + **P2-lite 라이브 손실 검증 통과** (2026-07-14, 루프백+clumsy 5%/20%: FEC-only 렌더 라이브, 20%에서도 화면 깨짐 0, needs-IDR 복구 루프 39+36회 순환, 손실 중 재접속 ~1.1 s — fec-framing.md §8-A) | Gate B/D — P2 정량화(복구율 vs 비율, 재정렬 ACK 윈도), P3 적응 비율 A/B·기본 경로 판정 |
| U3. 서브프레임 슬라이스 | [x] 제약 인벤토리 + [x] 콜백 granularity 조사(1콜백=1프레임 확정, slice-qu-constraints.md §5) | [x] **소형 레버**: `slices_per_frame` C-경로 래퍼 갭 수정 — capability bits 24-31 패킹, 기본 1 비트-동일 핀(2026-07-14, streamer 261 tests). 잔여: [ ] 활성값 튜닝(P0 RTX 4070 슬라이스 지연 실측 선행) · [ ] per-slice DU 전달(depacketizer 패치, 중규모) · [ ] 호스트 슬라이스 모드+per-slice 송신(포크) | Gate C에서 인코드→프레젠트 겹침 이득 실측 |
| U4. QU build-to-lossless | [x] 채널·타일 프로토콜 설계 (2026-07-14, docs/design/qu-protocol.md — `video_qu` 단일 reliable 채널, PNG 타일+CRC 자가검증, epoch/invalidate, 오버레이 캔버스 합성 결정, localhost 릴레이 인터페이스) | [x] **P1 완료** (2026-07-14): 스트리머 릴레이(qu_relay, subscribe 게이트·세대 가드·4 MiB 바운드·재접속 replay) + 클라 오버레이(순수 상태/DOM 분리, CRC 자가검증, 디코드-중-invalidate 레이스 가드) — sonnet 리뷰 8건 전부 수정·핀, streamer 251·웹 103 그린. 잔여: [ ] 호스트 무손실 타일 경로(포크 P2, 릴레이 포트 config 플러밍 포함) | Gate E — 데스크톱 모드 픽셀-퍼펙트 |
| U5. 네이티브 클라이언트 (ultra 티어) | [x] 리서치 05 §3 아키텍처 | [x] moonlight-qt 포크 스파이크 완료 (m6-native-spike.md, 접합 = Rust cdylib 사이드카 Option-3) → [x] **W1 완주** (2026-07-14): `transport-core` 추출 + 순수 `VideoReceiver`(TS 미러, 15 tests) + `client-transport` cdylib(C ABI ct_receiver_*/ct_start/FrameQueue, 헤더) + **WebRTC/signaling 클라 라이브 검증** — ct-probe가 브라우저 없이 699프레임/15s 수신, `CT-PROBE-OK` (StartStream 타이밍 교착·상태 역전 130ms 두 함정 해소, 스파이크 §F 갱신). 잔여: W2 셸 글루(Qt 6 빌드 환경 ~2–3h) 또는 통합 앱 셸 직행 | Gate C 외부 계측으로 지연 왕좌 판정 |

- **착수 조건 변경**: 리서치 04 §5의 "M6는 웹 클라 gate 통과 후"는 **owner 티어
  판정(리서치 05 결정 0, 2026-07-14)이 대체한다** — 네이티브는 ultra 티어 제품
  그 자체이므로 조건부(선택)가 아니라 must-do. 단 지연 우위의 **대외 주장**은
  여전히 Gate C 외부 계측 이후.
- U1–U2는 스트리머 단독(호스트 포크 불필요), U3–U4는 Sunshine 포크 필요(하드
  블로커 분류: slice-qu-constraints.md §4), U5는 별도 클라이언트 트랙.

### 다음 액션 순서 (2026-07-14 갱신)

완료: 워킹트리 커밋(1), U2 설계+P1 배선(3), U3 조사(4), U4 프로토콜 설계,
U5/M6 스파이크, f1-ack 소스 확인+0x5509 호스트 패치.

1. ~~라이브 paired 회귀~~ — 2026-07-14 완료 (stock + Foundation ACK 빌드,
   0x5509 왕복 5회 3면 교차 검증 — f1-ack.md step 4). 잔여 라이브 항목:
   U2 P1 손실-0 프레임 동일성 스모크(enableVideoFec=true)는 다음 세션에
   5분 항목으로.
2. f1-ack 클라 플러밍 — moonlight-common-c/rust 패치에 0x5509 수신 훅 +
   상태머신 arming (헤드리스 가능; vendor 재생성 필요).
3. U4 P1 — `video_qu` 채널 + localhost 릴레이 + 클라 오버레이(스트리머 단독,
   합성 타일 인젝터로 테스트).
4. ~~M6 W1~~ — **완주** (2026-07-14): ct-probe 라이브 검증 `CT-PROBE-OK` —
   브라우저 없이 로그인→시그널링→WebRTC answer→video_fec 구독→FEC
   디코드로 699프레임/15s(~58fps) 수신. **후속 설계 완료**:
   docs/design/unified-app-architecture.md — 통합 양방향 앱 D1–D10 결정
   (단일 바이너리 역할 스위치, Sunshine 서브프로세스 유지, web-server
   임베드, raw D3D11 프레젠트, 포크 W2 보류). **A0 슬라이스 1 라이브 검증**
   (2026-07-14, owner 확인): `betterparsec.exe`(app-native, egui 셸 +
   client-transport 세션 + 프레임 펌프)가 실스트림에서 프레임 카운트
   정상 상승. **슬라이스 2 기계 완성** (2026-07-14, 헤드리스 검증):
   FFmpeg 조달 핀(tools/bootstrap-ffmpeg.ps1, BtbN n7.1 lgpl-shared
   SHA-256) + libclang 핀(tools/bootstrap-libclang.ps1, PyPI 18.1.1 휠;
   머신 LLVM 22 아래서 bindgen 0.70이 opaque 구조체를 뱉는 문제 우회) +
   app-native `video` 피처: ffmpeg-sys-next D3D11VA hwaccel(+sw 폴백),
   NV12/YUV420P→RGBA, 디코드 실패→RxCore needs-IDR 래치, interim egui
   프레젠트(첫 IDR 게이트 포함). 검증 = openh264 합성 스트림 30 AU
   헤드리스 디코드 + 픽셀 비균일 assert(3 tests 그린), non-video 빌드
   무손상, clippy 클린. 잔여 = **라이브 첫 픽셀**(owner 스모크, 5분) →
   슬라이스 3: raw FLIP_DISCARD 표면(D5).
5. Gate B 2-machine 첫 신뢰 run — 이후 U1 CC 신호 판정, U2 P2 손실 복구
   실측·비율 튜닝, U3/U5 지연 계측이 전부 이 위에서 순차 판정된다.

---

## Browser-native product wedge

Moonlight 공식 FAQ는 브라우저가 raw TCP/UDP socket API를 제공하지 않아 순수 웹
클라이언트를 제공하지 않는다고 명시한다. 반면 Parsec은 Chromium 기반 웹 앱을
제공하지만, 공식 문서에서도 downloadable app보다 성능과 안정성이 낮고 네트워크 및
저수준 최적화 제어가 제한된다고 설명한다. 따라서 BetterParsec의 차별점은 단순히
"브라우저에서 열린다"가 아니라 다음의 측정 가능한 조합이어야 한다.

- 설치 없이 URL에서 첫 프레임까지 도달하고, 재방문 시 재페어링을 요구하지 않는다.
- selected ICE pair와 direct/relay, RTT, 후보 protocol을 기록해 접속 성공을 경로별로
  분리한다.
- 브라우저 decoder implementation과 CPU/GPU/power 증거를 함께 수집해 hardware decode를
  추정이 아니라 증명한다.
- 같은 실제 wire bitrate에서 화질, input-to-photon, freeze와 복구 시간을 native Parsec과
  Parsec 웹 앱 양쪽에 대해 별도 비교한다.
- Keyboard Lock, gamepad, audio/microphone, clipboard, secure-context 제약을 브라우저별
  지원표와 자동 smoke test로 관리한다.

제품 퍼널 gate는 connection success(직접/릴레이별), link-to-first-frame p50/p95,
재접속 성공률, 15분 disconnect/freeze, 입력 기능 성공률이다. 성능 gate는 아래 Parsec
hard gate를 그대로 사용한다. 이 둘을 통과하기 전에는 "Parsec보다 빠른 브라우저"라고
광고하지 않는다.

근거: Moonlight FAQ
<https://github.com/moonlight-stream/moonlight-docs/wiki/Frequently-Asked-Questions>,
Parsec web app 문서
<https://support.parsec.app/hc/en-us/articles/32381650129300-Use-the-Web-App-browser>.

---

## P0 — truth blockers (다른 최적화보다 먼저)

**목표:** 제품의 성능 수치와 코덱 capability가 실제 전송/호스트 동작을 정직하게
나타내게 한다. 아래 항목이 닫히기 전에는 Parsec 우위나 latency 개선을 주장하지 않는다.

- [x] Moonlight의 `presentationTimeUs`를 90 kHz 값으로 잘못 해석하던 RTP timestamp
  변환 수정. 60 fps timestamp 간격이 약 16,667 tick이 아니라 약 1,500 tick이 되도록
  단위 회귀 테스트 추가.
- [x] Moonlight의 0.1 ms 단위 `frameHostProcessingLatency`를 정수 나눗셈으로
  잘라내던 변환 수정. 100 us 해상도를 보존하는 회귀 테스트 추가.
- [x] 수정 후 same-PC H.264 smoke에서 browser RTP jitter가 약 168 ms에서 2-4 ms,
  jitter-buffer target이 약 11 ms, interval jitter-buffer delay가 0.03-0.04 ms로
  내려가고 loss/drop/freeze가 모두 0인지 확인.
- [ ] RTP timestamp를 packet capture로 교차 검증하고 host latency를 외부 marker/고속
  카메라 계측과 대조한다.
- [ ] AV1/HEVC/4:4:4 capability를 host encode, bridge transport, browser decode의
  3단 truth table로 만들고 실제로 끝까지 동작하는 조합만 광고한다.
- [x] encoder bitrate 요청의 client state를 `sent_unacknowledged`로 제한하고 Foundation
  host log와 대조해 실제 NVENC apply를 별도로 증명한다.
- [~] request-id 기반 protocol ACK: **라이브 검증 통과** (2026-07-14) —
  Foundation ACK 빌드(e110872d+양 패치, MSYS2 UCRT64 컴파일)로 paired
  H.264 세션에서 0x5506→0x5509 왕복 5회 연속: 클라
  `Applied{Dispatched}` (8500/6141/5219/3770/1000 Kbps) ↔ 호스트 로그
  capture-thread apply + NVENC 재설정 (6800/4912/4175/3016/800, FEC 20%
  차감) 3면 일치. Tier A 시맨틱 확인: ACK는 검증·디스패치 증명이며
  NVENC 실측은 호스트 로그가 담당. 잔여: step 5 ack_latency 벤치 상관 +
  step 6 Tier B 판정 (f1-ack.md).
- [x] patched Moonlight dependency를 고정 revision + repository-owned patch + bootstrap/CI
  구조로 전환해 외부 로컬 작업 트리 없이 clean clone을 재현한다.
- [ ] RTX 4070 ULL/슬라이스 인코드 지연 자체 실측(리서치 04 §3-1 문헌 상충 해소;
  U3 착수 전 필요).

**검증:** 내부 수치가 packet capture/외부 계측과 각각 ±2 ms 또는 ±10% 안에서
일치해야 한다. 기존 2026-07-13 최초 same-PC run의 host latency와 RTP jitter는 단위
버그의 영향을 받았으므로 성능 baseline에서 제외하고 연결 안정성 smoke로만 보존한다.

## M0 — 빌드 & 스트림 베이스라인
**목표:** 포크가 그대로 빌드되고, 집 Sunshine ↔ 브라우저 스트림이 한 번 붙는다(WARP/변형 전).

- [x] `tools/bootstrap-dependencies.{ps1,sh}`로 pinned moonlight-common-rust/common-c clone+patch
- [x] Rust 브리지 빌드 (nightly-2026-02-13) 및 Windows C backend 확인
- [x] `web/` 프론트 빌드 (`npm ci && npm run build`)
- [x] Windows Sunshine + 브리지 실행, 브라우저 로그인/페어링/H.264 1080p60 스트림
- **검증:** 브라우저 상태 wipe(시크릿창/캐시삭제) 후 **재로그인만으로 재페어링 없이** 재연결. 스트림 프레임 표시.

## M1 — WARP 없이 접속 (feature #2)
**목표:** 제약망에서 WARP off로 붙는다. **현장 확인(2026-07-14, owner)**:
UDP 차단 망에서는 접속 자체가 실패하고 WARP(1.1.1.1)로만 우회 가능 —
"UDP 기본 + 실패 시 TURN-over-TCP(443) 자동 fallback"이 M1의 완료 조건이다
(현장 이슈 백로그 #3).

- [ ] 공인 VPS에 coturn, TURN-over-TLS on **TCP 443**, `use-auth-secret`(HMAC TTL)
- [x] `ice_server_script` helper 작성 → 세션별 coturn REST 단기 credential 발급
- [x] `web/`에 `iceTransportPolicy:'relay'` 설정 경로 + 클라 ICE 구성
- [ ] 방화벽 규칙으로 "UDP 차단 + 443만 허용" 환경 재현
- **검증:** WARP off + UDP 차단 상태에서 접속 성공. 릴레이 **RTT/jitter/throughput 실측** → TCP-릴레이 지연 실사용성 판정(핵심 리스크). 실패 시 M2 이전에 전송 재설계.

## M2 — 적응형 비트레이트 (feature #1, flagship)
**목표:** 대역 급락 시 뭉개지는 대신 비트레이트가 따라 내려간다.

- [ ] TWCC(transport-cc) 피드백 활성 (SDP 협상 + webrtc-rs) — Gate D: 재생 가능
  트레이스 테스트 후에만 (감사 잠금 순서)
- [x] REMB + Receiver Report 손실 → bounded target_kbps (급락 즉시, 회복 완만)
- [x] 프레임딜레이 CC 순수 컨트롤러(cc.rs, 26 tests) + 송신루프 배선 +
  `min(abr, cc)` 합성 — U1, docs/design/cc-wiring.md (라이브 검증은 Gate B)
- [x] Tetrys 슬라이딩윈도우 FEC 순수 코덱(fec.rs, 47 tests) + P1 전송
  프레이밍/배선(video_fec DataChannel, 기본 off — fec-framing.md §8); 라이브
  손실 복구 실측은 P2(Gate B rig)
- [ ] CC 신호(송신측 service time) 대역폭 판정 — Gate B 벤치 셀; 부족 시
  TWCC/수신측 타임스탬프 피드로 같은 on_frame API에 교체
- [ ] 적응형 FEC 비율(고정 20% 탈피) A/B — U2 배선 후
- [x] WebRTC target → bridge apply task → moonlight-common-c `0x5506` sender
- [x] 10% hysteresis, 900ms 일반 제한, 20% 이상 하향은 즉시, 실패 시 baseline 미갱신
- [x] `DYNAMIC_BITRATE_V1` capability gate + stock Sunshine 기본 미지원 처리
- [x] Foundation Sunshine에 capability patch 적용 후 Windows host build/stage
- [x] staged Foundation으로 실제 BetterParsec paired stream을 연결하고 H.264 NVENC dynamic bitrate 적용 검증
- [x] benchmark runner가 Foundation start/host/stdout/stderr/cleanup log를 run artifact로 수집
- [x] encoder request-id ACK로 client telemetry에서 dispatch 결과를 직접
  확인 — 2026-07-14 라이브 왕복 5회 (Tier A: 인코더 apply 실측은 여전히
  호스트 로그 대조; Tier B 판정은 f1-ack step 6)
- **검증:** 호스트에서 `tc netem`으로 대역/지터/손실 주입 → target 그래프 추종 + frame drop 억제. 동일 조건 Parsec과 뭉개짐 A/B.

## M3 — 코덱 / 화질 (feature #3)
**목표:** 같은 비트레이트에서 더 선명.

- [ ] AV1 협상·HW 디코드 실동작 검증(`chrome://gpu`), 안 되면 HEVC/H.264 폴백 유지
- [ ] HEVC 4:4:4 협상 경로 노출(색 텍스트 fringing 제거)
- [ ] `nvenc_vbv_increase` 등 rate-control 튜닝(모션 스파이크)
- **검증:** 동일 delivered bitrate에서 AV1 4:2:0 / HEVC 4:2:0 / HEVC 4:4:4 정지·모션·색텍스트 채점.

## M4 — 하드닝 + 세션 UX (owner 요구로 선택 → 필수 승격, 2026-07-14)
- [ ] **스톨 감지·복구 사다리** (현장 이슈 #1): 클라 프레임 수신 워치독 →
  IDR 요청 → ICE restart → 전체 재접속 에스컬레이션 + 가시 상태 표시.
  hard gate 복원력(freeze/min, 15분 disconnect 0)의 전제.
- [ ] **커서 P1** — `cursor` DataChannel + 호스트 권위 자동 lock/unlock
  (cursor-channel.md §3 P1, 설계 완료) → [ ] **P2** 모양 채널
  zero-latency 커서
- [ ] **immersive 모드** — 전체화면 + pointer lock + Keyboard Lock 일괄
  토글 (cursor P1 클라 상태머신에 배선)
- [ ] 보안 감사(ARCHITECTURE §보안 6항: 서명·짧은토큰·상수시간·replay·안전인코딩·revocation)
- [ ] 입력(Gamepad/Keyboard Lock) secure-context 동작, 오디오, 재접속 안정성
- [ ] upstream 병합 전략 정리

## M5 — 서브프레임 슬라이스 + QU (ultra 코어 U3·U4, must-do)
**목표:** 인코드→전송→디코드 겹침으로 프레임 내 지연을 깎고, 정지 화면을
픽셀-퍼펙트로 만든다(데스크톱 모드의 구조적 "뭉개짐" 해결).

- [x] moonlight-common 포크의 슬라이스 콜백 granularity 조사 — 2026-07-14
  해소(slice-qu-constraints.md §5): 1 콜백=1 완성 프레임이 프로토콜 구조상
  항상 성립. per-slice 전달 = depacketizer 패치(FEC-block 경계를 DU 경계로,
  중규모·기존 패치 확장). 부수 발견: `CAPABILITY_SLICES_PER_FRAME`은
  인코더 전용 레버(포크 불필요·저비용)이고 Rust C-경로 래퍼가
  `slices_per_frame`을 무시하는 갭 존재(래퍼 패치 소규모).
- [ ] Sunshine 포크 NVENC 슬라이스 모드 + 스트리머 per-slice 즉시 송신. 깨지는
  4개 불변 재설계: IDR 검출, PLI 응답, IDR 큐 클리어, RTP marker bit
  (slice-qu-constraints.md §2)
- [ ] QU 전용 reliable DataChannel(스트리머 단독) + 호스트 무손실 타일 인코드
  경로(포크, 중규모) + 클라 합성 방식 결정
- **검증:** Gate C 상관 계측에서 슬라이스 on/off 프레임 내 겹침 이득 실측. QU는
  모션 정지 후 정지 화면 픽셀-퍼펙트 + 정적 대역 ~0. 하드 블로커 분류는
  docs/design/slice-qu-constraints.md §4를 따른다.

## M6 — 네이티브 클라이언트 (ultra 티어, must-do)
**목표:** 지연 왕좌 — CUVID 4:4:4 디코드, VRR/tearing 프레젠트, WASAPI
exclusive, Raw 입력. 웹 클라는 간편/호환 티어로 유지(동일 백엔드).

**제품 방향 (owner, 2026-07-14)**: 최종 형태는 **Sunshine+Moonlight 통합
양방향 단일 앱**(호스트도 되고 클라도 되는), UI·알고리즘 전면 자체화 —
웹/네이티브 공통 전역 개편. 따라서 moonlight-qt 포크(Option-3)는 **참조
구현·지연 검증 비히클**이지 제품 셸이 아니다. 투자 우선순위는 UI-무관
엔진 조각(transport-core, client-transport, cc/fec/cursor 알고리즘)에
두고, 포크 전용 글루는 최소화한다. "(장기) Rust 네이티브 전환 판단"은
판단이 아니라 **확정된 종착지**로 승격 — 시점만 Gate C 검증 후 결정.

- [x] moonlight-qt 포크 스파이크: 커스텀 전송(CC/FEC/슬라이스 보존) 이식 공수
  검증 — 2026-07-14 완료, docs/design/m6-native-spike.md. 판정: "2–4주"는
  OPTIMISTIC, 현실 3–5주(~1.4–1.7k LOC). 접합 = Rust cdylib 사이드카
  (moonlight-common-c 무수정, fec.rs/cc.rs 재사용, IVideoDecoder 경계 주입,
  `LiWaitForNextVideoFrame` pull 루프 ~30 LOC 교체). 프레젠트는
  FLIP_DISCARD+ALLOW_TEARING 기존재/waitable(1)만 추가(~50 LOC), WASAPI
  exclusive 렌더러 신규(~250 LOC), CUVID는 Windows pass-0 승격 ~30 LOC.
  핀: moonlight-qt c0c4d60 / moonlight-common-c 2ea4775.
- [~] 포크 착수 W1 (2026-07-14 진행): `transport-core` 추출(fec 74.1KB+wire,
  streamer 재export로 콜사이트 불변) → 순수 `VideoReceiver`(재조립+ACK
  cadence+eviction, TS 파이프 시나리오 패리티 15 tests) → `client-transport`
  cdylib(C ABI: lifecycle/on_message/tick/poll_ack/poll_needs_idr/
  wait_frame — `LiWaitForNextVideoFrame` 대체 블로킹 큐 포함, C 헤더
  include/client_transport.h, ABI 경유 11 tests). 잔여: WebRTC 클라 +
  video_fec 구독을 on_message에 연결 — 빌드 환경(Qt 6 MSVC, ~2–3h 설치)은
  W2 착수 시 구축
- [ ] CUVID `ulMaxDisplayDelay=0` + `FLIP_DISCARD`/`ALLOW_TEARING`/waitable(1)
  프레젠트 + WASAPI exclusive + RawInputBuffer/GameInput
- [ ] (장기) Rust 네이티브(nvcodec-rs+wgpu+windows-rs) 전환 판단
- **검증:** Gate C 외부 input-to-photon 계측으로 LAN 120Hz G2G 8–12ms 가설
  (리서치 05 §3) 검증. 착수 조건 없음(owner 티어 판정으로 must-do) — 단 지연
  우위 **대외 주장**은 Gate C 통과 후.

---

## Parsec 대비 hard gates

"항상 100% 더 좋다"는 환경과 콘텐츠를 한정하지 않으면 검증 불가능하다. 대신 아래
고정 조건의 **모든 hard gate를 100% 통과**해야 우위라고 판정한다. 하나의 가중 평균으로
실패 항목을 숨기지 않는다.

- 환경: clean LAN, typical WAN(30/3 ms, loss 0.1%, 20 Mbps), bad Wi-Fi
  (30/12 ms, 1% burst loss, 10 Mbps), congested WAN(50/8 ms, 2%, 5 Mbps),
  recovery(5%→0% loss 및 5→20 Mbps).
- 실험: 동일 host/client/trace에서 후보 순서를 무작위 ABBA로 배치하고 cell당 5회.
  bootstrap 신뢰구간 전체가 기준선을 넘어야 통과.
- 화질/효율: 동일 실제 wire bitrate에서 VMAF-NEG +2 이상, 또는 동일 화질에서 총
  양방향 wire bytes 20% 이상 감소. "훨씬 효율적"은 30% 이상 감소일 때만 사용.
- latency: 외부 1,000 fps 또는 GPIO+photodiode input-to-photon 기준 LAN median
  2 ms 이상, p95 5 ms 이상 개선하고 p99는 비열등.
- 네트워크: 총 양방향 wire bytes 20% 이상, static trace는 40% 이상 감소.
- 복원력: freeze/min 30% 이상, recovery p95 25% 이상 개선하고 15분 run disconnect 0.
- 안정성: connect/disconnect 100회 crash 0, 성공률 99% 이상, start p95 개선.
- 계측 진실성: 공개할 내부 metric은 외부 ground truth와 ±2 ms 또는 ±10% 이내.

우선순위는 P0 계측 진실성 → Foundation 동적 bitrate 실증 → wire/quality/latency 자동
벤치 → AV1 4:2:0·HEVC 4:4:4·HDR의 실제 end-to-end 지원 순이다. Parsec이 현재
제공하지 않는 기능은 좋은 wedge이지만, hard gate를 대신하지 않는다.

### 지금 위치
P0에서 발견한 두 단위 오류는 수정·회귀 테스트했고, Foundation paired H.264
1080p60 stream과 dynamic bitrate의 실제 NVENC 적용까지 확인했다. 75초 JSON에서
host 처리 p50/p95는 2.203/2.272 ms, streamer 처리는 1.263/1.552 ms, decode는
0.460/0.509 ms, WebRTC jitter는 1/1 ms였고 loss/drop/freeze/NACK는 모두 0이었다.
다만 이것은 loopback smoke이지 Parsec 비교 baseline이 아니다. 다음 병목은
**실제 write 성공량·full wire byte·ICE path를 포함한 계측 진실성과 deterministic
network shaping/Parsec ABBA 비교**다.

브라우저 계측은 selected ICE pair를 표준 `transport.selectedCandidatePairId`에서
video transport 우선으로 선택하고, 명확한 단일 fallback만 허용하도록 구현했다. 이제
candidate type/protocol, direct/relay, RTT, available outgoing/incoming bitrate를 매초
benchmark sample에 포함한다. 내보내기는 schema v2로 실행 시간, 보존 구간, 샘플
sequence, source freshness, 요청 stream 설정, 실제 codec/pipeline/HDR, sanitized page URL을
기록한다. 앱/호스트 build와 driver는 아직 자동 수집하지 않으므로 추측 대신
`unknown`으로 남긴다.

새 빌드의 실브라우저 smoke에서는 H.264 1920x1080/60 연결이 유지됐고,
host/UDP-to-host/UDP direct ICE pair와 0.00 ms loopback candidate-pair RTT가 확인됐다.
최종 구간의 track write는 87 packets 성공, 실패 0이었으며 queue wait 평균은
0.084 ms, write await 평균은 0.261 ms였다. schema-v2 export는 sequence 0-45의
46개 연속 샘플과 query가 제거된 page URL을 기록했다. 이는 browser-native 계측
경로의 기능 증명일 뿐 WAN 성능이나 Parsec 우위 증거는 아니다.

2026-07-13 로컬 호스트 확인: 실행 중인 설치본은 LizardByte Sunshine
`2026.516.143833` (`14ffa6fd`)이며 Foundation 빌드가 아니다. 로그상 NVENC
H.264/HEVC/AV1은 사용 가능하지만 YUV444 encode는 미지원이다. 따라서 이 설치본은
동적 bitrate capability와 HEVC 4:4:4 검증 대상이 될 수 없고, 현재 그대로 두면
bridge가 안전하게 초기 고정 bitrate를 유지하는 것이 정상 동작이다.

Foundation Sunshine capability patch의 build/stage는 성공했다. staged binary는 SHA
`b29f747b510f416db0d8075ed23b280e50ce3cbc531b766c828dedb9c2ee7dec`, version
`2026.0713.165229.杂鱼`이며, 설치된 stock 서비스를 유지한 채
alternate base port `49000`에서 startup도 확인했다. RTX 4070 encoder probe는 H.264,
HEVC, AV1을 통과했고 HEVC 10-bit YUV444도 성공했다. AV1 YUV444는 미지원으로
확인되어 그 경로는 광고하면 안 되며, fallback AV1 10-bit 4:2:0은 성공했다.

관리자 helper로 stock pairing bundle을 ACL 제한된 일회성 디렉터리에 복제하고,
Foundation을 stock 기본 포트에서 기존 host identity로 실행했다. BetterParsec 통합
controller 요청 3431→3787→4229→4814 Kbps 각각에 대해 Foundation 수신, capture-thread
apply, NVENC AVC 성공을 확인했다. 별도 host API 8000/15000 Kbps 요청은 FEC 20% 제외
후 6400/12000 Kbps로 적용됐지만 이것은 통합 shaping 증거가 아니다. 종료 후 stock
service, 기본 포트, apps/config/state/cert/key 해시가 모두 원상 복구됐음을 확인했다.

비용 효율 순서:

1. [x] sender write-success/failure/skip byte, queue wait/write latency, in-flight와 selected
   ICE candidate/RTT를 기록해 payload를 wire 사용량으로 오인하지 않게 한다.
2. [x] `tools/benchmark/`에 profile/trace, 20→8→15 Mbps SSH `tc netem`, pktmon PCAPNG,
   OS counters, browser JSON, Foundation host log를 한 run manifest로 묶는 runner와
   randomized ABBA plan generator를 구현하고 dry-run으로 검증한다.
3. [ ] 별도 host/client/router에서 첫 실제 run을 수행해 packet capture, OS byte,
   browser schema v2, host apply log의 시간축을 검증한다.
4. [ ] BetterParsec/Parsec Native를 cell당 5회 randomized ABBA로 실행하고 hard gate를
   통과한 뒤에만 HEVC 4:4:4, AV1 4:2:0, HDR10 확장을 제품 우위로 주장한다.
