# M6 네이티브 클라이언트 스파이크

**Date**: 2026-07-14  
**Pin**: moonlight-qt c0c4d60, moonlight-common-c 2ea4775  
**목적**: research/05 §1-E "전송 이식 2–4주" 추정 검증 + 전송 교체 접합 지점 확정  
**범위**: 소스 기반 타당성·공수 조사. moonlight-qt 코드 변경 없음.

---

## A. moonlight-qt 아키텍처 맵

### A-1. 세션 생명주기 — LiStartConnection 진입

`app/streaming/session.cpp:1689`

```cpp
int err = LiStartConnection(&hostInfo, &m_StreamConfig, &k_ConnCallbacks,
                            &m_VideoCallbacks, &m_AudioCallbacks,
                            NULL, 0, NULL, 0);
```

**흐름**: `Session::start()` (session.cpp:1732) → 비동기 스레드 `AsyncConnectionStartThread` 생성
→ 해당 스레드 내에서 `LiStartConnection` 호출 → 완료 후 `Session::exec()` (SDL 이벤트 루프) 진입.

**세 struct 등록**:

| struct | 등록 위치 | 주요 콜백 |
|---|---|---|
| `CONNECTION_LISTENER_CALLBACKS k_ConnCallbacks` | session.cpp:50–64 (정적) | `clStageStarting`, `clStageFailed`, `clConnectionTerminated`, `clLogMessage`, `clRumble`, `clConnectionStatusUpdate`, `clSetHdrMode` 등 13개 |
| `DECODER_RENDERER_CALLBACKS m_VideoCallbacks` | session.cpp:661–662 (init), 514–520 (capabilities/pull 분기) | `setup = drSetup`, `submitDecodeUnit = drSubmitDecodeUnit` (push 모드) 또는 null (pull 모드 + `CAPABILITY_PULL_RENDERER`) |
| `AUDIO_RENDERER_CALLBACKS m_AudioCallbacks` | session.cpp:703–707 | `init = arInit`, `cleanup = arCleanup`, `decodeAndPlaySample = arDecodeAndPlaySample` |

moonlight-common-c가 통과하는 스테이지(`STAGE_PLATFORM_INIT` ~ `STAGE_INPUT_STREAM_START`, 총 12단계, Limelight.h:374–385)는 RTSP 핸드셰이크, ENet 컨트롤 스트림, UDP 비디오/오디오 소켓 바인딩을 포함한다. 커스텀 전송으로 교체 시 이 단계 전체가 우회 대상이다.

### A-2. 비디오 Decode Unit 수신 경로

moonlight-common-c 내부 흐름 (`mlc/src/`):
```
VideoStream.c → RtpVideoQueue.c (재정렬 + RS FEC) → VideoDepacketizer.c (프레임 조립)
  → DecoderRendererSubmitDecodeUnit 콜백 호출
```

**DECODE_UNIT** (Limelight.h:144–194) 핵심 필드:

| 필드 | 타입 | 용도 |
|---|---|---|
| `frameNumber` | int | 단조 증가 프레임 번호 |
| `frameType` | int | `FRAME_TYPE_PFRAME(0)` 또는 `FRAME_TYPE_IDR(1)` |
| `receiveTimeUs` / `enqueueTimeUs` | uint64_t | 지연 측정 (수신 ~ 큐 삽입) |
| `presentationTimeUs` | uint64_t | 프레임 페이싱 기준 |
| `rtpTimestamp` | uint32_t | 90kHz RTP 타임스탬프 |
| `fullLength` | int | 전체 버퍼 바이트 수 |
| `bufferList` | `PLENTRY` | Annex-B 단편 linked list (`BUFFER_TYPE_SPS/PPS/VPS/PICDATA`) |
| `hdrActive` / `colorspace` | bool / uint8_t | HDR/색공간 힌트 |

moonlight-qt의 FFmpegVideoDecoder는 **`CAPABILITY_PULL_RENDERER`**를 설정(ffmpeg.cpp:153)하므로, moonlight-common-c는 콜백 대신 내부 큐에 DECODE_UNIT을 쌓는다. 디코더 자체 스레드가 `LiWaitForNextVideoFrame()` / `LiCompleteVideoFrame()`으로 풀(pull) 방식으로 소비한다. **커스텀 전송에서 이 API를 우회하려면 이 pull 루프를 자체 프레임 큐로 교체해야 한다** (B절 참조).

`drSubmitDecodeUnit` (session.cpp:358–385): `SDL_TryLockMutex(m_DecoderLock)` → `decoder->submitDecodeUnit(du)` → 리턴 `DR_OK` 또는 `DR_NEED_IDR`. 실제 decode 경로는 `IVideoDecoder::submitDecodeUnit()` → `FFmpegVideoDecoder::submitDecodeUnit()` → `avcodec_send_packet()`.

**외래 전송에서 DU 주입 가능 지점**: `IVideoDecoder::submitDecodeUnit()` 경계.  
구체적으로: `Session::drSubmitDecodeUnit()` 함수를 직접 호출하거나, IVideoDecoder 포인터를 직접 소유한 스레드에서 lock을 거쳐 `decoder->submitDecodeUnit(du)` 호출.

### A-3. 오디오 수신 경로

moonlight-common-c → `arDecodeAndPlaySample(char* sampleData, int sampleLength)` (audio.cpp:150)

- **포맷**: 원시 **Opus 멀티스트림 패킷** (sampleLength bytes). `opus_multistream_decode()` 또는 `opus_multistream_decode_float()`으로 즉시 디코딩.
- 백엔드 선택 (audio.cpp:41–49): (1) SLAudio (Steam Link 전용) → (2) **SdlAudioRenderer** (기본값). Windows에서는 SDL_OpenAudioDevice → WASAPI shared 모드. **WASAPI exclusive 경로는 현재 없음** — `IAudioRenderer` 인터페이스를 구현하는 신규 `WasapiExclusiveRenderer` 추가가 필요.
- 콜백은 moonlight-common-c의 오디오 수신 스레드에서 호출됨. 커스텀 전송에서는 DataChannel 수신 스레드 → `arDecodeAndPlaySample` 직접 호출로 교체 가능.

### A-4. 입력 이그레스

`SdlInputHandler` (input/input.cpp 외 5개 파일, 총 49회 `LiSend*` 호출):

| API | 파일 | 설명 |
|---|---|---|
| `LiSendMouseMoveEvent` / `LiSendMousePositionEvent` | mouse.cpp (7회) | 상대/절대 마우스 |
| `LiSendMouseButtonEvent` / `LiSendScrollEvent` | mouse.cpp | 버튼, 휠 |
| `LiSendKeyboardEvent` / `LiSendKeyboardEvent2` | keyboard.cpp (2회) | 키보드 |
| `LiSendMultiControllerEvent` / `LiSendControllerArrivalEvent` | gamepad.cpp (24회) | 패드 |
| `LiSendTouchEvent` / `LiSendPenEvent` | abstouch.cpp (8회), reltouch.cpp (7회) | 터치/펜 |

커스텀 전송 시 모든 `LiSend*` 호출을 커스텀 input 전송 함수로 교체해야 한다. moonlight-common-c의 input 암호화(AES) 로직도 재구현 필요 (혹은 DataChannel TLS가 대체).

### A-5. 컨트롤 플레인 — moonlight-common-c 의존 항목

moonlight-common-c가 담당하는 비(非)미디어 기능:

| 기능 | 파일 | 교체 필요 여부 |
|---|---|---|
| RTSP 핸드셰이크 (앱 launch/resume) | RtspConnection.c, SdpGenerator.c | **완전 우회** — 우리 streamer는 WebRTC signaling over HTTP/WS를 사용 |
| ENet 컨트롤 스트림 | ControlStream.c | **우회** — 우리 general DataChannel이 대체 |
| 연결 테스트 (`LiTestClientConnectivity`) | ConnectionTester.c | 스텁 필요 (clStageFailed에서 사용) |
| 통계 (`LiGetRTPVideoStats`, `LiGetRTPAudioStats`) | VideoStream.c, AudioStream.c | 자체 메트릭으로 교체 필요 |
| HDR 메타데이터 (`LiGetHdrMetadata`) | ControlStream.c | 우리 streamer DataChannel로 재전송 가능 |
| RTT 추정 (`LiGetEstimatedRttInfo`) | ControlStream.c | 우리 cc.rs의 RTT로 대체 |
| IDR 요청 (`LiRequestIdrFrame`) | ControlStream.c | 우리 f1-ack DataChannel로 대체 |

## B. 전송 교체 공수 평가

### B-1. 접합 옵션 3개

**Option-1: moonlight-common-c를 스텁화하고 IVideoDecoder 경계에서 DU 직접 주입**

- `LiStartConnection` / `LiStopConnection` / `LiSend*` 전부를 no-op 스텁 링크로 교체
- 별도 스레드에서 자체 전송 수신 → DECODE_UNIT 구성 → `Session::drSubmitDecodeUnit()` 직접 호출
- 장점: 완전한 moonlight-common-c 분리. 단점: RTSP/launch 대체를 처음부터 구현해야 하고 `CAPABILITY_PULL_RENDERER`의 `LiWaitForNextVideoFrame` 큐도 스텁해야 함 — FFmpegVideoDecoder 내부 pull 루프가 이 함수를 직접 호출하므로 링크 수준에서 리다이렉트가 필요.

**Option-2: moonlight-common-c VideoStream.c / RtpVideoQueue.c 패치 — 자체 수신기로 소스 교체**

- VideoStream.c의 UDP 소켓 / receiveThread를 우리 WebRTC 수신기로 교체
- 장점: FFmpegVideoDecoder pull 루프를 건드리지 않음. 단점: moonlight-common-c 서브모듈을 포크 패치해야 하므로 유지보수 부담이 크고, ENet 컨트롤 스트림과의 내부 결합이 복잡.

**Option-3 (추천): Rust cdylib 사이드카 + session.cpp 최소 글루**

- 별도 Rust crate (`client-transport`)를 cdylib으로 빌드: `extern "C"` ABI로 `ct_start()`, `ct_stop()`, `ct_set_video_callback(fn)`, `ct_set_audio_callback(fn)`, `ct_send_input(kind, data, len)` 노출
- session.cpp: `LiStartConnection` 호출 블록을 `ct_start()` + 스테이지 콜백 수동 에뮬레이션으로 교체
- FFmpegVideoDecoder pull 루프의 `LiWaitForNextVideoFrame` → 자체 `FrameQueue::waitPop()` 로 교체 (약 30 LOC 수정)
- `LiSend*` 함수들은 cdylib의 `ct_send_input()` 래퍼로 교체
- 장점: moonlight-common-c 서브모듈 손대지 않음, Rust 전송 코드(fec.rs, cc.rs) 재사용, 테스트 격리 가능

### B-2. Option-3 추천: 파일별 변경 LOC

| 파일 | 변경 성격 | 추정 LOC |
|---|---|---|
| `app/streaming/session.cpp` (2361 L) | `LiStartConnection` 블록 교체, 스테이지 콜백 에뮬, m_VideoCallbacks 재구성 | ~150 변경 |
| `app/streaming/session.h` | cdylib 헤더 include, `m_OurTransport` 멤버 추가 | ~30 변경 |
| `app/streaming/video/ffmpeg.cpp` (2119 L) | pull 루프의 `LiWaitForNextVideoFrame` → `FrameQueue::waitPop()` 교체 | ~50 변경 |
| 신규 `app/streaming/transport/our_transport.{cpp,h}` | C++ shim: cdylib 호출 래퍼, FrameQueue 구현, DECODE_UNIT 구성 | ~300 신규 |
| `app/streaming/input/` (6개 파일) | `LiSend*` 49개 호출 → `ct_send_input()` 래퍼로 교체 | ~100 변경 |
| `app/streaming/audio/audio.cpp` | `arDecodeAndPlaySample` DataChannel 콜백으로 재연결 | ~30 변경 |
| `moonlight-qt.pro` / `app.pro` | 신규 .cpp 파일 등록, cdylib 링크 | ~20 변경 |
| 신규 Rust crate `client-transport/` | WebRTC client, video_fec DataChannel 수신, Annex-B 재조립, DECODE_UNIT 출력, input 송신 | ~700–1000 신규 |
| **합계** | | **~1380–1680 LOC** |

### B-3. 주요 위험 및 스레딩 모델

**스레딩**: FFmpegVideoDecoder는 `CAPABILITY_PULL_RENDERER`를 사용(ffmpeg.cpp:153), 자체 디코더 스레드가 `LiWaitForNextVideoFrame()` 루프로 DECODE_UNIT을 가져간다. 커스텀 전송에서는 이 함수를 스텁하고, `our_transport.cpp`의 `FrameQueue` (mutex + condvar 또는 lock-free 큐)를 직접 폴링하도록 약 30 LOC를 교체. `m_DecoderLock` SDL mutex는 기존대로 유지 가능.

**RTSP 우회**: 우리 streamer는 WebRTC signaling (HTTP POST + ICE)을 사용하므로 moonlight-common-c RTSP 스택(`RtspConnection.c`, `SdpGenerator.c`) 전체가 불필요. `LiStartConnection` 대신 `ct_start(signal_url)` 호출. CONNECTION_LISTENER_CALLBACKS의 stage 콜백은 수동으로 순서대로 호출해 GUI 진행 표시를 유지.

**통계/OSD**: 현재 `LiGetRTPVideoStats()`, `LiGetRTPAudioStats()`, `LiGetEstimatedRttInfo()`를 호출하는 OSD 코드(overlaymanager.cpp 등)가 있음. 이들이 zero를 반환하므로 OSD stats가 비어있게 됨 — 1주차 내 필수 수정 사항은 아니지만 UX 열화 요인.

**Pacer 결합**: FFmpegVideoDecoder의 Pacer (ffmpeg.cpp:499) 는 `DECODE_UNIT.presentationTimeUs`를 사용. 우리 fec-framing.md의 chunk header에 `timestamp_us`가 있으므로 이 필드를 채울 수 있음 (fec-framing.md §chunk layer: `offset 9 u32 timestamp_us`).

### B-4. 2–4주 추정 타당성 — 주차별 분해

| 주차 | 작업 | 위험 |
|---|---|---|
| W1 | Rust cdylib 스켈레톤: WebRTC client 연결, video_fec DataChannel 수신, chunk 재조립, DECODE_UNIT 구성, C ABI 노출 | WebRTC ICE/DTLS 타이밍 |
| W2 | session.cpp glue: LiStartConnection 교체, FrameQueue, FFmpegVideoDecoder pull 루프 수정, Windows 빌드 통합 (Qt + cdylib) | CAPABILITY_PULL_RENDERER 스레드 교체 |
| W3 | 입력 경로 (6파일 LiSend* → ct_send_input), 오디오 DataChannel 연결, 스테이지 콜백 에뮬, 기본 스트리밍 확인 | 입력 암호화 (AES RI key) |
| W4 | OSD/stats 적응, ALLOW_TEARING 경로 확인, CUVID 4:4:4 실험적 활성화, 안정성 테스트 | CUVID pass-0 승격, 통계 API 교체 |

**판정**: 4주가 빠듯한 일정이며, 경험 있는 엔지니어 1명 기준으로 **현실적 범위는 3–5주**. 가장 큰 변수는 W1의 WebRTC ICE 연결 안정성(ICE restart 포함)과 W2의 FFmpegVideoDecoder pull 루프 교체.

## C. Ultra-레시피 삽입 지점 (research/05 §1-D)

### C-1. CUVID `ulMaxDisplayDelay=0` + HEVC 4:4:4 NVDEC 디코드

**현재 상태**: Windows에서 D3D11VA가 pass-0 (ffmpeg.cpp:1003), CUDA/CUVID가 pass-1 (ffmpeg.cpp:1048–1051)로 등록됨. pass-1 CUDA 렌더러(`CUDARenderer`)의 주석: "CUDA should only be used to cover the NVIDIA+Wayland case" — 즉 Windows에서는 의도적으로 CUDA path가 pass-0 우선순위에서 제외됨.

**문제**: HEVC RExt 4:4:4 프로파일은 D3D11VA NVIDIA 드라이버에 없음 (research/05 §1-A 확인). NVDEC(CUVID) 경로로만 HW 디코드 가능. 현재 moonlight-qt는 Windows에서 CUVID 경로를 명시적으로 선택하지 않는다.

**삽입 지점**:
- `ffmpeg.cpp:createHwAccelRenderer()` (ffmpeg.cpp:991) — pass-0 분기에 Windows + `videoFormat & VIDEO_FORMAT_MASK_YUV444` 조건으로 `CUDARenderer` 반환 추가 (~10 LOC)
- `CUDARenderer::prepareDecoderContext()` (cuda.cpp) — `av_dict_set(&options, "surfaces", "4", 0)` 및 cuvid 전용 옵션 설정. `ulMaxDisplayDelay=0`은 FFmpeg에서 `av_opt_set_int(ctx->priv_data, "delay", 0, 0)` 또는 `av_dict_set(&options, "delay", "0", 0)`으로 설정 (~20 LOC 추가)
- `m_VideoDecoderCtx->extra_hw_frames` (ffmpeg.cpp:547): CUVID의 경우 기본값 4 → `ulMaxDisplayDelay=0`에 맞게 `1`로 줄여야 함

**기반 존재 여부**: CUDARenderer와 CUDAGLInteropHelper (cuda.h, cuda.cpp) 이미 존재. ffnvcodec/dynlink_loader.h 연동도 구현됨. Windows에서 `HAVE_CUDA` 빌드 플래그만 활성화하면 코드 경로는 이미 컴파일된다. **기반 존재, 약 30 LOC 수정으로 가능**.

### C-2. 프레젠테이션 — FLIP_DISCARD / ALLOW_TEARING / waitable swapchain

**현재 상태 (d3d11va.cpp)**:
- `DXGI_SWAP_EFFECT_FLIP_DISCARD`: **이미 설정됨** (d3d11va.cpp:538)
- `DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING`: **이미 구현됨** — vsync=off 시 `CheckFeatureSupport(DXGI_FEATURE_PRESENT_ALLOW_TEARING)` 후 플래그 세팅, `DXGI_PRESENT_ALLOW_TEARING`으로 Present (d3d11va.cpp:573–773). **VRR tearing 경로는 이미 존재한다.**
- **waitable swapchain (MaxLatency=1)**: **부재**. d3d11va.cpp:550–554 주석에서 명시: `IDXGIDevice1::SetMaximumFrameLatency(1)` 사용 시 Present가 블로킹이 되므로 의도적으로 피함. 올바른 구현은 `IDXGISwapChain2::GetFrameLatencyWaitableObject()` + `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` + `SetMaximumFrameLatency(1)`을 스왑체인에 설정하는 것 (디바이스가 아닌 스왑체인에 설정).

**삽입 지점**: d3d11va.cpp `swapChainDesc.Flags` 설정 부분 (d3d11va.cpp:540). `DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT` 추가, `m_SwapChain->GetFrameLatencyWaitableObject()` 핸들 획득, 렌더 루프에서 `WaitForSingleObjectEx(handle, 1000, TRUE)` 추가 (~50 LOC).

**결론**: ALLOW_TEARING은 이미 완성. waitable swapchain만 추가 필요.

### C-3. 오디오 — WASAPI exclusive

**현재 상태**: 자동 백엔드 선택 (audio.cpp:41–49) — SLAudio (Steam Link) → **SdlAudioRenderer** (기본값). SDL은 WASAPI shared 모드 (~15ms 지연).

**삽입 지점**:
- `app/streaming/audio/renderers/renderer.h` — `IAudioRenderer` 인터페이스 (prepareForPlayback, getAudioBuffer, submitAudio 등)를 구현하는 `WasapiExclusiveRenderer` 신규 클래스
- `app/streaming/audio/audio.cpp:41` — Windows에서 `TRY_INIT_RENDERER(WasapiExclusiveRenderer, opusConfig)` 를 최우선으로 추가
- WASAPI exclusive event-driven: `IAudioClient::Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, ...)`, 128샘플 버퍼(2.67ms@48kHz). Opus 2.5ms 프레임 → 알고리즘 지연 5ms + 버퍼 2.67ms = **총 ~8ms**

**기반 존재 여부**: SDL audio 백엔드만 있음. WASAPI exclusive는 신규 구현 (~250 LOC).

### C-4. 입력 — RawInputBuffer / GameInput

**현재 상태**: `SdlInputHandler` (input.cpp)가 SDL 이벤트 펌프(SDL_PollEvent)에서 마우스/키보드/패드 이벤트를 수신. SDL은 Windows에서 RAWINPUT을 내부적으로 사용하지만 8kHz 플러드 패킷 처리에는 최적화되지 않음.

**삽입 지점**:
- 마우스: `session.cpp`의 SDL 이벤트 루프 바깥에 별도 Win32 메시지 루프 추가, `GetRawInputBuffer()`로 마우스 이벤트 배치 읽기 → `ct_send_input(MOUSE_MOVE, ...)`. `RegisterRawInputDevices(RIDEV_INPUTSINK | RIDEV_NOLEGACY)` 필요.
- 키보드: 시스템 키 캡처는 현재 `SDL_HINT_GRAB_KEYBOARD`로 처리. Win32 `WH_KEYBOARD_LL` 훅으로 Win키/Alt-Tab 처리 보강.
- GameInput (v3.4): `IGameInput::CreateReading()` → 통합 타임스탬프. SDL 패드와 병행 또는 대체 가능.

**기반 존재 여부**: SDL 이벤트 기반 입력 완성. RawInput/GameInput은 **신규** — 별도 작업 (W4 이후 백로그 권장).

## D. Windows 빌드 타당성

### D-1. 빌드 시스템

- **qmake + .pro 파일** (최상위 `moonlight-qt.pro` → `app/app.pro`, `moonlight-common-c/`, `qmdnsengine/`, `h264bitstream/`). Qt6 또는 Qt5.15 필요. `scripts/build-arch.bat` 참조: qmake PATH 탐색 후 `scripts/setup-deps.ps1` 실행.
- `setup-deps.ps1`: `moonlight-stream/moonlight-qt-deps` GitHub Releases에서 tag v8의 `windows-x64.zip` 다운로드 → `libs/windows/`에 압축 해제. 포함: SDL2, FFmpeg, OpenSSL, Opus, ffnvcodec 헤더 등 prebuilt 정적 라이브러리.

### D-2. 이 머신 현재 상태

```
where.exe qmake    → (빈 출력, Qt 없음)
C:/Qt              → (없음)
C:/Program Files/Qt → (없음)
```

Qt 미설치 확인. Rust toolchain(cargo)은 프로젝트 빌드 이력상 존재 가정.

### D-3. 빌드 환경 설치 예상 공수

| 단계 | 내용 | 시간 |
|---|---|---|
| Qt 설치 | Qt Online Installer에서 Qt 6.8 (MSVC 2022 x64) 선택 | ~1.5시간 (다운로드 포함) |
| MSVC 확인 | Visual Studio 2022 또는 Build Tools 이미 존재 가능성 높음 (Rust MSVC toolchain 사용) | 0–30분 |
| setup-deps.ps1 | prebuilt deps 다운로드 (~500MB) | ~20분 |
| 첫 빌드 | `build-arch.bat` | ~10–20분 |
| **합계** | | **~2–3시간** |

### D-4. 권장 시점

포크 착수 시 즉시 환경 구축 권장. 스파이크 단계에서는 설치 불필요 — 이 문서가 코드 분석 스파이크이므로 빌드는 Option-3 W2에서 처음 필요.

## E. 판정 테이블

### E-1. 전송 이식 2–4주 추정

**판정: OPTIMISTIC** — 현실적 범위는 **3–5주** (엔지니어 1명 기준)

| 항목 | 추정 공수 | 근거 |
|---|---|---|
| W1: Rust cdylib 전송 코어 | 7–10일 | WebRTC ICE+DTLS+video_fec DataChannel 클라이언트, fec.rs 재사용, ICE restart 처리 포함 |
| W2: session.cpp 통합 + 빌드 | 5–7일 | LiStartConnection 교체, FrameQueue, CAPABILITY_PULL_RENDERER 루프 수정, Qt cdylib 링크 |
| W3: 입력 + 오디오 + 스테이지 | 4–5일 | 6파일 LiSend* 교체, DataChannel 오디오, 스테이지 콜백 에뮬 |
| W4: 안정화 + ultra 옵션 | 3–5일 | OSD stats, CUVID pass-0, ALLOW_TEARING 경로 확인 |
| **합계** | **19–27일** | 4주 = 20 영업일: 빠듯하고 버퍼 없음 |

낙관적이었던 이유: RTSP/launch를 완전 우회하는 비용과 CAPABILITY_PULL_RENDERER 스레드 교체가 원래 추정에서 과소평가됨.

### E-2. 추천 접합 방식 — **Option-3 (Rust cdylib sidecar)**

- moonlight-common-c 서브모듈 무수정 (유지보수 부담 없음)
- Rust fec.rs / cc.rs 재사용 — 전송 혁신(CC/FEC/슬라이스) 보존
- IVideoDecoder / IAudioRenderer / 렌더러 파이프라인 전체 유지
- 격리 테스트 가능 (cdylib 독립 unit test)

### E-3. Top-5 위험 및 대응

| # | 위험 | 심각도 | 대응 |
|---|---|---|---|
| 1 | **CAPABILITY_PULL_RENDERER 루프 교체**: `LiWaitForNextVideoFrame` 내부 큐 제거 후 FFmpegVideoDecoder 디코더 스레드가 데드락 | 높음 | ffmpeg.cpp pull 루프를 자체 `FrameQueue::waitPop()` 30 LOC로 교체. W2 초반에 검증. |
| 2 | **RTSP/launch 스텁 불완전**: CONNECTION_LISTENER 스테이지 순서가 틀리면 decoder 초기화 시퀀스 오작동 | 높음 | 스테이지 콜백을 명시적 순서로 수동 호출. session.cpp:658–700 초기화 시퀀스를 그대로 재현. |
| 3 | **CUVID 4:4:4 경로** — Windows에서 CUDA pass-0 승격 시 D3D11VA와의 VRAM 공유 충돌 가능 | 중간 | `HAVE_CUDA` 플래그 활성화 후 4:4:4 + non-4:4:4 경로 분리 테스트. 비 4:4:4는 D3D11VA 유지. |
| 4 | **OSD/Stats 공백**: `LiGetRTPVideoStats()` 등이 zero 반환 → 스탯 오버레이 공백 | 낮음 | W4에서 VideoTransportMetrics (streamer/src/transport/metrics.rs 참조) DataChannel로 전달. 단기 허용. |
| 5 | **입력 AES 암호화**: moonlight-common-c는 RI key를 AES-CBC로 암호화. 우리 input 전송이 DataChannel TLS 위에서 plain text라면 Sunshine 포크 호환성 확인 필요 | 중간 | Sunshine 포크의 input 채널 검증 여부 확인. DataChannel DTLS가 대체 암호화로 충분한지 판단. |

### E-4. Pin

```
moonlight-qt:        c0c4d60
moonlight-common-c:  2ea4775 (enet + nanors 서브모듈 포함)
```

## F. W1 구현 계약 — client-transport 접속 프로토콜 (2026-07-14 소스 핀)

W1 잔여(WebRTC/signaling 클라)를 위해 웹 클라 소스에서 확정한 사실.
구현은 이 계약에 맞추고, 변경 발견 시 이 절을 갱신한다.

### F-1. 접속 순서 (라이브 검증 완료, 2026-07-14 ct-probe)

1. **인증**: `POST /api/login` (PostLoginRequest JSON) → 세션 쿠키.
2. **Signaling WS**: `ws(s)://{host}/api/host/stream` (쿠키 인증).
3. WS open → 클라 `StreamClientMessage::Init { host_id, app_id,
   video_frame_queue_size, audio_sample_queue_size }`. app_id는
   `GET /api/apps?host_id=…`의 실제 Sunshine 앱 ID(예: Desktop
   881448767) — 임의 값이면 "app was not found"로 종료.
4. 서버 `Setup { ice_servers }` → 클라는 **연달아** peer 생성 +
   `SetTransport(WebRTC)` + `StartStream { settings }` 송신.
   ⚠ **StartStream은 협상 완료를 기다리면 안 된다** — 스트리머는
   StartStream 수신 후에야 moonlight 세션과 offer 생성을 진행하므로
   peer-connected를 기다리면 상호 대기 교착 (라이브 프로브로 확인,
   웹 클라도 동일 순서: index.ts tryWebRTCTransport → startStream).
5. **스트리머가 offerer** — `WebRtc(Description(offer))` 수신 →
   answer 회신. ICE candidate 양방향 (peer/remote-description 전
   도착분은 버퍼링).
6. 서버 `ConnectionComplete { format, width, height, fps, audio_* }` —
   디코더/DECODE_UNIT 셋업 파라미터 전부 여기서. ⚠ WS의
   ConnectionComplete가 peer-connected 콜백보다 **먼저** 올 수 있다
   (실측 130 ms 역전) — 상태머신은 업그레이드 전용이어야 한다.
   종료는 `ConnectionTerminated { error_code }`.

### F-2. DataChannel/FEC 계약

- DataChannel은 전부 **offerer(스트리머)가 개설**, 클라는 `datachannel`
  이벤트(webrtc-rs: `on_data_channel`)로 라벨 매칭: `video_fec`,
  `video_fec_ack`, `video_qu` + TransportChannelId 컨트롤 채널들.
- FEC 구독: 클라가 `video_fec_ack`에 `[0x01]` 송신 → 호스트 FEC 송신 활성.
  이후 수신 메시지를 `ct_receiver_on_message`로, `ct_receiver_poll_ack`
  결과(u32 LE)와 `ct_receiver_poll_needs_idr`(`[0x00]`)를 `video_fec_ack`로
  회신 (fec-framing.md §8, transport-core::fec_wire 인코딩 재사용).
- 타이머: `ct_receiver_tick`을 ~50 ms 주기로 (ACK cadence, 인코더 윈도 유지).

### F-3. 구현 메모

- 크레이트 의존: tokio + webrtc-rs(워크스페이스 핀 c9675e2) +
  `common`(StreamClientMessage 등 타입 재사용, 수동 JSON 미러 금지) +
  WS/HTTP 클라(tokio-tungstenite 또는 reqwest — cargo-shear 게이트 유의).
- TLS: web-server는 자가서명(server/tls) — 네이티브 클라는 인증서 핀
  옵션(`ct_start` 파라미터로 cert fingerprint) 기본, dev 플래그로만 무검증.
- C ABI 확장: `ct_start(base_url, user, pass, host_id, app_id, …)` /
  `ct_stop` — 기존 CtReceiver poll/wait 계약은 불변.
