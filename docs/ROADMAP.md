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
   전환, P2=모양 채널 zero-latency 커서). **네이티브 클라 소형 레버 완료**
   (2026-07-15): 스트림 자식 HWND `WM_SETCURSOR`→`SetCursor(NULL)`로
   로컬 커서 숨김 — Sunshine이 visible 커서를 영상에 구워주므로
   데스크톱 이중커서 해소(P1의 "video 영역 cursor:none" 등가물,
   app-native 23 tests). 웹 P1(`cursor` 채널 + auto lock/unlock)은
   구현 착수 필요. 추가로 **immersive 모드**(전체화면 + pointer lock +
   Keyboard Lock Win/Alt-Tab 캡처 일괄 토글) 요구 — cursor P1과 같은
   클라 상태머신에 함께 배선.
3. **UDP-only 접속 — TCP fallback 미동작** — 현재 UDP가 막힌 망에서는
   접속 자체가 안 되고 WARP(1.1.1.1)로만 우회 가능. 목표는 UDP 기본 +
   실패 시 자동 fallback = **M1 그 자체**. 소스 확인(2026-07-14):
   WebSocket 전송 자체는 구현돼 있음(`SetTransport(WebSocket)`,
   `permissions.allow_transport_websockets` 게이트, stream/index.ts 전송
   폴백 체인) — WebRTC 실패 → WS 자동 강등이 실동작하는지·권한 기본값이
   막는지부터 진단. TURN-over-TCP(443) coturn 배포는 별도 잔여.
   **현장 갱신(2026-07-15, owner)**: TCP(WS) 접속 자체는 성립 — 단
   **폴백 첫 시도 실패, 리프레시 후 성공** 재현 보고(서버 로그 미포착:
   재시도 시 리프레시 없이 30초 대기 + F12 콘솔 캡처 필요). **비관
   교정**: "TCP-릴레이 실패 시 전송 재설계" 톤은 과했다 — DCV는
   WebSocket/TCP를 프로덕션 경로로 출하 중. 폴백 안정화가 남은 일이지
   전송 재설계 사유 아님. **라이브 재확인(2026-07-15 저녁, 인천 WAN)**:
   TCP 폴백 "느리지만 됨" — 실사용성 성립. 같은 세션 계측 관찰:
   host processing latency 보통 ≤3ms(우수), streamer→browser RTT는
   출렁이며 피크 60ms대(인천↔호스트 WAN — bufferbloat/무선 구간 의심,
   Gate B 상관 계측 대상. CC가 이 출렁임을 먹고 사는 신호다).
   **현장 이슈 2건 추가(2026-07-15 밤, 유튜브 시청 세션)**: ⑴ **오디오
   지연 체감** — 원인 미확정(NetEQ 지터버퍼 성장 가설: WAN 지터에서
   버퍼가 크고 느리게만 수축). 대응 = 오디오 리시버 지터버퍼
   지연/타깃을 stats 오버레이에 노출(`audioJitterBufferDelayMs` 등,
   receiver-scoped getStats 별도 조회) — 다음 세션에서 지연 체감 시
   오버레이 수치로 판정. ⑵ **immersive 중 스톨 2회 + 복구 실패, F5
   필요** — 원인: 워치독 사다리의 reconnect가 **원샷**이라 새 세션이
   videoReady에 못 닿으면 영구 정지("폴백 첫 시도 실패"와 동일 버그
   패밀리). 수정 = 재시작 후 15s 내 videoReady 미도달 시 자동
   재시도(최대 5회, videoReady 시 카운터 리셋, 소진 시 fatal 안내) —
   폴백 경로에도 동일 적용.

### 경쟁사 갭 감사 (2026-07-15 — "불가/저기대/스터글" 항목 vs Parsec·DCV·GFN 실물)

| 항목 | 경쟁사 실물 | 판정·조치 |
|---|---|---|
| TCP 전송 실사용성 (M1 "실패 시 재설계") | DCV: WebSocket/TCP 프로덕션 | 비관 교정(위 #3), 폴백 버그만 수리 |
| 커서 모양 채널 (P2 "나중") | Parsec 출하 (zero-latency 클라 렌더) | P1 라이브 판정 후 즉시 착수. 선행 결정 1건: `display_cursor` 끄기 방식(P2a 키 주입 vs P2b 포크 config) — 클라 렌더 + 구움 커서 중복 방지 |
| 클립보드 동기화 (research 05 메모 후 방치) | Parsec·DCV 출하, GFN paste 지원 | **v1 완료 + 라이브 PASS** (2026-07-15, 텍스트 전용; owner 인천 WAN 검증 "성공"): `CLIPBOARD=28` 채널, 호스트 500ms 시퀀스 폴링 워처(CF_UNICODETEXT, spawn_blocking, 256 KiB 캡), 양방향 루프가드(`should_publish`, 12 tests), WebRTC 전용 채널 + WebSocket 프리픽스 프레임 둘 다 배선, 웹 focus-poll(readText/writeText, 권한 거부 시 세션 내 outbound 비활성) — common 81 + streamer 196 tests, 웹 clipboard_wire 16 tests, clippy/tsc 클린 |
| 게임패드 럼블 | Parsec 출하 | **갭 아님** — 이미 풀배선 확인(capability 광고 → 호스트 이벤트 → vibrationActuator, input.ts) |
| 마이크 패스스루 | DCV 출하 | 백로그 등재 — vendor에 Foundation mic 훅 잔존(`send_microphone_opus_data` 미사용 경고들), 중형 |
| 펜/스타일러스 | DCV 출하 | 백로그 — Moonlight 펜 이벤트 존재, 웹 PointerEvent 매핑 미구현, 중형 |
| **HDR10 end-to-end** | Parsec·Moonlight·GFN 전부 출하 | **미추적 갭(2026-07-16 감사)** — 현 로드맵엔 "게이트 통과 후 주장"으로만 존재, 빌드 트랙 부재. 호스트 HDR 인코드(NVENC 10-bit, Foundation은 HEVC 10-bit YUV444 encode 확인됨) + PQ/HLG 메타데이터 전달 + 브라우저/네이티브 HDR 디코드·프레젠트. **화질 최강 주장의 필수 조건.** 대형(호스트+와이어+클라). Gate E 계열 |
| **멀티모니터** | Parsec·DCV·Moonlight 전부 출하 | **미추적 갭(2026-07-16 감사)** — 로드맵 전체 언급 0. 모니터 선택(단일 전환) + 스팬/개별 스트림. Sunshine은 output 선택 config 존재 → 클라 모니터 피커 + 와이어가 헤드리스 착수 가능, 호스트 다중 캡처는 포크/config. 데스크톱 실사용의 핵심. 중~대형 |
| **프라이버시 모드** | Parsec·DCV 출하(호스트 화면 블랭크 + 로컬 입력 차단) | **미추적 갭(2026-07-16 감사)** — **owner 시나리오 직결**(집/사무실에 다른 사람 있을 때 물리 모니터·키보드 차단). 호스트측(Sunshine/포크): 스트림 중 물리 디스플레이 블랭크 + 로컬 HID 차단 토글. 중형, 호스트 포크 |
| **서라운드 오디오 5.1/7.1** | Moonlight·Sunshine 출하 | [~] **채널맵 기계 완성** (2026-07-16 새벽, 헤드리스 — task 병렬): `app-native/src/audio.rs` — `frame_to_f32`가 실 `ch_layout.nb_channels` 인터리브(하드코딩 2 제거), `convert_into(+src_channels)` + 신규 `map_channels`(passthrough / mono→N / stereo→N / **5.1→stereo ITU-R BS.775 다운믹스** L'=L+0.707C+0.707Ls·클램프 / N→M 폴백), 리샘플은 채널당 일반화. 잔여: [ ] **디코더 N채널 개방**(현 stereo 하드코딩, SDP 협상 채널수 필요 — TODO 표시) + Sunshine 서라운드 config + WASAPI 멀티채널 렌더 라이브. convert 테스트 확장(5.1→stereo 손계산 검증). 채널맵은 이제 정확, 잠재 non-stereo 오처리 버그도 수리 |
| **이미지/파일 클립보드 + 파일 전송** | Parsec·DCV 이미지 클립보드, Parsec 파일 드래그드롭 | **미추적 갭(2026-07-16 감사)** — 현 클립보드 텍스트 전용. 이미지 = CLIPBOARD 채널에 PNG kind 추가(호스트 CF_DIB→PNG 워처 + 웹 Clipboard API image), 헤드리스 와이어/코덱 검증 가능. 파일 전송은 별도 reliable 채널. 중형 |

### ultra 성능 코어 트랙 (must-do)

| 트랙 | 순수 로직/설계 | 배선/구현 | 활성·판정 게이트 |
|---|---|---|---|
| U1. 프레임딜레이 CC (Pudica류) | [x] cc.rs 26 tests | [x] 송신루프 배선 + `min(abr, cc)` 합성 + 세대 가드/손실 모멘텀 수정 (2026-07-14, docs/design/cc-wiring.md, +12 tests) | Gate B 벤치 셀에서 송신측 신호 대역폭 판정 → 부족 시 TWCC/수신측 피드(Gate D) |
| U2. Tetrys 슬라이딩윈도우 FEC | [x] fec.rs 47 tests (GF(256), MDS sweep) + u32 wrap/윈도캡 가드 3핀 | [x] 프레이밍 설계(video_fec DataChannel) + **P1 배선 완료** + **P2-lite 라이브 손실 검증 통과** (2026-07-14, 루프백+clumsy 5%/20%: FEC-only 렌더 라이브, 20%에서도 화면 깨짐 0, needs-IDR 복구 루프 39+36회 순환, 손실 중 재접속 ~1.1 s — fec-framing.md §8-A) + [x] **P2 계측 기반 완성** (2026-07-15, task 병렬): Recovered/LossSpan 카운터 양측 미러 — `FecDecoderStats`/`VideoReceiverStats`(source/repair 수신, symbols_recovered, frames_recovered(프레임이 복구 심볼 사용), loss_spans/loss_spans_recovered) + `Recovered{via_fec}` 이벤트 플래그, loss-span은 단일 frontier 지연 발견 근사(인라인 문서화, 기존 동작 비트-동일) + [x] **P2 정량화 rig 완성** (2026-07-16 새벽, 헤드리스 검증): `transport-core/src/bin/fec_rig.rs` (`cargo run --release -p transport-core --bin fec-rig`) — 결정적 손실 채널(SplitMix64, Bernoulli + Gilbert 버스트) × 비율 sweep(0/10/20/33/50%) × 손실(5/10/20/30%), 48심볼 윈도우 400 trial/셀, 디코더 자체 카운터로 복구율·잔여손실·오버헤드 집계, 8 unit tests. **실측 발견**: 현 고정 20% 비율은 5% 독립 손실에서 dropped source의 90.0%만 복구, 5% 클리어에 50% 비율 필요; 버스트 채널은 훨씬 가혹(5% 버스트/20% = 34.8%) + [x] **P3 적응 비율 컨트롤러 완성** (2026-07-16 새벽, 헤드리스 검증): `transport-core/src/fec_ratio.rs` `FecRatioController` — 손실 추정 → 목표 비율(headroom + slope·loss) AIMD, 급락 즉시 상승·클린 스트릭 후 완만 감쇠·[min,max] 클램프·히스테리시스, `(num,den)` → `FecEncoder::set_redundancy`, 10 unit tests(단조성·즉시 상승·감쇠 스트릭·min 바닥·블립 리셋·config sanitise). 상수는 Gate B/C 튜닝, 기본 경로 활성화는 라이브 A/B 게이트. transport-core 111 lib + 8 rig + 웹 fec 49 tests | Gate B/D — ~~P2 rig~~ [x] · ~~P3 컨트롤러 기계~~ [x] · P3 손실 피드백 배선(디코더 stats→인코더) + A/B·기본 경로 활성 판정 |
| U3. 서브프레임 슬라이스 | [x] 제약 인벤토리 + [x] 콜백 granularity 조사(1콜백=1프레임 확정, slice-qu-constraints.md §5) + [x] **와이어 계약 핀** (2026-07-15, §6): Sunshine은 FEC 블록을 크기 균등 분할(mid-NAL)이라 "블록=슬라이스" 전제는 GFE 전용 — per-slice DU는 포크 선행(슬라이스 정렬 블록 + `multiFecFlags 0x20` 시그널, 2비트 한계로 슬라이스 ≤4)의 양측 계약으로 재분류 | [x] **소형 레버**: `slices_per_frame` C-경로 래퍼 갭 수정 — capability bits 24-31 패킹, 기본 1 비트-동일 핀(2026-07-14, streamer 261 tests) + [x] **활성값 튜닝 봉인 해제** (2026-07-16 새벽, P0 실측 완료 — 아래 P0 항): RTX 4070에서 슬라이스 2/4는 h264_nvenc 인코드 지연 비용 없음(±6% 노이즈) → slices_per_frame 상향은 겹침 이득 목적으로 자유 + [x] **포크: 슬라이스 정렬 FEC 블록+0x20 완료** (2026-07-16 새벽, foundation-sunshine `52777ec0` = betterparsec-baseline 브랜치, 리포 패치 docs/host-patches/foundation-sunshine-slice-aligned-fec.patch): `slice_aligned_fec` config(기본 off, 스톡과 와이어 비트-동일 폴백) — VCL-NAL 컷 스캐너(H.264 1-5/HEVC <32, 4바이트 스타트코드는 선행 0을 이전 그룹 trailing zero로), 그룹별 독립 패킷화(frame_header는 그룹 0만, lastPayloadLen 그룹-로컬 재계산, 그룹당 shard 예산 검증 실패 시 폴백), multiFecFlags 0x30. MSYS2 빌드·스테이지 `2026.0715.232805.52777ec0` SHA `ca274994…90806a`. 잔여(§6.3): [ ] depacketizer 패치(0x20 게이트 — **트레일링 제로 DU 관용 필수**, 포크 트래픽 루프백 검증) · [ ] 스트리머 per-slice 송신 | Gate C에서 인코드→프레젠트 겹침 이득 실측 |
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
2. ~~f1-ack 클라 플러밍~~ — **완료** (2026-07-14, f1-ack.md 구현 순서
   1–4 전부 [x]): moonlight-common-c 0x5509 수신 훅 +
   `LiRegisterBitrateAckListener` + Rust 트램폴린 + 스트리머 0x80
   arming + **라이브 왕복 ×5 검증**(커밋 4e32ba5). 잔여 5–6단계
   (`ack_latency_ms` 벤치 계측, Tier B 판정)는 라이브 벤치 런 의존.
3. ~~U4 P1~~ — **완료** (2026-07-14, U4 행 참조): `video_qu` 릴레이 +
   클라 오버레이, streamer 251·웹 103 그린. 잔여 = 호스트 무손실
   타일 경로(포크 P2).
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
   무손상, clippy 클린. **슬라이스 3 기계 완성** (2026-07-14, 헤드리스
   검증): raw D3D11 FLIP_DISCARD 프레젠트(D5) — 스트림 전용 자식
   HWND(D9, hit-test transparent) + `IDXGISwapChain2`
   `FLIP_DISCARD|FRAME_LATENCY_WAITABLE_OBJECT`, `SetMaximumFrameLatency(1)`
   **스왑체인에**(moonlight-qt d3d11va.cpp:550 함정 회피), 버퍼=비디오
   해상도 + `DXGI_SCALING_STRETCH`, HWND 자체를 aspect-fit(레터박스),
   디바이스 lost 시 1회 재생성 → egui 텍스처 폴백 래치(비 Windows 포함).
   검증 = 실 하드웨어 스왑체인 draw→백버퍼 readback 픽셀-동일 +
   중간 해상도 변경(ResizeBuffers) + Present 성공(4 tests 그린),
   non-video 빌드 무손상, clippy(--tests 포함) 클린. **슬라이스 4 기계
   완성** (2026-07-14, 헤드리스 검증): 오디오(D7 Phase A) — 지금까지
   discard-read되던 opus RTP 트랙(RFC 7587)을 client-transport가
   `RxCore` 샘플 큐(drop-oldest 64)로 라우팅(+C ABI
   `ct_receiver_wait_audio` 미러·헤더 동기), app-native `a0-audio`
   스레드가 avcodec 내장 opus 디코드(신규 네이티브 조달 0) → 믹스포맷
   변환(채널맵·선형 리샘플) → WASAPI **shared** 폴링 필(200 ms 버퍼,
   실패 시 오디오-off 래치·세션 무영향). 검증 = client-transport 22
   tests(큐 FIFO/overflow/close·ABI 복사/절단/타임아웃) + app-native 8
   tests(libopus 인코드→디코드 라운드트립 에너지, 변환 항등/다운믹스/
   리샘플, 실 엔드포인트 100 ms 렌더), clippy 클린. **A2 입력 기계
   완성** (2026-07-14, 헤드리스 검증): 호스트가 만드는 입력
   채널(mouse_*/keyboard/touch/controllers)을 session이 라벨→
   `TransportChannelId` 매핑으로 등록하고 `Session::input_sender()`
   (encode+try_send, 오버플로 드롭)로 노출. 스트림 자식 HWND가 입력
   활성 시 hit-test 투명을 해제하고 wndproc이 Win32 메시지를
   `common::input_wire::InboundPacket`으로 변환(VK 코드 패스스루,
   절대 마우스 스트림 좌표 스케일+클램프, 드래그 SetCapture, 휠
   부호 유지, Alt/F10 시스템 메뉴 억제). 검증 = client-transport 24
   tests(라벨 매핑·와이어 바이트-정확 인코드·드롭) + app-native 15
   tests(translate 순수 함수: 스케일/클램프/버튼/휠/VK), clippy 클린.
   **A1 슬라이스 1 완료** (2026-07-14, 헤드리스 검증): web-server
   bin→lib 분리 — `web_server::build(config) -> BoundServer`(바인드된
   주소 + stop 핸들, 포트 0 해석) / `start()`, 바이너리는 CLI+로깅
   래퍼로 축소, actix 서비스·클라 프로토콜 무변경. 검증 =
   `embedded_server_boots_serves_and_stops`(에페메랄 포트 부팅→raw
   HTTP 응답→graceful stop) 포함 lib 20 + bin 22 tests,
   clippy --all-targets 클린. **A1 슬라이스 2 완료** (2026-07-14,
   헤드리스 검증): app-native **host 롤** — `web_server::spawn_embedded`
   (전용 actix 스레드, graceful stop+join, 바인드 충돌은 에러로 표면화,
   rustls provider 설치 레이스-세이프化) + `host.rs`(표준 서버와 동일한
   `./server/config.json` human-json 로드, 파싱 오류는 기본값 silent
   override 대신 표면화) + egui host 스트립(Start/Stop, 바인드 주소·
   config 출처 표시, 클라 세션과 동시 가동=both 토폴로지). streamer는
   임베디드 서버가 exe 옆에서 해석·스폰(기존 경로 규약 그대로,
   betterparsec.exe와 co-locate). 검증 = web-server 44 tests(스레드
   임베더 sync HTTP 왕복·바인드 충돌) + app-native 3/18 tests(host 롤
   부팅→HTTP→stop, config 폴백/오류), clippy 클린. **A1 슬라이스 3
   완료** (2026-07-14, 헤드리스 검증): Foundation Sunshine 관리형
   서브프로세스 — Start-FoundationPaired.ps1의 라이브 검증된 기동
   계약(스테이지 exe + per-run live config에 identity 복사 +
   conf/port/pkey/cert/state/apps/log 오버라이드 9-인자)을 Rust로
   이식(`app-native/src/sunshine.rs`). `BP_SUNSHINE_STAGE`(+선택
   `BP_SUNSHINE_IDENTITY`)로 활성, 포트는 서버 config의
   `moonlight.default_http_port`와 자동 일치, 포트 선점/exe 부재
   조기 실패, 기동 중 사망은 stderr tail과 함께 표면화 → host는
   web/pairing-only로 강등(실패해도 서버 유지), UI에 pid/생존/사망
   상태 표시, Stop host = Sunshine→서버 순 종료. 스톡
   SunshineService 스왑·해시 검증·워치독은 의도적으로 벤치 스크립트
   잔류(테스트 전용 보호 기계). 검증 = app-native 7/22 tests(인자
   계약 정확 일치, identity 복사, 스텁 exe 사망 경로, 포트/exe 조기
   실패), clippy 클린. **A0+A1 헤드리스 기계 전부 완성.** **라이브
   스모크 ① 부분 통과** (2026-07-15, owner): 픽셀 OK(d3d11va HW
   디코드 확인) · 조작 OK · **이중커서 확인** → 네이티브 로컬 커서
   숨김(WM_SETCURSOR) 즉시 구현·배포(현장 이슈 #2 항 참조) · 오디오
   **렌더 실재 확인**(2026-07-15, owner: 볼륨 믹서 betterparsec 앱
   미터 동작 목격) — 최종 청취 판정용으로 `BP_AUDIO_DEBUG_DELAY_MS`
   옵트인 지연(무음 프리롤, 2s 클램프, +1 test) 추가: same-PC에서
   스트림 사본이 에코로 들리므로 귀로 즉시 판정. **빌드 함정 기록**:
   `cargo build -p app-native`는 기본 피처가 비어 있어 `--features
   video` 누락 시 디코더 없는 exe가 나온다(세션·입력·오디오 채널은
   정상 연결, 화면만 부재 — 2026-07-15 오전 재현·소요). 정식 커맨드 =
   `cargo build --release -p app-native --features video`. **라이브 ②차
   (2026-07-15 오후, Parsec 원격 경유 — 판정 오염 주의) 현장 이슈 3건
   진단·수정 (헤드리스 검증)**: ⑴ 재접속 화이트스크린 = `wait_for_key`
   스킵 경로가 IDR을 요청하지 않아 mid-GOP 조인(Sunshine resume) 시
   무한 대기 — 스킵 시 `request_idr` 래치로 수정(+로그 근거: 워치독
   무발화 = 프레임은 도착, "surface up" 부재 = 퍼블리시 없음).
   ⑵ 오디오 = AUDCLNT_E_DEVICE_INVALIDATED(Parsec 가상 장치의 기본
   엔드포인트 전환)에서 죽은 sink에 fill 스팸 + 복구 없음 → sink 폐기 +
   2s 주기 재생성 + 딜레이 프리롤 재장전으로 수정(딜레이 자체는 매 실행
   armed 확인 — 미청취는 장치 사망 때문). ⑶ 커서 = 클릭 후 단일(owner
   확인) — 원인: 부모(winit)가 포커스 보유 중 매 프레임 커서를 재적용해
   child의 WM_SETCURSOR 숨김과 경합 → 포인터가 스트림 child 위면 egui에
   `CursorIcon::None` 보고(`StreamSurface::cursor_over`)로 부모 권위도
   차단. + `surface_failed` 래치를 접속 단위로 리셋(Connect·워치독
   재접속), 래치 시 warn 로그. **라이브 ③차 (2026-07-15, owner)**:
   ⑴⑵ 검증 통과 — **오디오 청취 PASS**(500ms 에코 확인; "무한증식"
   관찰 = same-PC 루프백 피드백: 스트림 사본 재생→Sunshine 재캡처→
   재인코드 캐스케이드, 딜레이가 가시화한 예상 물리·딜레이 off면 소멸)
   + **재접속 반복 정상 = 화이트스크린 픽스 확정**. ⑶ 호버 이중커서 =
   **원격 판정 불가로 이월** (owner 확인: 세션 전체를 Parsec 경유로
   관측 — 보이는 두 번째 커서는 뷰어 쪽 Parsec 포인터라 호스트 코드
   관할 밖; 클릭 후 단일은 재확인). 대비책 선탑재: WM_MOUSEMOVE마다
   SetCursor(NULL) 재강제 + WM_SETCURSOR 경로 레이트리밋 debug 로그
   (`betterparsec::input=debug`) — 물리 모니터 세션(인천, 네이티브 배포
   또는 웹 개선 후)에서 5초 판정. 잔여 라이브 = ①호버 단일커서(물리
   모니터), ②host 롤 — 임베디드 서버로 paired 접속
   (`BP_SUNSHINE_STAGE` 관리형 Sunshine — **스테이지 재구축 완료**
   2026-07-15 밤: 21:19 중단 빌드(세션 사망 동반) ninja 재개,
   `-DDRIVER_DEPS_REQUIRED=OFF`(설치러 페이로드, 스테이지 계약 무관),
   정적 링크 확인(ldd 전부 시스템 DLL), exe+assets 스테이지 =
   `C:\Users\kje12\Projects\foundation-sunshine\stage`, `--version` 부팅
   스모크 `2026.0715.221257.c12c2a91.杂鱼`, SHA-256
   `a3318c2d…86223a4` — 남은 건 라이브 접속뿐) →
   다음 헤드리스 대형 항목: M4 `session-ux`(스톨 워치독 웹 배선 완료
   2026-07-15 — 잔여: 커서 P1, immersive) · U3는 와이어 계약 핀 완료
   (slice-qu-constraints.md §6) — depacketizer 패치는 포크(슬라이스 정렬
   FEC 블록) 선행으로 재순서, 클라 단독 착수 불가 판정(2026-07-15).
   이후는 전부 라이브 게이트(Gate B/C).
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
- [x] RTX 4070 ULL/슬라이스 인코드 지연 자체 실측 — **완료** (2026-07-16 새벽,
  `nvenc-slice-probe` bin: `cargo run --release -p app-native --features video
  --bin nvenc-slice-probe`, 1080p NV12 CBR 10Mbps tune=ull, 30 warmup + 300
  measured, 비트스트림 VCL NAL 카운트로 슬라이스 적용 검증 — 전 config
  요청=관측): p50 slices=1→2→4: p1 1634→1540→1586us, p4 2142→2129→2166us,
  델타 -6%~+1% = **노이즈 수준. 문헌의 "슬라이스 = 수 ms 비용" 주장 기각**
  (리서치 04 §3-1 상충 해소) — U3 slices_per_frame 튜닝은 인코드 지연
  세금 없이 겹침 이득만 판정하면 된다(Gate C). 주의: h264_nvenc private
  `slices` AVOption 부재 — `AVCodecContext.slices` 필드가 유효 레버.

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

## M3 — 코덱 / 화질 / 효율 (feature #3)
**목표:** 압도적 네이티브급 화질 + 타 대비 저비트레이트 고효율. 전체 감사:
`docs/research/quality-efficiency-audit.md` (2026-07-16).

**감사 정정 — capability는 대부분 이미 있고, "꺼진 채"다:**
- **웹 클라 디코드 완비** [x]: H264/HEVC/AV1 × 4:4:4 × 10-bit 전부 WebCodecs
  감지 지원(video_decoder_pipe.ts). M3 미완의 상당수는 **클라가 아니라
  호스트 encode 검증 + 기본값 전환.**
- **인코더 무료 점심 세트(지연 비용 ~0, 꺼져 있음)**: spatial
  `adaptive_quantization`(false) · `quality_preset` P1(→P4/P5, +500µs@1080p
  affordable) · `weighted_prediction`(false). Foundation `nvenc_config.h` 실측.

### 효율 지렛대 (측정→활성)
- [x] **화질-효율 probe 완성** (2026-07-16 새벽, 헤드리스 검증 — task 병렬):
  `app-native/src/bin/quality_probe.rs` (`cargo run --release -p app-native
  --features video --bin quality-probe`) — 스트레스 합성 프레임(이동 그라디언트
  + Nyquist 체커보드 + PRNG 노이즈) 120장을 h264_nvenc preset{p1,p4,p6} ×
  spatial_aq{off,on} 고정 8Mbps CBR로 인코드 → 소프트웨어 디코드 → 원본 대비
  PSNR/SSIM(순수, 6 tests). **실측**: SSIM p1→p4→p6 = 0.8987→0.9137→0.9192,
  PSNR 27.34→27.87→28.18 — **preset가 화질/비트 대부분 캐리**, p4+aq vs p1 =
  같은 8Mbps에서 **+0.60dB PSNR / +0.0166 SSIM**(무료 점심 실측 이득).
  spatial_aq는 고주파 노이즈 콘텐츠에선 미미(평탄/텍스트에서 빛남 — 실 데스크톱
  콘텐츠 라이브 재측 필요). `spatial_aq` AVOption명 확인됨. codec/chroma 축은
  라이브(호스트 인코드) 확장.
- [ ] **[라이브/호스트] 무료 점심 활성**: spatial AQ ON + preset P4 +
  weighted-pred ON (probe로 sweet spot → Gate B VMAF 검증)
- [ ] **[라이브/호스트] 코덱 기본 전환**: HEVC e2e 검증 → 기본 h264→hevc,
  AV1 e2e(AV1 4:2:0 probe 통과) → av1. 같은 화질 −30~50% 비트
- [ ] **[라이브/호스트] 4:4:4** 협상·검증(색 텍스트 fringing 제거, 클라 준비 완료)
- [ ] **[라이브/호스트] 10-bit SDR** — 밴딩 제거(그라디언트·어두운 씬 네이티브급),
  클라 MAIN10/REXT10 준비 완료
- [ ] `nvenc_vbv_increase`/VBR 등 rate-control 튜닝(모션 스파이크) — CC 상호작용, Gate C

### 심층 발견·수리 (2026-07-16 파이프라인 추적)
- [x] **네이티브 색 정확도 버그 수리** (헤드리스 검증) — `app-native/src/video.rs`
  `frame_to_rgba`가 swscale 색공간/레인지 미지정 → BT.601 기본으로 HD(709)
  스트림 색 틀어짐(피부톤·채도 이동), present까지 도달(웹은 브라우저 VUI로
  정상 = 네이티브 전용). 프레임 `colorspace`/`color_range` → `sws_setColorspaceDetails`
  (709/601/2020 + full/limited, 미지정 시 ≥720p→709 폴백). 순수 `sws_cs_for`/
  `is_full_range` +2 tests, app-native 52 tests. quality-efficiency-audit.md §F.
- [ ] **8-bit present 천장** — R8G8B8A8 스왑체인, 10-bit 디코드 절단. R10G10B10A2 필요
- [~] **present CPU 왕복 제거(제로카피 NV12 텍스처)** — [x] **opt-in 경로 완성** (2026-07-16, 01b4968 — 아래 M6 실행 버스트 ⑷): 'Fast GPU present (NV12)' 인앱 체크박스, swscale GPU→CPU→GPU 왕복 제거·8MB→3MB 업로드, 기본 off·무회귀. 잔여: [ ] 라이브 A/B + 기본 전환 판정

### 신규 capability (진짜 미구현)
- [~] **클라 샤픈/CAS 셰이더** — [x] **기계 완성** (2026-07-16 새벽, 헤드리스
  검증 — task): `app-native/src/present.rs` opt-in 샤픈 패스 — `BP_SHARPEN`(0..100)
  또는 테스트 훅으로 활성, **기본 off는 기존 `CopyResource` 경로 그대로**(bit-exact
  `swapchain_roundtrip_and_resize` 보존). 런타임 D3DCompile(vs_5_0/ps_5_0, 인라인
  HLSL): full-screen 트라이앵글 VS + CAS/unsharp PS(center+4축 이웃, `sharp =
  center + strength*(center*4 - 상하좌우)*0.25`, saturate). SRV는 upload 텍스처
  수명에 결속(리사이즈 시 재생성), FLIP_DISCARD라 RTV는 프레임마다 신규 + draw 후
  언바인드(bind hazard 회피). 실 D3D11 하드웨어 검증 — 신규 test(하드 엣지
  오버슛/언더슛 readback 확인, 평탄부 불변). app-native 53 tests. 잔여: [ ] 저해상도
  전송 + 업스케일 결합(동적 해상도 컨트롤러와) + 강도 라이브 튜닝
- [~] **대역 구동 동적 해상도 스케일링** — [x] **컨트롤러 기계 완성**
  (2026-07-16 새벽, 헤드리스 — task 병렬): `transport-core/src/resolution.rs`
  `DynResolutionController` — 대역 추정 → 해상도 사다리(2160/1440/1080/900/720/
  540/480, 룽별 min_kbps) 매핑, drop-fast/recover-slow 히스테리시스(현 룽 부족 시
  즉시 1룽↓, raise_streak 클린 후 1룽↑), 클램프, `sanitised()`(길이불일치·빈입력
  폴백), 12 tests. 상수는 Gate-B/C 튜닝. 잔여: [ ] 스트리머 배선(해상도 변경 요청)
  + 클라 업스케일(샤픈 셰이더와 결합)
- [ ] **[대형] 클라 슈퍼레졸루션(FSR/VSR급)** + 10-bit/HDR 프레젠트 경로
  (R10G10B10A2 + HDR 스왑체인) — GFN DLSS 지렛대
- [ ] **AV1 필름그레인 합성** — 그레인 제거 인코드 + 클라 합성(그레인 콘텐츠 효율)
- **검증:** 동일 delivered bitrate에서 preset·AQ·codec·chroma별 SSIM/PSNR/VMAF
  채점(probe) → Gate B/E에서 Parsec/GFN 대비 VMAF-NEG +2 또는 −20% 비트 판정.

## M4 — 하드닝 + 세션 UX (owner 요구로 선택 → 필수 승격, 2026-07-14)
- [~] **스톨 감지·복구 사다리** (현장 이슈 #1): 클라 프레임 수신 워치독 →
  IDR 요청 → ICE restart → 전체 재접속 에스컬레이션 + 가시 상태 표시.
  hard gate 복원력(freeze/min, 15분 disconnect 0)의 전제.
  **순수 상태머신 완성** (2026-07-15, `web/stream/session_ux.ts`
  `StallWatchdog` + 15 tests): 틱 구동 사다리(indicator 1s → IDR 2s/4s/6s
  → ICE restart 10s → reconnect 20s, 미튜닝 기본값), 틱당 최대 1단
  에스컬레이션(백그라운드 탭 타이머 스로틀 시 사다리 일괄 발화 방지),
  pause/resume(document-hidden), 회복 시 `recovered{stalledMs}` + 사다리
  리셋. **웹 배선 완료** (2026-07-15, 헤드리스 검증): 250ms 틱 드라이버 +
  프레임 신호(FEC/data 경로는 수신 리스너 직결, videotrack 경로는 receiver
  `framesDecoded` 폴링) + 인디케이터 DOM(`.stream-stall-indicator`, 양
  테마) + 신규 시그널링 메시지 `StreamClientMessage::RequestIdr`(전
  전송에서 needs_idr — 데이터 경로 사망에도 생존)·`RestartIce`(스트리머
  webrtc `send_offer(ice_restart=true)` 재-offer, 클라 answer 경로 기존) +
  reconnect = 기존 fresh-ws 재접속 경로 재사용(`restartSessionWithFreshWs`
  추출). 검증: tsc + 웹 70 tests, streamer 180 + common 81(신규 wire-pin
  1), clippy·fmt 클린. **Rust 미러 완성** (2026-07-15,
  `client-transport/src/watchdog.rs` — TS와 락스텝: 동일 사다리·기본값·
  틱당 1단, TS 스위트 포팅 15 tests, API만 `Option<WatchdogAction>`).
  **네이티브 배선 완성** (2026-07-15, 헤드리스 검증): 워치독을 세션
  루프에 내장(`client-transport/src/session.rs` 50ms 틱 — 셸 스왑에도
  생존, ct-probe 포함) — 프레임 신호 = `RxCore::frames_delivered`
  단조 카운터(+1 test), IDR/ICE restart = 시그널링 ws
  `RequestIdr`/`RestartIce`, reconnect = `WatchdogStatus` 플래그 +
  세션 Failed 종료 → 셸(app-native)이 자동 재생성. 셸 쪽 배선 =
  인디케이터 readback(구 임시 라벨 대체) + minimized pause. 검증:
  client-transport 40 + app-native 24 tests, clippy·fmt 클린. **주의**:
  신규 ws 메시지라 web-server/streamer도 같은 커밋 이후 빌드 필요
  (구버전 서버는 deserialize 실패로 릴레이 종료 — 배포 세트 일치 확인,
  2026-07-15 로컬 세트 재빌드·재기동 완료). 잔여 = 라이브 스톨 주입
  스모크(clumsy full-block, 웹+네이티브) + Gate B/C 임계 튜닝.
- [~] **커서 P1** — `cursor` DataChannel + 호스트 권위 자동 lock/unlock
  (cursor-channel.md §3 P1). **호스트 절반 완성** (2026-07-15, 헤드리스
  검증): `cursor` reliable·ordered 채널 + 60Hz Win32 트래커
  (`GetCursorInfo` 폴링, CURSOR_SUPPRESSED=hidden, 변화 시만 송신,
  ptScreenPos+GetSystemMetrics 단일 좌표공간 — 정규화 사상엔 일관성만
  필요) + POS 와이어(14B LE) 양측 바이트-핀(`cursor_wire.rs` ↔
  `web/stream/cursor_wire.ts`, streamer 182 + 웹 5 tests). **클라 순수
  머신 완성** (2026-07-15, `web/stream/cursor_auto.ts` `CursorAutoMode`
  + 9 tests): visible→unlock / hidden→lock, 히스테리시스 150ms(깜빡임
  내성·클록 재시작), 전이당 액션 1회, 샘플/틱 양쪽에서 커밋. `cursor`
  채널 스태시(webrtc.ts, video_qu 패턴)까지 완료. **웹 배선 + 네이티브
  플러밍 완성** (2026-07-15, 헤드리스 검증 — task 병렬 2슬라이스):
  ⓐ 웹: mouseMode "auto"(DOM-free `mouse_mode.ts`의 순수
  `resolveMouseMode` — auto+locked→relative/auto+unlocked→follow,
  StreamInput은 effective 모드로만 분기), Stream이 cursor 채널 →
  `CursorAutoMode` 구동(워치독 250ms 틱에 피기백) →
  `cursorAutoMode{locked}` InfoEvent, ViewerApp이 pointer lock
  진입/해제 + 제스처 부재 시 클릭 재장전(`autoModeWantsLock` —
  기존 relative 재장전이 mouseMode를 "relative"로 변조하던 부작용
  제거) + unlock 구간 `.stream-cursor-none`(양 테마). 비-auto 세션
  동작 불변. ⓑ 네이티브: client-transport `cursor.rs`(decode 바이트-핀
  3면 락스텝 + `CursorShared` atomic readback, x/y u64 패킹) + 세션
  cursor 채널 스태시 — immersive(Phase B) 기반. 검증: tsc + 웹 32
  tests(cursor_resolve 3 포함), client-transport 46, clippy·fmt 클린,
  streamer.exe·static/ 배포 세트 갱신. **런타임 전환 지원** (2026-07-15,
  현장 리포트 "세션 중 auto 선택 무반응" 대응): 사이드바 선택기에 auto
  추가 + `Stream.onMouseModeChanged` → `setCursorAutoEnabled`(채널
  리스너 dedup — 물리 채널당 1회, 머신은 필드 경유라 스왑 즉시 반영;
  전환이 settings에 반영돼 재접속에도 유지). **전송-불문 승격** (2026-07-15,
  owner 지적 "DCV는 TCP에서도 커서 됨" — 정당): cursor를
  `TransportChannelId::CURSOR`(=27) 1급 채널로 — WebRTC는 기존 전용
  DataChannel(라벨이 제네릭 테이블로 매핑, 스태시 특례 제거), WebSocket은
  CURSOR-프리픽스 프레임(트래커를 `CursorSink` 추상화로 이동:
  `transport/cursor_tracker.rs`, ws 프레임 바이트-핀 +1 test). 클라는
  `getChannel(CURSOR)` 단일 경로로 양 전송 소비 → **UDP 차단(TCP 폴백)
  망에서도 auto 커서 동작**. 검증: common 81 + streamer 183 + 웹 143,
  clippy·fmt·tsc 클린, streamer.exe·static/ 재배포. **라이브 판정 PASS**
  (2026-07-15 저녁, owner — 인천 원격 WAN + 물리 모니터): auto 커서
  정상 동작, FPS 360° lock 회전 확인. 현장 이슈 1건: **ESC가 pointer
  lock을 강제 해제**(브라우저 보안 스펙 — immersive의 Keyboard Lock만
  예외)해서 게임 중 거슬림 → 완화 배선: 클릭뿐 아니라 **아무 키
  입력에서도 lock 재획득**(keydown = transient activation, ESC 자체는
  제외 — WASD 누르는 순간 즉시 복귀). 게이밍 정답 경로는 immersive.
  → [~] **P2 모양 채널 기계 완성** (2026-07-15, 헤드리스 — task 슬라이스):
  POS v2(+u32 shape_id, 네이티브 관용성 핀 +1 test) + SHAPE 와이어(kind=1,
  PNG 256 KiB 캡, 양측 바이트-핀) + 스트리머 모양 추출(hCursor 변화 감지,
  GetIconInfoExW+GetDIBits — 레거시 제로알파는 AND 마스크로 복원, 모노크롬
  XOR은 PNG 알파 근사, oversize는 영구-미스, 실물 IDC_ARROW 추출
  라운드트립 test) + 웹 클라 렌더(shape 캐시 32개, chunked base64,
  ≤128px CSS `cursor:url() hotspot` — inline style이 cursor:none 클래스
  위에 자연 우선) — **`clientCursor` 설정 기본 off**: display_cursor가
  영상에 커서를 굽는 동안 이중 방지. **P2a/P2b 선행 결정 해소**
  (2026-07-15 밤, 포크 소스 확인): Foundation upstream에 이미
  `capture_cursor` config(+런타임 토글 Ctrl+Alt+Shift+N, config.cpp:1341
  — `display_cursor` 전역 직결)가 존재 — **포크 패치 불필요**. 활성화 =
  호스트 conf `capture_cursor=false` + `clientCursor` 기본 on + 라이브
  판정. **네이티브 shape 렌더 완성** (2026-07-16 새벽, 헤드리스 검증):
  client-transport POS v2 `shape_id` 소비 + SHAPE 디코드(3면 바이트-핀
  락스텝, 웹 parse 시맨틱 동일 — cap/절단/트레일링) + `CursorShared`
  최신-1 shape 슬롯(모양은 변화당 새 id 전체 재전송이라 단일 슬롯이
  완전 상태) → app-native `cursor_icon.rs`: PNG→BGRA(순수) →
  32bpp 알파 DIB+제로 AND 마스크 `CreateIconIndirect`(실 USER32
  라운드트립 test) → `ClientCursor` 펌프(id당 1회 빌드, 실패는 영구
  미스, HCURSOR 링 4로 사용-중 파괴 방지, 접속 단위 리셋) →
  WM_SETCURSOR/WM_MOUSEMOVE가 `SetCursor(NULL)` 대신 active 핸들
  적용. **`BP_CLIENT_CURSOR=1` opt-in**(구움 커서와 이중 방지 — 웹
  `clientCursor` 기본 off와 동일 자세). 이제 기본 전환 잔여 = 호스트
  `capture_cursor=false` 세션에서 웹+네이티브 라이브 판정만.
  client-transport 54 + app-native 33 tests.
- [~] **immersive 모드** — 전체화면 + pointer lock + Keyboard Lock 일괄
  토글. **웹 완성** (2026-07-15, 헤드리스 검증): 사이드바 Immersive
  버튼 — 진입 = fullscreen 확인 후 keyboard.lock(가드) +
  `wantsPointerLock`(relative 또는 auto+wantsLock, DOM-free 헬퍼 +3
  tests) 시 pointer lock; 이탈 = 버튼/fullscreen 상실/lock 상실 3경로가
  동일 teardown으로 수렴(키보드 unlock 누수 없음). **라이브 판정 PASS**
  (2026-07-15, owner: "굉장히 잘 작동"). **네이티브 Phase B1 기계 완성**
  (2026-07-16 새벽, 헤드리스 검증): `immersive.rs` 순수 틱 머신(웹
  시맨틱 미러 — 진입은 fullscreen+focus 확인 후에만 Engage, 이탈은
  버튼/fullscreen 상실/포커스 상실 3경로가 단일 Release로 수렴,
  reset은 상태별 owed 액션 반환, 8 tests) + RawInput 상대 마우스
  (`RegisterRawInputDevices` 자식 HWND 타깃, WM_INPUT→`raw_mouse_delta`
  순수 변환: absolute 플래그/제로 모션 드롭·i16 클램프, 델타 무스케일
  전송 = moonlight-native 시맨틱, MOUSE_RELATIVE 채널) + 캡처 중 절대
  WM_MOUSEMOVE 억제·커서 강제 숨김 + `ClipCursor` 매 프레임
  재단언(이동/리사이즈 이벤트 플러밍 불요) + egui viewport fullscreen
  토글 + 전역 해제 안전망(surface Drop·세션 리셋 3경로, 클립/등록
  잔류 없음). **Phase B2 완성** (2026-07-16 새벽, task 병렬, 헤드리스
  검증): WH_KEYBOARD_LL 훅 — 캡처 중 Win 키·Alt+Tab을 삼켜서 와이어
  Key 패킷으로 전달(SwallowForward), **Ctrl+Alt+Shift+Q = immersive
  탈출 해치**(커서가 스트림 child에 클립돼 사이드바 버튼 도달 불가 —
  키다운 엣지에서만 발화, `CaptureShared.take_exit_requested()`로 셸
  토글에 합류). 순수 `hook_decision` 테이블 5 tests, 훅 설치/해제는
  engage/`release_mouse_capture_global` 단일 수렴점에 배선(기존 해제
  3경로 전부 무누수), LL 훅 컨텍스트 한계는 OnceLock 슬롯 + VK_Q
  콤보는 GetAsyncKeyState(LL 훅에서 GetKeyState 1이벤트 지연 회피).
  app-native 49 tests. **host-authority auto 전환 완성** (2026-07-16 새벽,
  task 병렬, 헤드리스 — owner "Parsec/Moonlight 방식 그대로" 결정):
  immersive 캡처가 호스트 커서 권위를 미러 — 순수 `wants_relative_capture
  (engaged, host_cursor_visible) = engaged && !visible` (immersive.rs,
  +table test), 셸 틱이 매 프레임 `CursorShared::visible()`로 판정 →
  숨김(게임)=relative+clip, 보임(메뉴/데스크톱)=release+unclip(절대 follow,
  커서 표시). 전체화면·키보드 훅·RawInput 등록은 immersive 세션 내내
  유지(auto-switch는 relative 플래그+클립만 토글). input.rs 무변경(기존
  `capture.relative()` 분기가 절대/상대 전환을 이미 처리). present.rs
  `release_cursor_clip`(ClipCursor(None)) 추가. app-native 50 tests.
  잔여 = 라이브 판정(물리 모니터 게임 세션). **필드 버그 수리** (2026-07-16,
  owner 라이브 리포트 — 헤드리스 검증, cef622a): ⑴ **Alt(및 Ctrl/Shift/Win)
  스턱** — 탈출 시 스트림 child가 포커스를 잃어 물리 key-up이 와이어에 못
  닿으면 호스트가 모디파이어를 latch(해치 Ctrl+Alt+Shift+Q는 그 셋, Alt+Tab은
  Alt). `input::release_sticky_keys(sender)`가 Alt/Ctrl/Shift/Win/Tab/Q key-up
  강제 — 훅 ExitImmersive·wndproc 탈출·WM_KILLFOCUS·사이드바 Exit(main.rs
  Release) 전 경로에서 호출. ⑵ **전체화면 갇힘→앱 강제종료** — 해치가 LL 훅에만
  있었는데 `SetWindowsHookExW`는 실패 시 warn만 하고 진행 → 훅 실패면 클립된
  커서로 완전히 갇힘. 훅-독립 탈출(`is_wndproc_escape`, 순수+6 tests) 추가 —
  캡처된 child가 포커스 보유 시 Ctrl+Alt+Shift+Q가 wndproc에서도 발화(훅 정상
  시 Q를 먼저 삼켜 이중발화 없음). ⑶ **진짜 포커스 상실**(실패 훅으로 Alt+Tab
  이탈·시스템 다이얼로그·UAC) — 캡처된 child의 WM_KILLFOCUS가 sticky 키 해제 +
  immersive 탈출 요청(커서 언클립). app-native 55 tests. 잔여 관찰 = 스톨 중
  UI 스레드 자체가 얼면 인앱 탈출 불가는 별개 신뢰성 축(스톨 근원).
- [x] **웹 UI/UX 폴리시 패스** (2026-07-16 새벽, task 병렬 — owner
  "moonlight-web 느낌 너무 구리고 불편" 대응): 타이포 스케일(14px UI
  베이스, h1/h2/h3 위계, 전역 text-shadow 제거), 유휴 네온 글로우
  제거(글로우는 hover/focus-visible만), bg-0..3 실 엘리베이션 스케일,
  모든 인터랙티브 요소에 hover/focus-visible/active/disabled 상태,
  터치 타깃 ≥40px, 모달/폼 max-width(울트라와이드), 설정 메뉴 섹션
  그룹화(`.settings-section`), 사이드바 aria-expanded + 선택 상태
  하이라이트. **moonlight 테마 대수선**: standard.css 전용 토큰/셀렉터
  다수가 moonlight 테마에서 미정의(토스트·호스트 상태·로딩·검색이
  통째로 무스타일)였던 것을 자체 팔레트로 포팅 — 양 테마 기능 동등.
  클래스 리네임/삭제 0(프로그램 검증), tsc 클린, 웹 160 tests 그린.
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
- [~] **네이티브 클라 실행 버스트 + 인천 배포판 패키징** (2026-07-16 오전, 헤드리스 빌드·검증 — 7커밋 9f2912b…9def901, 워킹트리 클린): 인천 물리 모니터 필드 배포를 겨냥한 app-native 실행 묶음.
  ⑴ **한글 IME 수리**(21943ce): 스트림 자식 HWND의 IME 컨텍스트 분리(`ImmAssociateContext(NULL)`)로 클라측 조합을 막아 raw Win32 VK 키다운이 와이어로 직행 → 호스트 IME가 한글 조합(웹의 브라우저-IME 억제와 동치). `Win32_UI_Input_Ime` 피처 추가.
  ⑵ **immersive OS 리드백 웨지 수리**(8a41f52): 상태머신이 `viewport().fullscreen/focused` OS 리드백을 캡처 게이트로 삼아 필드 클라에서 리드백이 영영 true로 안 뒤집혀 Entering에 영구 갇힘(전체화면은 됐으나 캡처·자식 리사이즈 무발 = immersive가 no-op처럼). 리드백 게이트 제거 → fullscreen 요청 후 1 settle 프레임에 Engage, 이탈은 명시 토글/Ctrl+Alt+Shift+Q 해치만(LL 훅이 이미 Win/Alt-Tab 삼켜서 focus-loss 자동이탈은 중복·오탈출 원인이라 제거). 매프레임 커서 재클립이 지오메트리 보정 + immersive 전체화면 사이징(자식을 `screen_rect` 풀윈도, 비immersive는 뷰포트 aspect-fit). Alt+Tab 수리(c688881): LL 훅 modifier를 GetKeyState 대신 `LLKHF_ALTDOWN`+GetAsyncKeyState로.
  ⑶ **프레젠트 지연 바운드**(c688881): 프레임 펌프가 매 이터레이션 큐를 비우고(`try_pop`/`try_frame`) 스테일 유닛은 `Decoder::decode_drop`(receive+unref만, download/convert 없음 — P프레임 참조체인 유지)로 넘긴 뒤 **최신 유닛만** convert·present. HW 디코드가 60fps를 쉽게 버텨서 참조는 안 밀림 → convert/present가 아무리 밀려도 프레젠트 지연은 1프레임 고정(기존 16-deep FIFO 백업 ~266ms→IDR 플러시 히칭 해소). fps 리드아웃은 이제 실 present 레이트 반영.
  ⑷ **NV12 GPU 프레젠트 경로**(01b4968, opt-in): `decode_nv12`가 d3d11va NV12 평면 타이트 복사 + YUV→RGB 매트릭스(709/601/2020+range) Rust 산출 → Y(R8)+UV(R8G8) 텍스처 업로드 + 셰이더 변환(CPU swscale 없음, ~3MB). `DecodedFrame`이 RGBA/NV12 양형 운반, 'Fast GPU present (NV12)' 체크박스(기본 off, raw+HW 둘 다 활성 시만) — 검증된 RGBA 경로 기본·무회귀. 색매트릭스 그레이스케일축·크로마 부호 유닛테스트. **M3 "present CPU 왕복 제거" [~]로.**
  ⑸ **WASAPI exclusive 오디오**(13283aa, opt-in `BP_AUDIO_EXCLUSIVE=1`): 이벤트 구동 전용 스레드(thread::scope, 디바이스 이벤트 대기) 렌더, init 실패(포맷/AUDCLNT_E_*/이벤트) 시 shared 폴링으로 폴백(오디오 무사망). shared 버퍼 지연 절감 지반 — 인앱 토글 전까지 opt-in(필드 클라 env 불가). + `frame_to_rgba` 매프레임 8MB `vec![0u8;…]` memset 낭비 제거(reserve+set_len).
  ⑹ **샤픈 인앱 슬라이더**(9f2912b): `VideoShared.sharpen_pct`를 present 스레드가 라이브 리드, egui 슬라이더 — 스크립트/env 없이 원격 샤픈 A/B(M3 샤픈 "강도 라이브 튜닝" 잔여 해소, `BP_SHARPEN`은 기본 시드 유지).
  ⑺ **제로컨피그 접속폼 프리필**(9def901): 더블클릭 배포 exe가 localhost·빈 user/host/app로 열려 접속 실패 상습 원인이던 것 수리 — `ConnectForm::default`가 exe 옆 `betterparsec.conf`(key=value, package-portable.ps1 생성) 읽음, 우선순위 env>conf>기본, **비밀번호는 절대 굽지 않음**(사용자 타이핑). 다운로드 즉시 전부 프리필, 사용자는 비번만.
  **배포 산출물**(09:36, 커밋과 동시): `tools/package-portable.ps1`이 exe+FFmpeg DLL+`run-incheon.bat`+`betterparsec.conf`를 zip → `static/betterparsec-portable.zip`(65MB)로 복사, 가동 중 web-server가 `https://<server>:8080/betterparsec-portable.zip` 자가 배포. 정식 빌드 = `cargo build --release -p app-native --features video`.
  **잔여(전부 라이브 게이트, owner·인천 물리 모니터)**: ①한글 IME 입력 ②immersive 게임 세션(웨지 수리 후) ③present 지연 히칭 소멸 ④WASAPI exclusive 오디오 A/B(체감 지연) ⑤NV12 fast-present A/B ⑥host-authority 호버 단일커서 ⑦immersive Alt-스턱·탈출 수리(위 immersive 항). **인앱 exclusive 오디오 토글 완성** (2026-07-16 f5ada37 — `AudioShared::exclusive` 아톰 + App `audio_exclusive`(env `BP_AUDIO_EXCLUSIVE` 시드) + 사이드바 체크박스, `Running::start`가 스레드 스폰 전 시드): **원 로드맵의 "토글은 판정 후" 순서는 오판** — 배포 exe는 더블클릭이라 env 불가 = 토글 없이는 필드에서 exclusive 자체를 못 켜므로 토글이 판정의 **전제조건**. **exclusive FIFO 캡 40ms 축소 완료** (cef622a — 기존 shared/exclusive 공통 250ms(`rate/4*ch`)라 exclusive에서도 버스트 시 250ms 적체하던 것을 exclusive만 40ms로, run_shared 250ms 불변, +1 test). **필드 zip 재빌드·재배포 완료(11:42)**: IME·immersive 웨지·present 바운드·NV12·exclusive 오디오 토글·immersive 스턱/탈출 수리 전부 포함. **설정 통합 방향** = docs/design/config-model.md(제로컨피그 북극성 + 프리셋 + BP_* env→인앱 토글 이관 + 호스트 설정 통합).
  **non-live 백로그 실행 완료** (2026-07-16 오후 — ralplan run 019f68d6 합의(Architect/Critic) → owner 승인 → ultragoal 6골 전부 complete, 커밋 f980144…b0c6fae): ⑴ 순수 mode 엔진(transport-core/mode.rs — Fast/Medium/Quality→knobs, free-lunch 상수, 웹 미러 canonical 벡터) ⑵ 서라운드 N채널 seam(negotiated_channels+RxCore 셀+디코더 reopen+stereo 폴백, exclusive DEVICE=2 유지) ⑶ 10-bit R10G10B10A2 present(BP_PRESENT_10BIT/스토어 토글+CheckFormatSupport+셰이더 blit+R8 폴백) ⑷ streamer 순수 fn(SDP 코덱선택·rtt→IDR·relay 포트, wire-in 게이팅) ⑸ settings.json 스토어(schema:1, 미지필드 보존, corrupt→백업+재생성)+egui Stream settings 패널(모드/비트레이트/해상도/fps/10bit/커서)+FlowConfig 스토어 파생+웹 mode.ts 미러(Rust↔TS 값 정확 일치 10 tests)+BP_CLIENT_CURSOR→토글 ⑹ config-model.md 재작성. 게이트가 P1 실검출(기본 시드가 모드 셀렉터 마스킹→0-센티널 수정+가드 테스트, fa25162). **주의: 기본 비트레이트 8→20Mbps(Medium 기본)**. 검증: transport-core 129·client-transport 55·streamer 206·app-native 66·web 10, clippy 클린. **필드 zip 재배포(13:38)** — 한 세션 최대 검증 절차 = `docs/LIVE-CHECKLIST.md`(세션 1 기본/IME/히칭/sharpen/NV12/immersive 스턱·탈출 → 세션 2 exclusive 오디오 → 세션 3 10-bit → 세션 4 모드 실효 + 옵션 서라운드/클라커서/스톨 로그).
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
