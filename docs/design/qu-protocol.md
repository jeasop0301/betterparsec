# QU (build-to-lossless) 채널·타일 프로토콜 (U4)

**Status**: Design — 채널·와이어 포맷·클라 합성 방식 결정. 호스트 인코드 경로는
인터페이스만 규정(Sunshine 포크, 중규모 — slice-qu-constraints.md §3).
**Date**: 2026-07-14.
**Source**: docs/design/slice-qu-constraints.md §3–4, docs/design/fec-framing.md
(채널 패턴), docs/research/05-native-client-dcv-composite.md §2 (DCV 기법),
streamer/src/transport/webrtc/mod.rs (채널 생성 패턴).

목표: 데스크톱 모드에서 모션 정지 후 화면을 **픽셀-퍼펙트**로 수렴시키고
(구조적 "뭉개짐" 해결), 정적 화면의 지속 대역을 ~0으로 만든다. 게임 모드는
QU off (research 05 §3).

---

## 1. 채널

fec-framing.md의 `video_fec`와 같은 생성 패턴(`transport/webrtc/mod.rs`의
채널 테이블), 단 **reliable + ordered 단일 채널**:

| channel | ordered | reliability | direction | carries |
|---|---|---|---|---|
| `video_qu` | true | reliable | 양방향 | host→client: config/tile/invalidate · client→host: subscribe/budget |

- FEC와 달리 ACK 전용 채널이 불필요하다: DataChannel은 양방향이고 reliable
  채널이므로 제어 메시지를 같은 채널로 되돌릴 수 있다.
- `general` 프로토콜 enum(`common/src/api_bindings.rs`)은 건드리지 않는다 —
  라벨 문자열로 식별하는 raw 채널 (fec-framing.md와 동일한 phase-1 원칙).
- 클라이언트가 `QU_SUBSCRIBE`를 보내기 전까지 호스트 측은 휴면(dormant) —
  video_fec의 subscribe 게이트와 동일한 기본-off 원칙.

## 2. 와이어 포맷 (binary, little-endian)

모든 메시지는 `u8 msg_kind`로 시작한다. host→client는 `0x0x`,
client→host는 `0x8x` 대역.

```
host → client
0x01 QU_CONFIG      u16 tile_w | u16 tile_h | u16 grid_cols | u16 grid_rows
                    | u32 epoch
0x02 QU_TILE        u32 epoch | u16 col | u16 row | u8 format | u8 flags
                    | u32 crc32_bgra | u32 payload_len | payload
0x03 QU_INVALIDATE  u32 epoch | u16 count | count × (u16 col | u16 row)
0x04 QU_EPOCH       u32 new_epoch          (전체 오버레이 클리어)

client → host
0x81 QU_SUBSCRIBE   u8 version (=1)
0x82 QU_BUDGET      u32 kbps               (0 = 일시 정지)
```

- **tile format**: `0 = PNG` (v1 필수), `1 = WebP-lossless` (후보),
  `2 = raw BGRA + zstd` (네이티브 티어 후보, reserved). PNG를 v1로 택한
  이유: 브라우저 `createImageBitmap` 네이티브 디코드(추가 디코더 배선 없음),
  무손실 보장, 정적 데스크톱 콘텐츠 압축률 양호. NVENC lossless HEVC 타일은
  타일별 디코더 세션이 필요해 웹 티어에서 기각.
- **crc32_bgra**: 타일의 원본 BGRA 픽셀 CRC32. 클라이언트가 디코드 후 재계산해
  픽셀-퍼펙트를 **전송 계층에서 자가 검증**할 수 있게 한다 (Gate E 계측의
  내부 크로스체크; 불일치 시 타일 폐기 + 로그).
- **epoch**: 해상도 변경·모드 전환·스트림 재설정마다 증가. 이전 epoch의
  TILE/INVALIDATE는 도착해도 무시된다 (reliable 채널이라 유실은 없지만
  재설정 경계에서 스테일 메시지가 순서상 뒤늦게 처리되는 것을 차단).
  fec/cc의 generation guard와 같은 계열의 방어.
- 기본 타일 크기 128×128 (QU_CONFIG로 호스트가 통지; 클라이언트는 수신값을
  따른다). grid = ceil(width/tile_w) × ceil(height/tile_h), 우/하단 가장자리
  타일은 잘린 크기.

## 3. 타일 생명주기와 video 트랙과의 레이스

1. 호스트(Sunshine 포크)가 DXGI dirty rects로 타일별 "안정 프레임 수"를
   추적, N프레임(기본 ~30) 연속 무변경 타일을 무손실 인코드해 `QU_TILE`로
   내려보낸다 (budget 내 페이싱, 오래된 dirty 순).
2. 클라이언트는 디코드한 타일을 오버레이에 그린다 — 이 시점부터 해당 영역은
   비디오 대신 무손실 픽셀이 보인다.
3. 모션 재개 시 호스트는 **해당 모션 프레임을 인코드하기 전에**
   `QU_INVALIDATE`를 먼저 송신한다. 클라이언트는 해당 타일의 오버레이를
   지워 비디오가 다시 보이게 한다.

**레이스 분석**: `video_qu`(SCTP reliable)와 비디오 RTP 트랙 사이에는 순서
보장이 없다. INVALIDATE를 모션 프레임 인코드 **전에** 보내므로 정상적으로는
INVALIDATE가 먼저 도착하지만, SCTP 큐가 밀린 경우 모션 프레임이 먼저 렌더될
수 있다 — 그 짧은 구간 동안 스테일 무손실 타일이 새 모션 위에 남는다.
- v1 판정: **허용**. 반대 방향 오류(비디오가 무손실 위에 그려짐)는 QU의
  목적상 무해하고, 스테일 구간은 INVALIDATE 도착으로 종료된다. 실측으로
  구간 길이를 Gate C/E에서 수치화한다.
- 타이머 만료로 오버레이를 자동 제거하는 방식은 기각 — 정지 화면의
  픽셀-퍼펙트 정상 상태를 깨뜨린다.

## 4. 클라이언트 합성 방식 — 결정

**v1: 오버레이 캔버스** (slice-qu-constraints.md open question에 대한 답):

- 렌더러 요소(<video> 또는 canvas) 위에 CSS로 정합된 별도 `<canvas>`를 두고
  (position: absolute, 동일 box, pointer-events: none), `QU_TILE`을
  `createImageBitmap` → `drawImage`, `QU_INVALIDATE`를 `clearRect`로 처리.
- 채택 이유: 기존 비디오 파이프라인(파이프 그래프, WebCodecs/OpenH264/
  MediaSource 전 변형)과 **renderer-agnostic**하게 분리된다. 파이프 통합
  (WebGL 아틀라스 합성)은 canvas 계열 렌더러에서만 가능하고 침습적 —
  성능 필요가 실측되면 후속 최적화로.
- 좌표계: 타일은 스트림 픽셀 좌표. 오버레이 캔버스의 백스토어를 스트림
  해상도로 두고 CSS 스케일은 렌더러와 동일 규칙을 따른다(리사이즈 시 CSS만
  변함 — 재전송 불필요).
- 제약(문서화): 오버레이는 SDR 8-bit 경로다. HDR 스트림에서는 v1 QU를
  비활성(호스트가 HDR 세션에서 subscribe 무시). 4:2:0 비디오 위 4:4:4
  무손실 텍스트라는 조합은 그대로 성립 — 색 텍스트 fringing이 정지 화면에서
  구조적으로 사라진다 (M3 목표와 시너지).

네이티브 티어(M6): 같은 와이어 포맷을 소비해 D3D11 프레젠트 패스에서 타일
아틀라스 텍스처로 합성 (m6-native-spike.md Option-3의 `our_transport` 확장).

## 5. 호스트 측 인터페이스 (Sunshine 포크 ↔ streamer)

QU 타일에는 Moonlight 프로토콜 캐리어가 없다 (slice-qu-constraints.md §5.5:
"no protocol carrier exists"). 대신 **로컬 사이드채널**로 우회한다 — 포크와
streamer는 같은 호스트에서 돈다:

- streamer가 `127.0.0.1` 전용 TCP 리스너(포트는 IPC init로 전달)를 열고,
  Sunshine 포크의 QU 인코더 스레드가 접속해 §2와 동일한 메시지 프레이밍
  (u32 길이 프리픽스 + 메시지)을 흘린다. streamer는 epoch 검증 외 무해석
  릴레이로 `video_qu` DataChannel에 전달한다.
- 이 선택의 근거: moonlight-common 패치 표면 0, 포크 쪽은 소켓 클라이언트
  하나면 된다. 공유메모리/UDS 대비 Windows 이식성 단순.
- 역방향(BUDGET, 클라 부재)도 같은 소켓으로 중계 — 포크의 페이싱 입력.

호스트 인코드 경로 자체(dirty-rect 추적, 안정 감지, PNG 인코드, 페이싱)는
Sunshine 포크 작업(중규모, slice-qu-constraints.md §4)이며 이 문서의 범위 밖.
인터페이스 계약만 여기 고정한다.

## 6. 대역 예산과 CC 상호작용

- QU 바이트는 실제 wire 바이트다 — CC 배선(P2, fec-framing.md §5와 동일
  원칙)에서 서비스 타임에 계상돼야 한다. 단 QU가 의미있게 흐르는 시점은
  정의상 비디오가 ~0인 정지 구간이므로 v1은 정적 예산으로 충분하다.
- v1 예산: `min(QU_BUDGET, target_bitrate의 50%)`, 기본 QU_BUDGET =
  4000 kbps. 모션 재개(=INVALIDATE 발생) 시 즉시 타일 송신 중단 — 잔여
  SCTP 큐만 비운다.
- 적응 예산(손실/RTT 연동)은 Gate B/D 이후 (fec 적응비율과 같은 판정 게이트).

## 7. Phasing

1. **P1 — streamer 채널 + 클라 오버레이 (스트리머 단독, 포크 불필요)**:
   `video_qu` 채널 생성/휴면 게이트, localhost 릴레이 리스너, 클라 오버레이
   매니저 + 메시지 파서 + CRC 검증. 테스트: 합성 타일 인젝터(테스트가
   릴레이 소켓에 PNG 타일을 밀어넣음) → 오버레이 픽셀 검증, epoch/
   invalidate 순서 도메인, budget=0 정지.
2. **P2 — Sunshine 포크 인코드 경로**: dirty-rect 안정 감지 + PNG 타일 +
   페이싱. 검증: 실제 데스크톱 정지 화면 픽셀-퍼펙트(외부 캡처 비교),
   정적 대역 ~0 (ROADMAP M5 검증 기준).
3. **P3 — 네이티브 티어 합성 + 예산 적응 + WebP/zstd 포맷 평가.**

## 8. Open items

1. 안정 판정 N프레임(기본 30)과 타일 크기 128의 트레이드오프는 P2에서
   dirty-rect 실데이터로 튜닝 (DCV는 값 비공개).
2. 커서 처리: 커서가 지나간 타일의 무효화 폭주 방지 — 클라이언트-사이드
   커서 분리(research 05 §2 DCV 항목)와 함께 설계해야 함.
3. HDR 세션의 QU (v1 비활성) — 10-bit 무손실 포맷과 오버레이 경로 필요.
4. 멀티모니터/해상도 변경 시 epoch 전환의 클라 리사이즈 타이밍.
