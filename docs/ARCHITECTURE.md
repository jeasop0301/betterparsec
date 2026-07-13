# betterparsec — 아키텍처

베이스(moonlight-web-stream) 위에 얹는 3개 기능의 설계. 모든 seam은 이 리포의 실제 file:line.

```
[사지방 Chromium]  --TLS 443-->  [공인 VPS: coturn TURN/TLS + relay]  -->  [집 Windows: betterparsec 브리지 + Sunshine]
   무설치·계정로그인               WARP 없이도 도달 (feature #2)              서버측 페어링·NVENC 인코드
        ^                                                                         |
        |----------------- 영상: AV1/HEVC → WebCodecs 디코드 (feature #3) ----------|
                            ↑ REMB/TWCC 혼잡추정 → 런타임 비트레이트 (feature #1)
```

브리지 구조(베이스): `streamer/` = Sunshine(GameStream)에 붙는 Moonlight 클라이언트 + WebRTC/WS 송출. `src/` = actix-web 서버(계정·페어링·API). `web/` = TS 프론트(WebCodecs 디코드).

---

## Feature 1 — 적응형 비트레이트 (flagship, 난이도 상)

### 현재 상태
- REMB와 Receiver Report 손실을 `AbrController`가 bounded target으로 변환한다.
- WebRTC transport가 target을 `Arc<AtomicU32>`로 apply task에 전달한다.
- apply task는 초기 session bitrate를 baseline으로 사용하고, 일반 변경은
  900ms/10% gate를 통과시킨다. 20% 이상 하향은 혼잡 큐 방지를 위해 즉시 보낸다.
- active moonlight-common fork가 encrypted ENet `0x5506` 메시지를 전송한다.
- stock Sunshine은 이 메시지를 지원하지 않는다. `x-ss-general.featureFlags`의
  provisional `DYNAMIC_BITRATE_V1 (0x40)`을 광고한 paired host에서만 송신한다.
- 프로토콜 ACK가 없으므로 성공 상태는 `sent_unacknowledged`이며 `applied`가 아니다.
- 현재 ABR 신호는 bridge→browser WebRTC 구간만 관측한다. 이 설계는 Sunshine과
  bridge가 같은 Windows 호스트에 있다는 상단 배치도를 전제로 한다. 별도 머신이나
  원격 ingress로 분리하면 Moonlight 구간 estimator를 추가하고 두 target의 최솟값을
  사용해야 한다.

### 설계
1. **신호 수집 강화**: REMB만으로 부족 → transport-cc(TWCC) 피드백을 켜서 손실/RTT/도착간격 기반 대역 추정을 얻는다. (webrtc-rs 설정 + SDP에 `transport-cc` 협상)
2. **컨트롤러**: AIMD/GCC류로 target_kbps를 평활 산출. 우선순위 = latency > framerate > quality (Parsec BUD 철학). 급락 시 즉시↓, 회복은 완만히↑. per-role `maximum_bitrate_kbps`(`common/src/lib.rs:29`) 상한 준수.
3. **호스트 적용 확인**: Foundation Sunshine의 기존 `0x5506` handler와
   encoder reconfigure를 사용하되 capability patch를 함께 배포한다. 다음 단계는
   encoder thread 성공 뒤 request-id ACK를 보내 `queued`와 `applied`를 분리하는 것이다.
   stock Sunshine이나 capability 없는 호스트에서는 초기 고정 bitrate를 유지한다.

### 검증
- 집 호스트 스트림 중 `tc netem`으로 대역/지터/손실 인가 → target_kbps 그래프가 따라 내려가고 frame drop이 억제되는지. 동일 조건 Parsec과 뭉개짐 대조.

---

## Feature 2 — WARP 없이 접속 (난이도 중, 훅 재사용)

### 현재 상태 (베이스)
- ICE 서버를 **정적 env**(`src/cli.rs:23`, `WEBRTC_ICE_SERVER_*`) **또는 동적 스크립트**로 주입 가능:
  `streamer/src/dynamic_ice_servers.rs` — `ice_server_script`를 실행해 `Vec<RtcIceServer>` JSON을 읽는다. **시간제한 TURN 크레덴셜 발급에 그대로 쓸 수 있는 훅.**

### 설계
- **공인 VPS에 coturn**: TURN-over-TLS를 **TCP 443**에 리슨(`turns:HOST:443?transport=tcp`). 제약망도 443 아웃바운드는 대개 통과 → **WARP 없이 브라우저가 집 브리지에 릴레이로 도달**.
- **동적 크레덴셜**: `ice_server_script`로 coturn `use-auth-secret`(HMAC, TTL) 크레덴셜을 세션마다 발급 → 정적 비밀 노출 제거.
- **relay 강제**: 클라이언트 `RTCPeerConnection`에 `iceTransportPolicy: 'relay'` 토글 추가(`web/`), 신호/미디어를 전부 443 릴레이로 몰아 "아웃바운드는 TLS 443뿐"을 성립시킴.
- 시그널링(HTTPS/WSS)은 이미 443. TURN도 443 → 단일 포트 프로파일.

### 검증
- 대상 PC에서 **UDP 전면 차단 + 443만 허용** 흡사 환경(방화벽 규칙)으로 접속 성공 여부. WARP off 상태로 붙는 게 성공 기준. 릴레이 RTT/throughput 실측.

### 미검증 리스크
- TCP-릴레이 게임 지연의 실사용성은 **아직 미검증** — M1에서 최우선 측정. DPI가 443 TURN을 막을 가능성도.

---

## Feature 3 — AV1 / 화질 (난이도 중하, 이미 부분 배선)

### 현재 상태 (베이스)
- WebRTC 비디오 전송이 **이미 AV1을 안다**: `streamer/src/transport/webrtc/video.rs:21`(`MIME_TYPE_AV1`), `:28`(`Av1Payloader`). H264/H265는 커스텀 payloader(`video/h264`,`video/h265`), AV1은 crate 기본.
- 코덱 협상: 클라가 `supported_codecs` 선언 → `streamer/src/main.rs:747` (`supported_video_formats`). 색공간은 `:748` Rec709 / `:749` Limited 고정.

### 설계
- **AV1 경로 검증·활성화**: 클라 `getCapabilities`로 AV1 협상, 호스트 Sunshine NVENC AV1(RTX40+) 인코드 확인, 브라우저 WebCodecs AV1 HW 디코드 확인. 안 되면 HEVC/H.264 폴백(이미 있음).
- **HEVC 4:4:4**: 색 텍스트 fringing 제거용. `color_range`/`color_space`와 4:4:4 포맷 협상 경로 확인·노출(Sunshine 무료 4:4:4).
- **rate-control 튜닝**: Sunshine `nvenc_vbv_increase` 등으로 모션 스파이크 뭉개짐 완화(F1과 함께).
- **경로 선택 주의**: WebRTC RTP AV1 vs WebSocket+WebCodecs AV1 — 후자가 브라우저 AV1 디코드에 더 단순할 수 있음. 둘 다 측정 후 결정.

### 검증
- 동일 delivered bitrate에서 AV1 4:2:0 vs HEVC 4:2:0 vs HEVC 4:4:4를 정지/모션/색텍스트로 눈+지표 채점. 브라우저 `chrome://gpu`로 HW 디코드 실동작 확인.

---

## 보안 (인증 구현 시 non-negotiable)
계정/토큰 코드는 베이스+우리 추가분 모두 감사 대상. 구현 시 강제:
1. 토큰의 보안 관련 필드(만료·scope·role·user) **전부 서명**.
2. **짧은 수명 access + refresh**, 공용 PC엔 refresh 미저장.
3. 토큰/PIN/비밀 비교는 **상수시간**(`==` 금지).
4. **replay 방지**: nonce/jti + 만료 + 세션 바인딩, 시그널링 메시지도 인증.
5. 안전 인코딩(JSON/base64url), 구분자 순진 연결 금지.
6. TLS 전구간(secure-context), 쿠키 HttpOnly+Secure+SameSite, least-privilege scope, 서버측 revocation(unpair).
