# BetterParsec 통합 양방향 앱 — 아키텍처 설계

**Date**: 2026-07-14
**Owner 방향**: Sunshine+Moonlight가 통합된 **양방향 단일 앱**(호스트 겸
클라), 웹/네이티브 공통 **UI·알고리즘 전면 자체화**. moonlight-qt 포크는
참조 구현·검증 비히클이지 제품 셸이 아니다.
**참조 핀**: moonlight-qt `c0c4d60` (아키텍처 맵 = m6-native-spike.md §A),
리서치 05 (ultra 레시피), W1 라이브 검증 결과 (spike §F).

---

## 0. 결정 요약

| # | 결정 | 선택 | 핵심 근거 |
|---|---|---|---|
| D1 | 앱 형태 | **단일 바이너리, 런타임 역할 스위치**(host/client/both) | owner 방향; 세션·페어링·설정 상태 공유 |
| D2 | 호스트 캡처/인코드 | **Phase A: Foundation Sunshine 관리형 서브프로세스 유지** → 장기 Rust 네이티브 재판단 | 스테이징/identity/패치 기계가 이미 검증됨(f1-ack 세션 4회); 재작성은 최대 리스크 |
| D3 | 시그널링/계정 서버 | **web-server를 라이브러리로 임베드**(actix in-process) | 우리 코드(src/); 클라이언트 프로토콜(§F) 불변 → 웹 클라 호환 자동 유지 |
| D4 | 클라 전송 | **client-transport 그대로** (W1 라이브 검증 완료) | CT-PROBE-OK; UI-무관 설계가 이미 목적 |
| D5 | 스트림 프레젠트 | **raw D3D11/DXGI (windows-rs)** — wgpu 아님 | ALLOW_TEARING·waitable(1)·VRR 제어는 스왑체인 플래그 직접 제어 필수 (moonlight-qt d3d11va.cpp 교훈, §4) |
| D6 | 디코드 | Phase A: **FFmpeg C API(libavcodec) + D3D11VA hwaccel** → Phase B: CUVID `ulMaxDisplayDelay=0` (4:4:4는 CUVID 전용) | moonlight-qt와 동일 기반(검증된 경로); 4:4:4는 NVIDIA D3D11VA에 프로파일 부재 (리서치 05 §1-A) |
| D7 | 오디오 | **WASAPI exclusive** event-driven 128f(2.67ms) + libopus | 경쟁 전무 축; moonlight shared ~15ms 대비 5–10ms 우위 (spike §C-3) |
| D8 | 입력 | RawInputBuffer(+RIDEV_NOLEGACY) → GameInput; **기존 input DataChannel 라벨 재사용** | 스트리머 수신부 무수정 (mouse_relative/keyboard/… 라벨 이미 존재) |
| D9 | UI 셸 | **egui(chrome) + 전용 스트림 HWND 자식창** 분리 | 스트림 표면은 D5의 raw 스왑체인이어야 함; UI는 셸과 독립 교체 가능 |
| D10 | moonlight-qt 포크 W2 | **보류** — 통합 앱 A0가 같은 목적 달성 시 스킵, Gate C 일정 압박 시에만 부활 | 포크 글루는 재사용 불가 투자 (owner 방향) |

## 1. 현재 자산 지도 — 무엇이 살아남나

```
살아남는 엔진 (UI-무관, 통합 앱에 그대로):
  transport-core/      fec + fec_wire + video_rx (순수, 262+ tests)
  client-transport/    flow + session + tls + frame_queue + C ABI (라이브 검증)
  streamer/            호스트 송신 엔진 (cc/abr/fec_sender/qu_relay/bitrate_apply)
  src/ (web-server)    계정·페어링·시그널링 (D3로 라이브러리화)
  common/              프로토콜 타입 단일 소스
  docs/host-patches/   Foundation capability+ack 패치 (컴파일·라이브 검증)

교체/재작성 대상:
  web/ UI 전체         (C단계 전면 개편 — 프로토콜은 불변)
  moonlight-qt 포크 글루  (착수 전 — D10으로 보류)
```

## 2. 토폴로지

```
betterparsec.exe (단일 바이너리)
├─ role: host  ──┬─ embedded web-server (actix, in-process thread)
│                ├─ Foundation Sunshine (관리형 서브프로세스, 기존 stage/identity 기계)
│                └─ streamer (서브프로세스, 기존 IPC — crash 격리 유지)
├─ role: client ─┬─ client-transport session (WebRTC/시그널링, W1 완성)
│                ├─ decode: FFmpeg D3D11VA → (B) CUVID 4:4:4
│                ├─ present: D3D11 FLIP_DISCARD [+ALLOW_TEARING+waitable(1) in B]
│                ├─ audio: WASAPI [shared in A → exclusive in B] + opus
│                └─ input: RawInput → 기존 DataChannel 라벨
└─ shell: egui chrome + 스트림 전용 HWND + 공통 세션 UX 상태머신
                          (cursor P1 auto-lock · 스톨 워치독 · immersive 토글)
```

- host↔client 동시(both): LAN 파티/듀얼PC 시나리오 — 프로세스 격리 구조라 공짜.
- 웹 클라: 임베디드 web-server에 그대로 접속 (프로토콜 §F 불변) — high 티어 유지.

## 3. moonlight-qt 참조 매핑 (핀 c0c4d60)

### 가져온다 (개념/교훈 포팅 — 코드 복사 아님)
| 소스 | 무엇 | 우리 구현 |
|---|---|---|
| `ffmpeg.cpp` Pacer(:499) | presentationTimeUs 기반 프레임 페이싱 + 정책 노브 | present 루프의 pacing 정책(게임=최저지연/데스크톱=지터흡수) — chunk header `timestamp_us` 사용 |
| `d3d11va.cpp:538-773` | FLIP_DISCARD + ALLOW_TEARING(CheckFeatureSupport 후) 구현 형태 | 그대로 재현. **함정 회피**: device-level `SetMaximumFrameLatency(1)` 금지(:550 주석, Present 블로킹) → **스왑체인** waitable object 방식 |
| `ffmpeg.cpp:991-1051` + `cuda.cpp` | CUVID 경로 존재·pass-1 강등 이유("NVIDIA+Wayland용") | Windows에서 4:4:4일 때 CUVID를 1순위로 — `delay=0`, `extra_hw_frames` 1 |
| `session.cpp:658-700` | 디코더 초기화 스테이지 순서 | 우리 세션 라이프사이클 상태 정의 참조 (§F 상태머신 확장) |
| PR #1282 | HEVC 4:4:4 협상 + 2× 비트레이트 배수 | 협상 파라미터 기준값 |
| moonlight-common-c RFI | 손실 복구 레퍼런스 | 이미 우리 포크에 존재 — Tetrys FEC와 병존 판정은 Gate B |

### 안 가져온다
- Qt/SDL 셸, RTSP/ENet 스택(WebRTC로 대체됨), `LiWaitForNextVideoFrame`
  pull 큐(→ FrameQueue), moonlight-common-c 클라 전체(→ client-transport),
  SDL 오디오(shared 15ms), SDL 입력 펌프.

## 4. 클라 미디어 스택 상세

### 4-1. 디코드 (D6)
- **A**: libavcodec(prebuilt, moonlight-qt-deps와 동일 계열) + `d3d11va`
  hwaccel → `AVFrame(D3D11 texture)` 그대로 프레젠트 텍스처로.
  DecodeUnit(Annex-B) → `av_parser` 없이 완성 프레임 단위 `send_packet`
  (FrameQueue가 프레임 경계 보장 — video_rx가 이미 재조립).
- **B**: CUVID 직결 — `ulMaxDisplayDelay=0`, `CUVID_PKT_ENDOFPICTURE`,
  HEVC Rext 4:4:4 (Turing+). CUDA-D3D11 interop로 표면 공유.
- 바인딩: `ffmpeg-sys-next`(A) → `cudarc`/자체 cuvid-sys(B). 라이선스:
  LGPL 동적링크 유지.

### 4-2. 프레젠트 (D5)
- 스트림 전용 자식 HWND + `IDXGISwapChain2`:
  `FLIP_DISCARD | FRAME_LATENCY_WAITABLE_OBJECT` (+vsync-off 시
  `ALLOW_TEARING`), `SetMaximumFrameLatency(1)` **스왑체인에**,
  렌더 루프 `WaitForSingleObjectEx(waitable)` 선행.
- 검증 지표: PresentMon으로 present-to-display ~3ms 재현 (리서치 05 §1-D).

### 4-3. 오디오/입력
- 오디오 A: WASAPI shared(cpal 가능) — B: exclusive event-driven 신규
  (~250 LOC, spike §C-3 스펙). opus 디코드는 `audiopus`.
- 입력: Win32 메시지 루프(스트림 HWND) + `GetRawInputBuffer`;
  와이어는 웹 클라와 동일한 InboundPacket 직렬화(스트리머
  `on_data_channel` 라벨 계약) — **송신부만 신규, 수신부 무수정**.

## 5. 공통 세션 UX 상태머신 (M4 항목의 배치)

`session-ux` 순수 모듈(Rust + TS 미러, transport-core 패턴 동일):
- **cursor P1**: `cursor` 채널 자동 lock/unlock (cursor-channel.md §3)
- **스톨 워치독**: 프레임 수신 타임스탬프 감시 → IDR → ICE restart →
  재접속 사다리 (현장 이슈 #1)
- **immersive**: 전체화면 + 커서 lock + (네이티브) RawInput 캡처 /
  (웹) Keyboard Lock — 한 상태머신의 두 백엔드
네이티브·웹이 같은 상태 전이표를 공유해야 UX가 갈라지지 않는다.
테스트는 transport-core 방식(순수 로직 + 시나리오 패리티)으로.

## 6. 단계 계획

| 단계 | 산출물 | 규모(추정) | 검증 |
|---|---|---|---|
| **A0 클라 first light** | `app-native/` crate: winit/Win32 창 + egui 최소 셸 + client-transport 연결 + FFmpeg D3D11VA 디코드 + FLIP_DISCARD 프레젠트 + WASAPI shared 오디오 | ~1.5–2k LOC | 로컬 스트림이 네이티브 창에 렌더; ct-probe 지표(first-frame/fps) 재확인 |
| **A1 호스트 롤** | web-server 라이브러리화 + Sunshine 서브프로세스 관리(기존 스크립트를 Rust로) + 페어링/역할 UI | ~1k LOC + 리팩토링 | 웹 클라가 통합 앱 호스트에 접속; paired 회귀 재실행 |
| **A2 입력 왕복** | RawInput → input 채널 송신; 마우스/키보드 실사용 | ~0.5k | 로컬 조작 왕복; 입력 지연 조계측 |
| **B ultra** | CUVID 4:4:4 + waitable/tearing + WASAPI exclusive + cursor P1 + 스톨 워치독 + immersive | 스파이크 추정치 준용 | **Gate C 외부 input-to-photon** — 지연 왕좌 판정 |
| **C UI 전면 개편** | 웹+네이티브 공통 디자인 시스템, 새 UI | 별도 설계 | 제품 퍼널 게이트 |

의존성: A0는 오늘 완성된 W1 위에 바로 선다(추가 프로토콜 작업 0).
A1은 src/ bin→lib 분리 리팩토링이 선행(작음 — actix 서비스 팩토리가
이미 함수형: `api_service()`/`web_service()`).

## 7. 리스크

1. **FFmpeg 바이너리 배포**(Windows prebuilt 조달·라이선스 절차) — A0 첫 주에 해소.
2. **입력 와이어 미러** — InboundPacket 직렬화를 common으로 승격해야 수동 미러 방지 (A2 선행 작업).
3. **egui↔D3D11 공존** — 별도 HWND 분리로 회피(합성 불필요); 실패 시 chrome을 Win32 네이티브로 강등.
4. **Sunshine 서브프로세스 수명주기**(서비스 충돌/포트) — 기존 swap 기계 이식으로 완화, 단 사용자 머신 일반화 필요.
5. 일정 리스크 — 각 단계에 검증 게이트; A0가 4주를 넘기면 D10 재평가(포크 W2 부활).

## 8. 로드맵 반영

- M6 W2(포크 글루) → **보류(D10)**, 통합 앱 A0~B가 대체.
- M4의 cursor/워치독/immersive는 §5 `session-ux`로 구현 위치 확정.
- Gate C 계측 대상 = 통합 앱 클라 롤(B 단계 완료 시점).
