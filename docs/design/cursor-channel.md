# Cursor channel — 게임/데스크톱 단일 커서 설계 (Parsec·DCV 조사 기반)

**Status**: 설계 (2026-07-14). 4-트랙 웹 리서치(Parsec SDK·DCV docs·Sunshine
소스·Win32/DXGI docs) 종합. **Owner 요구**: 게임에서 360°만이 아니라
**인게임 메뉴 커서가 보여야** 하고, 데스크톱에서 커서가 1개여야 한다.

## 1. 오늘 라이브 테스트 증상의 근본 원인 (소스 확인됨)

Sunshine의 DXGI 캡처는 커서를 프레임에 직접 받지 않는다 — DXGI Desktop
Duplication이 커서를 **별도 메타데이터**(`DXGI_OUTDUPL_POINTER_POSITION.Visible`
+ `GetFramePointerShape`)로 주고, Sunshine이 `display_cursor && Visible`일 때만
**수동으로 blend**한다 (display_ram.cpp:251,344 / display_vram.cpp:1402,1407).

체인: 게임이 `ShowCursor(FALSE)`/DirectInput exclusive → `Visible=false` →
Sunshine blend 스킵 → 게임이 메뉴 커서를 자기 D3D 프레임에 소프트웨어로
그리지 않으면(OS 하드웨어 커서 의존) → **영상 어디에도 커서 없음**. 클라는
pointer lock으로 로컬 커서도 숨김 → "커서가 아예 안 보임" 증상 완성.

부가 사실: `display_cursor`는 config 없이 런타임 토글(Ctrl+Alt+Shift+N)만
존재(globals.cpp:10, input.cpp:285). Moonlight 프로토콜에는 host→client 커서
채널이 **없다**(Limelight.h 콜백에 부재; moonlight-qt #1273이 미해결 요청).

## 2. 두 제품의 실제 메커니즘 (조사 결과)

| | Parsec | DCV |
|---|---|---|
| 커서가 영상에? | **아니오** — DXGI 분리 메타를 그대로 씀, 인코딩에 미포함 | **예(absolute 모드)** — 서버 OS 커서가 구워짐 |
| 모양 전송 | `ParsecCursor{w,h,hotX,hotY,hidden,relative,imageUpdate}` + RGBA/PNG 버퍼, 변경 시에만 | 없음 (Extension SDK의 SetCursorPoint는 네이티브 전용·위치만) |
| 클라 렌더 | OS 커서/오버레이로 직접 그림 (SDL_CreateColorCursor) | absolute: 구운 커서 + 로컬 커서가 같은 좌표에 **겹쳐** 1개처럼 보임 |
| FPS↔메뉴 전환 | **호스트 권위**: `modeUpdate+relative` 플래그로 클라 lock/unlock 지시, 해제 시 positionX/Y로 커서 재배치 | **호스트 권위**: "원격 OS 커서 숨김→relative(lock), 보임→absolute" 자동 전환 (GameLift Streams 문서에 규칙 명문화) |

**공통 핵심 = 커서 가시성의 호스트 권위 + 자동 모드 전환.** 사용자가
mouseMode를 수동으로 고르는 우리 현재 구조가 근본 결함이다. 게임이 커서를
숨기고/보일 때마다 호스트가 클라에게 lock/unlock을 지시해야 메뉴가 산다.

## 3. 설계

### P1 — DCV식 자동 모드 전환 (커서 가시성 채널) ← 사용자 문제 직행

스트리머는 호스트 머신에서 실행되므로 Sunshine 무포크로 커서 상태를 읽는다.

- **호스트(스트리머)**: 60Hz 폴링 태스크 — `GetCursorInfo()`
  (`CURSOR_SHOWING`/`CURSOR_SUPPRESSED`/0) + `GetPhysicalCursorPos` 물리 좌표.
  상태 변화 또는 이동 시 메시지 송신.
- **채널**: `cursor` DataChannel (reliable·ordered — 상태 전이 유실 불가,
  트래픽 60Hz×13B 수준이라 재전송 부담 없음).
- **와이어 (LE)**:
  ```
  POS:   u8 kind=0 | u8 visible(0/1) | i32 x | i32 y | u16 vw | u16 vh
         (x,y = 물리 픽셀; vw,vh = 캡처 대상 모니터 물리 해상도 — 클라가
          비디오 rect로 사상)
  ```
- **클라 (relative 정책을 "auto"로 확장)**:
  - `visible=false` → pointer lock 유지/진입 (FPS 조준: 커서 0개, 정상)
  - `visible=true` → **lock 해제 + absolute(follow) 입력으로 자동 전환** →
    Sunshine이 Visible=true라 커서를 구워주므로 **메뉴 커서가 영상에 보임**.
    로컬 커서는 video 영역에서 `cursor:none`(DCV는 겹침으로 해결하지만 우리
    follow는 미스얼라인 이슈가 있어 숨김이 안전 — §5)
  - 전환 히스테리시스 150ms (게임 로딩 중 깜빡임 방지)
- **Sunshine 커서 구움**: P1에서는 **그대로 켜둠** (absolute 구간의 커서
  소스). display_cursor 끄기는 P2부터.

이것으로 사용자의 두 요구가 해결된다: FPS 360°(lock) + 인게임 메뉴 커서
(자동 unlock + 구운 커서).

### P2 — Parsec식 클라 렌더 (모양 채널, zero-latency 커서)

- SHAPE 메시지 추가: `u8 kind=1 | u32 shape_id | u16 w,h | u16 hotX,hotY |
  PNG(RGBA)` — `GetIconInfoExW`→`GetDIBits`로 추출, hCursor 핸들 변화 감지
  시에만 전송. POS에 `u32 shape_id` 부가.
- 클라: absolute 구간에서 CSS `cursor: url(data:image/png...) hotX hotY`
  (Chrome 한계 32×32 기본/최대 128px — 초과 시 오버레이 폴백), relative 구간
  메뉴는 오버레이 `<div>`(pointer lock 중에도 DOM 렌더 가능 — Guacamole/
  noVNC 선례).
- Sunshine `display_cursor`를 세션 중 off (P2a: 페어링 후 Ctrl+Alt+Shift+N
  주입으로 토글 / P2b: 포크에 config 3줄 추가) → 커서가 영상에서 빠지고
  전 구간 클라 렌더 = **DCV보다 나은**(모양까지 zero-latency) 단일 커서.
- MASKED_COLOR/MONOCHROME 커서(I-beam 등)는 XOR 시맨틱이라 PNG 알파로
  근사(Parsec도 동일 근사) — 흰/검 반전 커서만 시각 차이, 수용.

### 경계·주의 (조사에서 나온 함정)

- `CURSOR_SUPPRESSED`(터치 입력)는 hidden으로 취급.
- 좌표: `GetCursorPos`는 DPI 가상화 영향 → **GetPhysicalCursorPos** 사용.
  다중 모니터 v1 = 캡처 모니터(주 모니터) 밖 좌표는 visible이어도 클라가
  범위 클램프 후 표시 생략.
- WGC 백엔드는 커서가 OS에서 미리 구워져 옴(분리 불가) — P2의 구움 제거는
  DXGI 백엔드 전제. 현재 Sunshine 기본이 DXGI라 OK; WGC 강제 환경은 P1까지만.
- AMF 인코딩에서 커서 미표시 알려진 버그(Sunshine #513) — 진단 시 참고.

## 4. 구현 배치

| 단계 | 스트리머 | 웹 | 규모 |
|---|---|---|---|
| P1 | Win32 폴링 태스크 + `cursor` 채널 (+세대 가드, cc/fec와 동일 패턴) | auto 모드(스토어 1필드) + lock/unlock 전환 + cursor:none | 소–중 |
| P2 | SHAPE 추출·전송, display_cursor 제어 | CSS cursor/오버레이 렌더 | 중 |

## 5. 별도 발견 — follow 모드 이중커서 "미스얼라인"

DCV absolute가 1커서로 보이는 이유는 로컬/원격 커서가 **같은 좌표에 겹치기**
때문. 우리 follow에서 둘이 어긋나 보였다면 `getStreamRect` 좌표 사상(레터박스
오프셋/DPI)이 어긋났을 가능성 — P1 구현 시 좌표 사상 단위 테스트로 함께 검증.

## 출처 (조사 에이전트 인용 원본)

Parsec SDK parsec.h(ParsecCursor/ParsecHostSubmitCursor/CLIENT_EVENT_CURSOR),
examples/client/main.c(SDL 모드 전환); AWS DCV admin/websdk 문서
(enable-relative-mouse, autoPointerLock, GameLift Streams 모드 규칙); Sunshine
소스(globals.cpp, input.cpp, display_ram/vram/wgc.cpp); MS docs(DXGI outdupl
포인터 구조/GetCursorInfo/GetIconInfoEx/GetPhysicalCursorPos); moonlight-qt
#1273; NICE/Amazon 특허(12177280 등, 커서 제거+전용 채널 아키텍처).
