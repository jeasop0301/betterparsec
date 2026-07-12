# 리서치 3 — 실제로 만들 수 있는 설계 검증

> Sunshine 품질 + Parsec식 영속 계정 인증 + WARP-only 도달의 실현가능성. 재페어링 제거·연결성·적응성.

_다중 에이전트 리서치 + 적대적 검증. run: `wf_b513cad8-747`. 2026-07._

---

# betterparsec 최종 빌드 설계 메모

작성: principal engineer / 근거: 4개 분석 findings + 적대적 검증 verdicts. 결론과 추론(inference)을 구분하고, 검증 태그를 절대 과장하지 않는다. WARP-only 전송은 과제가 고정한 제약으로 받아들이되, 그 제약이 만드는 구체적 실패면을 명시한다.

---

## 0. 한 줄 결론

"한 번 로그인, 재-pairing 없음, WARP-only에서 볼 만한 품질"을 **가장 적은 미검증 의존성으로** 만족하는 스택은 **네이티브 Moonlight + 인증서 복원**이 아니라 **브라우저 WebRTC/WebCodecs + 서버측 계정 인증**이다(대표 베이스: `moonlight-web-stream`, 대안 all-in-one: Vibeshine/Vibepollo). 전송은 **공인 VPS의 self-hosted coturn TURN-over-TLS/TCP-443 + relay-only ICE**로 WARP의 기본 full-tunnel egress에 얹는다. 다만 **품질 적응성(adaptivity)과 TCP-릴레이 지연은 이 스택의 구조적 약점**이며, 바로 그 세 가지(계정 인증·4:4:4·적응형 비트레이트)를 Parsec Warp가 이미 월 $8.33~9.99에 제공한다는 점이 build-vs-buy의 핵심이다.

---

## 1. Pillar별 실현가능성 판정

### 1-A. Auth — 재-pairing 제거

**판정: [CONFIRMED] (단, Sunshine/Apollo의 계정·토큰 기능으로가 아니라, 클라이언트 아이덴티티 지속화 또는 서버측 계정 인증으로만 가능)**

- **정확한 메커니즘 = 인증서 지속/복원(Sunshine 본체 기능 아님).** moonlight-qt는 클라이언트 아이덴티티(cert/key/uniqueid)와 페어링된 호스트의 pinned server cert를 단일 QSettings 스토어에 저장한다. Windows에서는 레지스트리 `HKCU\Software\Moonlight Game Streaming Project\Moonlight`, `portable.dat`가 exe 옆에 있으면 portable INI. 이 스토어를 그대로 복원하면 mutual-TLS로 PIN 없이 재연결된다 — **[CONFIRMED]** (검증1: identitymanager.cpp가 cert/key/uniqueid를 평문 PEM으로 저장, 없을 때만 재생성 / computermanager.cpp가 `srvcert` pin 저장 / Sunshine #1353가 pin 방향 확증).
  - 출처: https://github.com/moonlight-stream/moonlight-qt/blob/master/app/backend/identitymanager.cpp , https://github.com/moonlight-stream/moonlight-qt/blob/master/app/backend/computermanager.cpp , https://github.com/moonlight-stream/moonlight-qt/blob/master/app/main.cpp , https://github.com/LizardByte/Sunshine/issues/1353
  - **전제(둘 다 충족되어야 함):** 호스트가 해당 client cert를 계속 authorize하고, 호스트 server cert를 로테이트하지 않아야 한다. Sunshine state 재생성 시 새 uniqueid로 전 클라이언트 무효화(#2305 / PR #2365). 즉 홈 PC의 `sunshine_state.json`을 안정적으로 유지해야 함.
    - 출처: https://github.com/LizardByte/Sunshine/issues/2305
  - **보안 주의:** 이 방식은 공용 PC에 client private key를 세션 내내 노출한다(평문·머신 바인딩 없음). 전용 throwaway 아이덴티티 + Apollo 퍼-디바이스 권한 축소 + 필요 시 unpair 취소가 완화책. 자동 세션별 비밀 로테이션은 없음(재사용 cert가 목적이므로).

- **portable 패키지 주장 = [DISPUTED].** portable.dat → INI 메커니즘 자체는 소스로 확인되나, "official Chocolatey `moonlight-qt.portable`"는 **거짓**이다. 커뮤니티 패키저 chtof의 비공식 패키지이며, moonlight-stream 공식 배포는 GitHub Releases의 portable ZIP이다. 또한 INI는 exe 바로 옆이 아니라 CWD 하위 `Moonlight Game Streaming Project/Moonlight.ini` 서브폴더에 생성된다(사소한 정정).
  - 출처: https://community.chocolatey.org/packages/moonlight-qt.portable , https://github.com/chtof/chocolatey-packages/tree/master/automatic/moonlight-qt.portable , https://github.com/moonlight-stream/moonlight-qt/issues/1790

- **Sunshine/Apollo의 계정·토큰 인증 부재 = [CONFIRMED].** PIN-pairing을 대체하는 account/token auth는 어느 릴리스에도 없다. 유일한 토큰 제안 moonlight-qt #1576은 open·comment 0·구현 PR 없음(2025-04-12). Apollo의 OTP·권한 매니저도 여전히 퍼-디바이스 페어링이지 재사용 자격증명이 아니다. `/api/pin`은 PIN "입력"만 자동화할 뿐 wiped client의 재-pairing을 없애지 못한다(#4490).
  - 출처: https://github.com/moonlight-stream/moonlight-qt/issues/1576 , https://deepwiki.com/ClassicOldSong/Apollo/8.1-gamestream-protocol-and-pairing , https://docs.lizardbyte.dev/projects/sunshine/latest/md_docs_2api.html , https://github.com/LizardByte/Sunshine/issues/4490

- **브라우저 경로의 진짜 "계정 인증, 재-pair 없음" = [CONFIRMED] × 2:**
  - **Vibeshine/Vibepollo:** 모든 WebRTC 엔드포인트가 `authenticate()`로 게이팅(HttpOnly 세션 쿠키 또는 API 토큰). 브라우저는 Moonlight mTLS/PIN 페어링을 애초에 할 수 없으므로, 상태 wiped 공용 PC는 웹UI 재로그인만 하면 됨. 단 이 경로는 **explicitly experimental**, 코드 ~99% AI 생성.
    - 출처: https://github.com/Nonary/vibeshine/blob/vibe/architecture.md
  - **moonlight-web-stream:** 호스트 페어링을 서버측 JSON(`StorageHostPairInfo`: client cert/key + server cert)에 저장하고 재기동 시 재로딩(`host.set_identity(...)`) → 브라우저는 로그인만, 재-pair 절대 없음. **[CONFIRMED]** (검증6, master/v2.8 소스). HEVC/AV1도 코드에 실제 등록됨(아래 품질 참조).
    - 출처: https://github.com/MrCreativ3001/moonlight-web-stream/blob/master/src/app/storage/mod.rs , https://github.com/MrCreativ3001/moonlight-web-stream/blob/master/src/app/host.rs

**커스텀 빌드 필요분(Auth):** 계정 인증 자체는 두 브라우저 포크가 이미 제공하므로 "새로 만들" 것은 없다. 커스텀 작업은 (1) 포크 인증 코드의 보안 감사·강화(3절), (2) 네이티브-cert 경로를 쓸 경우 세션별 store 복원/주입 자동화 스크립트뿐이다.

---

### 1-B. Connectivity — WARP-only에서 홈 Sunshine 도달

**판정: 가장 견고한 단순 경로 = [CONTEXT-DEPENDENT] / WARP-to-Tunnel UDP 경로 = [DISPUTED]**

- **가장 단순하고 "확실히 WARP를 탄다"는 경로(명명): self-hosted coturn TURN-over-TLS/TCP-443(공인 VPS) + 브라우저 WebRTC 클라이언트, ICE를 `iceTransportPolicy:'relay'` + `turns:…:443?transport=tcp`로 강제.** 이유: consumer WARP는 기본 0.0.0.0/0 full-tunnel이라 공인 호스트로의 임의 outbound TCP-443을 투명하게 egress한다. 클라이언트의 미디어 흐름을 전부 TCP-443 릴레이로 몰면 WARP가 그대로 실어나른다. **[CONTEXT-DEPENDENT]** — 부품은 전부 오늘 존재하나(coturn TLS/443, relay-only ICE, WARP full-tunnel, 두 포크 모두 커스텀 ICE 설정 가능), 결정적 미검증분이 있다:
  - 두 포크 기본값은 **UDP-first**이고 relay-only 토글을 문서화하지 않는다 → **패치/설정으로 강제하지 않으면 "outbound가 TCP-443뿐"이 성립하지 않는다.** 또한 signaling(HTTPS REST+SSE / WebSocket) + DNS가 별도 흐름으로 항상 존재(대개 TCP-443이라 WARP-egress 가능하나 "단일 흐름"은 아님).
  - **must-test:** ① 대상 네트워크에서 WARP가 실제로 임의 공인 TCP-443을 egress하는가, ② 포크에서 relay-only ICE가 실제 강제되는가, ③ **Cloudflare edge + VPS 릴레이 경유 게임-지연이 실사용 가능한가(미검증)**, ④ 공용 PC가 외부 443 호스트 도달을 애초에 허용하는가.
  - 출처: https://www.metered.ca/blog/coturn/ , https://github.com/coturn/coturn/issues/1626 , https://blog.expressturn.com/debugging-ice-failed-webrtc , https://developers.cloudflare.com/warp-client/warp-modes/ , https://github.com/Nonary/vibeshine/blob/vibe/architecture.md , https://github.com/MrCreativ3001/moonlight-web-stream

- **더 "그럴듯한" WARP-to-Tunnel private-network UDP 경로 = [DISPUTED], 주 경로로 의존 금지.** 2025-07-15 Cloudflare changelog는 **GA 발표가 아니라 신뢰성 재설계**(UDP를 heavy TCP와 격리)이며, UDP private-network 자체는 2021-12부터 존재. changelog가 든 real-time 예시는 **DNS**이지 고대역 영상이 아니다. 게임-지연 사용가능성은 미검증이며 반증 다수: 공개 tunnel은 public UDP 미지원(cloudflared #964), 네이티브 Moonlight가 tunnel에서 깨짐(moonlight-android #1522), cloudflared는 서버-개시 연결 불가(Mesh로 유도), Moonlight FAQ는 tunnel이 아니라 ZeroTier 권장.
  - 출처: https://developers.cloudflare.com/changelog/post/2025-07-15-udp-improvements/ , https://github.com/cloudflare/cloudflared/issues/964 , https://github.com/moonlight-stream/moonlight-android/issues/1522 , https://github.com/moonlight-stream/moonlight-docs/wiki/Frequently-Asked-Questions

- **"WARP를 내 Zero Trust org로 전환" 전제 = [DISPUTED].** 대상 공용 PC가 WARP를 쓴다는 근거 자체가 불명이고(사지방은 별개의 잠금 규제망), 무엇보다 "mandatory(관리형) WARP"는 통상 `switch_locked` / "Lock device client switch" / org-leave 차단으로 **정확히 이 전환을 막는다**. 즉 WARP-to-Tunnel private-net을 아키텍처 근간으로 삼으면 안 된다. 대신 **WARP 기본 full-tunnel이 공인 TCP-443을 egress한다**는 훨씬 약한 가정에만 의존하라.
  - 출처: https://developers.cloudflare.com/cloudflare-one/connections/connect-devices/warp/deployment/mdm-deployment/switch-organizations/ , https://developers.cloudflare.com/cloudflare-one/team-and-resources/devices/cloudflare-one-client/configure/settings/

- **Tailscale/ZeroTier 병행 = 이 환경에서 최약체.** 두 시스템 WireGuard 충돌, 표준 해법(Zero Trust 대시보드에서 Tailscale를 WARP에서 exclude + 비-WARP egress)이 WARP가 유일 egress이면 불가 → nested-WireGuard의 취약 구성. WARP 제약이 없다면 Tailscale이 정답이라는 역설.
  - 출처: https://github.com/tailscale/tailscale/issues/5631 , https://tailscale.com/kb/1105/other-vpns

---

### 1-C. Quality — Sunshine가 Parsec를 WARP에서 이길/맞출 수 있는가

**판정: 코덱 품질-per-bit = Sunshine 우위 [CONTEXT-DEPENDENT] / 적응성 없이는 재현 불가 = [CONFIRMED negative]**

- **코덱 팩트(연구 findings, 적대검증 대상 아님·high conf):** Parsec 무료 = 8-bit 4:2:0 H.264/H.265, ~50 Mbit/s cap; 10-bit 4:4:4는 Warp($9.99/mo) 유료. Sunshine = 무료 AV1 + 무료 HEVC 4:4:4 + 10-bit, ~150 Mbit/s. AV1의 per-bit 우위(HEVC 대비 ~20-30%, H.264 대비 ~2×)는 **WARP가 주는 저비트레이트 구간에서 가장 큼**. → 동일 전달 비트레이트라면 Sunshine 그림이 per-bit 우위.
  - 출처: https://github.com/orgs/LizardByte/discussions/220 , https://parsec.app/warp , https://parsec.app/blog/an-introduction-to-video-compression-c5061a5d075e

- **그러나 결정적 부정 = [CONFIRMED]: Sunshine/Moonlight 계열 어느 프로젝트도 GCC/rtpgccbwe 대역폭 추정을 인코더 비트레이트에 커플링하지 않는다.** WebRTC 포크(Vibeshine, LuminalShine, moonlight-web-stream)는 WebRTC를 브라우저 "전송"으로만 쓰고 인코더 비트레이트는 고정. GitHub 전역 `rtpgccbwe` 검색에 Moonlight 계열 0건, 프로토콜이 TWCC 피드백을 안 나른다. (Vibeshine이 최근 client-driven 런타임 비트레이트 엔드포인트=ABR을 추가했으나 이는 클라이언트 요청이지 GCC 커플링이 아님.)
  - 출처: https://github.com/Nonary/vibeshine/blob/vibe/architecture.md , https://ideas.moonlight-stream.org/posts/217/adaptive-bitrate-congestion-control , https://gstreamer.freedesktop.org/documentation/rsrtp/rtpgccbwe.html

- **jittery WARP에서의 구체적 실패 메커니즘:** 고정-크기 프레임이 spike 때 스로틀된 파이프를 넘침 → Reed-Solomon FEC(기본 `fec_percentage` 20) 소진 → 손실 slice가 inter-frame 오염 → 복구 프레임(RFI, 최악 IDR)이 다시 링크를 혼잡시켜 자기지속. Parsec BUD는 1~2프레임 내 비트레이트를 낮춰 "깨끗하지만 흐릿"하게 유지. 게다가 RFI는 AV1/고fps/장거리(=WARP)에서 가장 약해 IDR로 폴백(common-c #120).
  - 출처: https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007 , https://docs.lizardbyte.dev/projects/sunshine/latest/md_docs_2configuration.html , https://github.com/moonlight-stream/moonlight-common-c/issues/120

- **오픈스택 완화책은 정적 보험(적응 아님):** 보수적 고정 비트레이트(WARP 스로틀 하한 아래)가 최강 레버, `fec_percentage` 30-40(단 비트레이트를 부풀려 혼잡 악화 가능), RFI(고립 손실엔 유효·지속 버스트엔 무용). `minimum_fps_target`은 정적 콘텐츠 대역폭 하한이지 혼잡 레버가 아님(오해 정정, PR #4114).
  - 출처: https://github.com/LizardByte/Sunshine/pull/4114

- **브라우저 HW HEVC/AV1 디코드 = [CONTEXT-DEPENDENT].** HEVC WebRTC 디코드는 Chrome 136+(2025-05)·HW HEVC 디코더·HW accel ON 전부 필요(Chrome엔 소프트웨어 HEVC 디코더 없음). AV1 HW 디코드는 2020+ GPU + Windows AV1 Video Extension 필요. **잠금된 공용 PC(구버전/HW accel off/GPU 없는 thin client)에서는 소프트웨어 H.264로 폴백** — Vibeshine의 문서화된 기본 폴백 그대로.
  - 출처: https://github.com/StaZhu/enable-chromium-hevc-hardware-decoding/blob/main/README.md , https://github.com/bluenviron/mediamtx/discussions/4396

**품질 종합:** 적응성을 동일화하면 Sunshine가 per-bit 근소 우위·급격모션 복구는 par. 그러나 **실 Windows+Sunshine 스택에서 적응성을 동일화할 수단이 오늘 없다**(GCC 미커플링). 진짜 GCC 스택(Selkies/webrtcsink)은 Linux 호스트라 Windows 게임 GPU를 못 쏜다. → **실무적 오픈스택 플레이 = 보수적 고정 비트레이트 + AV1 + 완만한 FEC.** WARP jitter 하에서 "깨끗함 유지"는 여전히 Parsec 우위.

---

## 2. 권장 아키텍처 (단일 스택 확정)

**선택: 브라우저 WebRTC/WebCodecs + 서버측 계정 인증** (세 후보 중 `browser-WebCodecs-with-account-auth`).

**왜 이것인가 (탈락 근거 포함):**
- `native-Moonlight-with-persistent-cert`: 품질 천장은 최고지만 **연결성이 WARP-only의 약점 그 자체**(UDP). 공개 tunnel에서 깨지고, WARP-to-Tunnel UDP는 DISPUTED, Zero Trust 전환은 MDM-lock 가능. 또 공용 PC에 private key 노출 + 세션별 store 복원 필요 + 잠금 PC가 네이티브 설치를 막으면 불가. → **품질 상한 확보용 옵션 경로로 강등**(Phase 4).
- `WebRTC-GCC path`: Sunshine-급 Windows 호스트에서 **존재하지 않음**(CONFIRMED). rtpgccbwe→NVENC를 직접 배선하는 무거운 커스텀이거나, Windows 게임을 못 쏘는 Linux Selkies 스택 → **off-target**.
- `browser-WebCodecs-with-account-auth`: **모든 제약을 최소 미검증분으로 만족** — 재-pair 없음(CONFIRMED, 서버측 계정), zero-install(잠금 PC 적합), WARP-only에서 TCP-443 릴레이로 확실히 egress. 품질-적응성이 tradeoff이나 이는 정직히 감수·완화한다.

**컴포넌트 리스트:**
- **Host(홈 Windows 게이밍 PC):** Sunshine(또는 Apollo). 안정적 `sunshine_state.json` 유지, server cert 로테이트 금지.
- **Web bridge / streamer:** **1순위 `moonlight-web-stream`**(성숙·v2.8·Docker, 서버측 페어링 지속 CONFIRMED, HEVC/AV1 코드 등록 CONFIRMED, WebCodecs VideoDecoder 사용). **대안 all-in-one: Vibeshine/Vibepollo**(계정 인증 CONFIRMED이나 WebRTC가 experimental·AI 생성).
- **Transport-over-WARP:** 공인 VPS의 **coturn, TURN-over-TLS on TCP-443**(`turns:host:443?transport=tcp`, setcap/SNI 처리) + 클라이언트 **`iceTransportPolicy:'relay'` 강제**. signaling은 이미 HTTPS/443. WARP 기본 full-tunnel이 이 443 흐름을 egress.
- **Client:** 대상 공용 PC의 **plain Chromium 브라우저**(설치 불필요, secure-context 필수 — WebCodecs/Gamepad/Keyboard-Lock이 HTTPS 요구).
- **Auth:** web bridge의 계정/세션(로그인 1회 → 서명 토큰/HttpOnly 쿠키). 호스트-Sunshine 페어링은 서버측에 1회 저장, 브라우저는 Moonlight cert를 만지지 않음.

---

## 3. Auth 설계 (재-pair 제거가 코어) — 구현 제약으로 못박음

**지속 계정/토큰 메커니즘:**
- 홈/VPS 서버가 사용자 계정을 보유. 최초 1회: 관리자/소유자가 Sunshine 호스트를 서버측에서 PIN 페어링(cert triple을 서버 스토어에 저장, 이후 재사용). 브라우저 클라이언트는 **로그인만** 하고 Moonlight 페어링을 하지 않음 → wiped 공용 PC는 재로그인=끝.
- 공용 PC 특성상 **장기 refresh 토큰을 그 PC에 저장하지 말 것.** 세션마다 재로그인(수용 가능)이 더 안전. 토큰은 서버측(VPS/홈)에 머무는 아이덴티티에만 결부.

**바로 이 세 후보 포크는 인증 코드가 부분적으로 AI 생성/experimental이므로, 다음을 구현 제약(non-negotiable)으로 감사·강화한다:**
1. **모든 보안 관련 필드 서명:** 베어러 토큰의 user id·만료·scope·role 등 전 필드를 서명(HMAC/JWT류) 아래 둔다. 서명 안 된 필드를 토큰/시그널링에 넣지 않는다.
2. **짧은 수명 access token + refresh:** access TTL 분 단위, refresh로 재발급, refresh 회전. 공용 PC엔 refresh 미저장 원칙.
3. **constant-time 비교:** 세션 토큰·PIN·비밀 비교를 상수시간 등가비교로(타이밍 사이드채널 차단). 서버측 1회 PIN 페어링 단계와 토큰 검증 모두 적용.
4. **replay 방지:** nonce/jti + 만료 + 세션 바인딩, refresh 시 회전. **SDP offer/ICE 시그널링 메시지 자체를 인증·비재생 가능하게** (시그널링은 "그저 인증된 API surface"라는 Vibeshine 설계 원칙을 유지·검증).
5. **안전한 delimiter/encoding:** 토큰/시그널링 payload에 필드를 담을 때 JSON/base64url·길이접두 등 **모호성 없는 인코딩**만 사용. 필드에 나타날 수 있는 구분자로 순진하게 문자열 연결 금지(필드-split/injection 차단 — GameStream 페어링류의 고전적 버그면).
6. **부대 제약:** TLS 전구간(WebCodecs/Gamepad/Keyboard-Lock이 secure-context 강제), 쿠키 HttpOnly+Secure+SameSite, 토큰 least-privilege scope(Apollo류 퍼-디바이스 권한 활용 가능), **전용 아이덴티티 + 서버측 revocation(unpair)**. private key가 공용 PC가 아니라 서버측에 있으므로 네이티브-cert-복원 대비 노출이 근본적으로 작음.
  - 근거·출처: https://github.com/Nonary/vibeshine/blob/vibe/architecture.md , https://github.com/MrCreativ3001/moonlight-web-stream/blob/master/src/app/storage/mod.rs

---

## 4. 단계별 계획 (MVP 먼저 → 하드닝)

- **Phase 0 — 사전 de-risk (약 1-2일).** 아무것도 짓기 전에: ① 대상 WARP-only 네트워크에서 **Parsec가 애초에 연결되는지** 실측(namu.wiki가 1.1.1.1/VPN 및 IDC egress 제한 경고 — build-vs-buy를 좌우). ② 대상 PC에서 **공인 VPS로 outbound TCP-443이 뚫리는지**(curl/iperf) 실측. ③ 대상 브라우저 Chrome 버전·secure-context·chrome://gpu 코덱 확인.
  - 출처: https://namu.wiki/w/Parsec
- **Phase 1 — MVP (약 1주). 증명 목표: "1회 로그인, 재-pair 없음, WARP에서 볼 만함".** 홈 Sunshine 기동 → `moonlight-web-stream` 계정 1개·호스트 1회 서버측 페어링 → 공인 VPS에 HTTPS ingress + coturn TLS/443 + relay-only ICE 강제 → 대상 공용 PC 브라우저에서 WARP 경유 접속. **검증:** (a) 신규 로그인 시 PIN 없이 연결, (b) 브라우저 상태 wipe 후 재로그인만으로 재-pair 없이 연결, (c) 실게임 1판의 비트레이트/지연/frame drop 실측.
- **Phase 2 — 품질 하드닝 (약 1주).** HEVC/AV1 네고 활성화 후 대상 브라우저/GPU에서 HW 디코드 실제 확인(안 되면 H.264 수용), **WARP 하한 아래 보수적 고정 비트레이트** + 완만 FEC, HW 허용 시 4:4:4/10-bit 확인.
- **Phase 3 — 보안 하드닝 (약 1-2주).** 3절 6개 제약을 코드 감사·구현(서명·짧은토큰+refresh·constant-time·replay·안전인코딩·HttpOnly/Secure·least-privilege·revocation). **Vibeshine 채택 시 ~99% AI 생성 인증 코드 정밀 감사 필수.**
- **Phase 4 — (선택) 품질 천장 (규모 큼/불확실).** 사용가능 UDP 전송(WARP-to-Tunnel private-net)이 실측으로 확인되면 네이티브-Moonlight-persistent-cert로 지연 개선; 또는 rtpgccbwe→NVENC 적응성 자체 배선(무겁고 오늘 존재하지 않음).

---

## 5. Top Risks (순위) + de-risk 테스트

1. **WARP-only 연결성이 실사용 지연에 도달 못함(최상위).** 테스트: Phase 0 — WARP 경유 VPS로 TCP-443 iperf/RTT/jitter → 릴레이 실미디어 세션 RTT·throughput·체감. (근거: cloudflared #964, moonlight-android #1522)
2. **TCP-443 릴레이 + 적응성 부재로 jittery WARP에서 화면 뭉개짐.** 테스트: 부하 하 실게임 세션의 frame drop 측정, 보수적 고정 비트레이트+AV1 적용 후 재측정, 동일 링크에서 Parsec와 대조. (근거: ideas #217, common-c #120)
3. **잠금 브라우저에서 HW HEVC/AV1 디코드 불가 → 소프트웨어 H.264.** 테스트: 실제 대상 PC의 chrome://gpu·Chrome≥136·실사용 코덱 확인; 폴백안=H.264. (근거: mediamtx #4396, StaZhu README)
4. **공용 PC 잠금이 브라우저/secure-context/외부 443/Gamepad·Keyboard-Lock를 차단.** 테스트: 실제 기기에서 Chromium 버전·secure-context·외부 443 도달·입력 API 동작 확인.
5. **AI 생성 포크 인증 코드의 취약점.** 테스트: `authenticate()`/토큰 코드 보안 감사 + replay/변조/타이밍 침투 테스트, HttpOnly/Secure 확인. (근거: architecture.md)
6. **Parsec가 이 네트워크에서 되느냐/막히느냐(build-vs-buy 좌우).** 테스트: 대상 PC에 Parsec 설치·WARP 경유 접속 시도·결과 기록. (근거: namu.wiki/w/Parsec)

---

## 6. 정직한 최종 판단 — DIY vs Parsec Warp($8.33-9.99/mo)

**핵심 사실:** Parsec Warp는 **이미** 계정 인증(1회 로그인·재-pair 없음) + 10-bit 4:4:4 + BUD 적응형 비트레이트를 제공한다. 이 세 가지는 정확히 DIY 스택이 가장 고전하는 지점이다 — 특히 **jittery WARP에서 "깨끗함 유지"의 적응성은 오늘 Sunshine 계열에 존재하지 않는다(CONFIRMED)**. 즉 "그냥 되게 하기"가 목적이면 냉정히 Parsec-paid가 우위다.
- 출처: https://parsec.app/warp , https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007

**Parsec-paid가 이기는 경우(대다수 사용자):** 최소 노력으로 동작·jitter 복원력이 필요하고, 월 구독을 감내하며, 대상 네트워크에서 Parsec가 실제로 연결될 때.

**DIY build가 이기는 경우(명확한 조건에서만):**
1. **Parsec가 그 네트워크에서 막히지만 자가호스트 443 엔드포인트는 뚫릴 때**(namu.wiki의 1.1.1.1/IDC egress 제한). 이건 Phase 0에서 반드시 먼저 확인 — DIY 정당화의 1차 근거.
2. **무료 4:4:4/10-bit/AV1을 원하고** 보수적 고정 비트레이트로 운용 가능할 때(정적/데스크톱/텍스트 선명도, per-bit 우위).
3. **반복 비용 회피 + 자가호스트 소유권/제어**를 값지게 볼 때, 그리고 적응성 부재로 인한 "spike 시 뭉개짐"을 감수할 수 있을 때.

**한 문장 권고:** Phase 0에서 **대상 네트워크의 Parsec 연결 여부**를 먼저 확인하라. Parsec가 잘 되면 대부분의 목적에는 **Parsec Warp 구독이 합리적**이다. Parsec가 막히거나, 무료 4:4:4/자가호스트 소유권이 요구되거나, 구독 회피가 목표라면 — 그때 위 브라우저-WebCodecs-계정인증 스택이 정당한 DIY 승리다. (연결성 자체가 이 프로젝트의 진짜 병목이지, 인증 제거는 이미 해결된 문제임을 유념.)


---

## 적대적 검증 판정 (10건)


**[CONFIRMED]** Vibeshine/Vibepollo's experimental WebRTC browser client authenticates solely via the Sunshine web-UI account session (HttpOnly cookie or API token) with NO Moonlight PIN pairing for the browser, so a state-wiped public PC only needs to re-log into the web UI — never re-pair.


> 정정/정밀화: Confirmed by the project's own architecture doc. In Vibeshine (the rebranded Sunshine host powering the Vibepollo fork), the experimental WebRTC browser-streaming path is served from the same HTTPS Web UI, and every WebRTC signaling endpoint (`/api/webrtc/sessions`, `/offer`, `/ice`, ...) is gated only by the server's `authenticate()` function, which accepts either an `Authorization:` API token or an HttpOnly web-UI session cookie (`extract_session_token_from_cookie`). The docs describe it as "just another authenticated API surface," with no Moonlight PIN-pairing step for the browser — indeed a browser cannot perform Moonlight's mTLS/PIN certificate pairing at all. Consequently, a client machine (e.g., a public PC) whose local state/cookies are wiped only needs to re-log into the Web UI (or present a stored scoped API token) to stream again; it never had a browser PIN pairing to redo. Caveats: this is explicitly labeled experimental (not the default), the docs state the sole auth gate is the session/token rather than literally saying "no PIN," and this applies only to the browser/WebRTC path — the coexisting native Moonlight path still uses traditional PIN pairing.


**[CONFIRMED]** No shipping Sunshine or Apollo feature provides account- or token-based authentication in place of per-device PIN pairing; the only token proposal, moonlight-qt issue #1576, is open with zero comments and no implementing PR.


> 정정/정밀화: Correct as stated, with two clarifications. As of July 2026, neither Sunshine (LizardByte) nor its Apollo fork (ClassicOldSong) ships any account- or token-based client authentication that replaces per-device PIN pairing: Sunshine uses PIN pairing (its admin username/password gates only the config web UI), and Apollo adds a host-initiated OTP method and a per-client permission manager that are still per-device pairing, not reusable account/token credentials. moonlight-qt issue #1576 ("Add export command and --token parameter for reusable host authentication") is open, has 0 comments (opened 2025-04-12), and has no linked/implementing PR — but note it is a moonlight-qt CLIENT-side proposal to export existing pairing credentials into a portable token file, not a host feature; and "the only token proposal" is best read as "the only concrete filed token proposal," since related token/per-client-secret ideas also appear informally in unimplemented Sunshine discussions.


**[DISPUTED]** moonlight-qt supports a portable mode triggered by a portable.dat file in the app directory that writes all settings including the client identity into an INI next to the executable, and an official portable package (Chocolatey moonlight-qt.portable) exists.


> 정정/정밀화: moonlight-qt does support a portable mode: if a `portable.dat` file exists in the process's current working directory (normally the app folder), app/main.cpp switches QSettings to IniFormat and redirects both user- and system-scope paths to that directory, so all settings — including the client identity (X.509 certificate, private key, and unique ID written by IdentityManager) — are stored in a Moonlight INI file under that directory (specifically a "Moonlight Game Streaming Project/Moonlight.ini" subfolder) instead of the Windows registry. However, the Chocolatey `moonlight-qt.portable` package (latest 6.1.0), while it does exist, is NOT official: it is community-maintained (by packager "chtof") in the Chocolatey Community Repository, which is explicitly community-provided and moderated, not published by the moonlight-stream project. moonlight-stream itself provides an official portable ZIP build on its GitHub Releases page, but does not maintain the Chocolatey package.


**[DISPUTED]** UDP over a Cloudflare Tunnel WARP-to-private-network path is generally available as of 15 July 2025 and now reliably carries real-time UDP, so a WARP-enrolled Zero Trust client can reach a home Sunshine host's LAN IP over Moonlight's UDP ports at usable game-streaming latency.


> 정정/정밀화: Cloudflare published a changelog on 15 July 2025 ("Faster, more reliable UDP traffic for Cloudflare Tunnel") that re-architected how cloudflared proxies UDP — isolating UDP from heavy TCP and cutting new-UDP-session setup latency — delivered automatically to all customers. This was a reliability improvement, not a general-availability announcement (UDP over WARP-to-private-network has existed since early access in December 2021), and its cited beneficiary is private DNS, not high-bandwidth real-time video. Cloudflare makes no claim that this enables game streaming, and it is not a supported/usable-latency path for Moonlight-to-Sunshine: WARP-to-Tunnel hairpins traffic through Cloudflare's edge (not a direct/P2P route), cloudflared cannot handle server-initiated connections back to the client (Cloudflare steers such bidirectional/real-time needs to its Mesh product), and users report Moonlight/Sunshine performing poorly or failing over Cloudflare Tunnel — Moonlight's own docs instead recommend a peer-to-peer VPN such as ZeroTier. Reaching a LAN IP over UDP ports via WARP is architecturally possible, but "usable game-streaming latency" is unproven and contradicted by available evidence.


**[DISPUTED]** The mandatory 사지방 Cloudflare WARP can be switched from consumer 1.1.1.1 mode to the user's own Zero Trust team login each session while still providing general internet egress AND routing the home private CIDR, without MDM lock-out.


> 정정/정밀화: There is no evidence that 사지방 (사이버지식정보방) uses Cloudflare WARP at all — 사지방 PCs run a separate, locked-down Korean filtering/security regime (network-isolated, no user installs, no disk writes, usage logged, filter-circumvention explicitly prohibited), so the "mandatory 사지방 Cloudflare WARP" premise is unfounded. Separately, the Cloudflare primitives are individually real: the consumer WARP client can log into a Zero Trust org via team name, Zero Trust WARP gives general internet egress by default, and split tunnels can route a private CIDR (default excludes RFC1918, so you must add an Include entry and run a cloudflared/WARP-Connector return path at home). But a truly MANDATORY WARP deployment is normally MDM-managed with switch_locked / "Lock device client switch" and org-leave disabled, which specifically blocks a user from switching to their own team each session. The described free per-session switch is possible only on an unmanaged, unlocked install — which contradicts "mandatory" — and corresponds to no documented 사지방 configuration.


**[CONFIRMED]** MrCreativ3001/moonlight-web-stream persists the Sunshine host pairing server-side across restarts (so browser clients never re-pair) and whether it can negotiate HEVC/AV1 beyond the documented H.264/openh264 path.


> 정정/정밀화: Confirmed on both counts. (1) moonlight-web-stream keeps the Sunshine pairing server-side: the client cert, private key, and server cert are stored per-host in its JSON data store (`StorageHostPairInfo`) and reloaded on startup, so the server re-uses the identity (`host.set_identity(...)`) after restarts and browser clients — which authenticate via login, not Moonlight pairing — never (re-)pair with Sunshine. (2) It can negotiate more than H.264: the WebRTC streamer registers HEVC (Main/Main10/RExt 4:4:4 8- and 10-bit) and AV1 (Main8/10, High 4:4:4) codecs with real payloaders, the web client detects and can select h264/h265/av1/auto, and role permissions default-allow all three. openh264 is merely a software H.264-only decoder fallback for the WebSocket transport in non-HTTPS contexts. Caveats: HEVC/AV1 are implemented and shipping (master, ~v2.8) but not documented in the README, and successful playback depends on the viewer's browser decoder support.


**[CONTEXT_DEPENDENT]** Vibeshine's WebRTC path can actually deliver hardware-accelerated HEVC/AV1 decode (Sunshine-grade quality) in a plain locked-down Chromium browser, rather than degrading to software H.264, given the target public-PC browser/GPU.


> 정정/정밀화: It depends on the specific target PC. Vibeshine's WebRTC path can request HEVC or AV1, but the browser negotiates by capability and (per Vibeshine's own architecture doc) falls back to H.264 when the higher codec isn't available. Hardware-accelerated HEVC decode over browser WebRTC is real but requires Chrome 136+ (May 2025), a GPU with HEVC hardware decode, AND hardware acceleration enabled — Chrome has no software HEVC decoder at all. On a locked-down public PC where hardware acceleration is disabled, Chrome is an older/managed build, or the machine is a GPU-less thin client/VDI, HEVC is unavailable and it will degrade to software H.264 exactly as the claim fears. Hardware-accelerated AV1 decode is even less likely: it needs a 2020-or-newer GPU (Intel 11th-gen/Tiger Lake, NVIDIA RTX 30, AMD RX 6000+) plus the AV1 Video Extension on Windows, otherwise Chrome uses non-accelerated software AV1. So "delivers HW HEVC/AV1, not software H.264" is achievable only on a favorable, recent target with hardware acceleration on — not a guarantee for locked-down public PCs generally — and "Sunshine-grade quality" is not automatic since native Moonlight typically outperforms browser WebRTC on latency/tuning.


**[CONFIRMED]** Restoring moonlight-qt's QSettings store (the Windows registry key HKCU\Software\Moonlight Game Streaming Project\Moonlight, or a portable INI) that contains the client certificate, private key, uniqueid, and the paired-host record with its pinned server certificate lets Moonlight connect to an already-paired Sunshine/Apollo host with no PIN and no re-pairing.


> 정정/정밀화: Accurate. moonlight-qt keeps all client-side pairing state in one QSettings store under organization "Moonlight Game Streaming Project" / application "Moonlight" — on Windows the native registry key HKCU\Software\Moonlight Game Streaming Project\Moonlight, or, when a portable.dat file sits next to the executable, a portable INI in that directory. IdentityManager writes the client identity there as plain-PEM values under keys certificate, key, and uniqueid; ComputerManager writes each paired host into a "hosts" array whose record holds the pinned server cert under srvcert (plus uuid and addresses). Because Sunshine/Apollo (GameStream) pairing is mutual TLS keyed on the client certificate the host authorized at pair time, with the client pinning the host's cert, faithfully restoring that store makes Moonlight present the already-authorized client cert and validate the host against the pinned srvcert, so the host returns PairStatus=1 and streaming resumes with no PIN and no re-pairing. Two implicit preconditions — both true for a host that is genuinely still paired and unchanged — are that the host must still list that client certificate as authorized and must still serve the same server certificate that was pinned (if the host was reset or its cert regenerated, this breaks and re-pairing is required). Caveat: this is a manual, unofficial operation, not a product feature — moonlight-qt has no export/import command (open feature request #1576), and it is portable across machines only because the private key is stored unencrypted with no OS/machine binding.


**[CONFIRMED]** No Sunshine/Moonlight-protocol project today couples GStreamer rtpgccbwe / Google Congestion Control bandwidth estimation to the encoder bitrate — the Sunshine WebRTC browser forks (Vibeshine, LuminalShine, moonlight-web-stream) use WebRTC only as a browser transport with a fixed encoder bitrate.


> 정정/정밀화: Correct. As of mid-2026, no Sunshine/Moonlight-protocol project couples a GStreamer rtpgccbwe / Google Congestion Control bandwidth estimate to the video encoder bitrate: a GitHub-wide rtpgccbwe search surfaces only non-Moonlight WebRTC projects (WebKit, generic gst-webrtc apps, Selkies), and the GStreamer-based Moonlight server Wolf plus Sunshine and all four forks contain no rtpgccbwe/TWCC/REMB code. The Sunshine WebRTC browser forks (Vibeshine, LuminalShine, moonlight-web-stream) use WebRTC purely as a browser transport and do not feed libwebrtc's bandwidth estimate back to the encoder. One refinement: the bitrate is not strictly immutable — Vibeshine/Vibepollo added a runtime bitrate endpoint with ABR capability negotiation that lets a supporting client request mid-stream bitrate changes — but this is client-driven, not automatic congestion-control (GCC/BWE) coupling, so the core claim holds. (The protocol also structurally lacks the TWCC feedback rtpgccbwe requires; adaptive bitrate/GCC was requested in moonlight-qt #802 and closed as not planned.)


**[CONTEXT_DEPENDENT]** A self-hosted coturn TURN server configured for TLS relay over TCP 443 on a public VPS lets a browser WebRTC client (vibeshine or moonlight-web-stream) relay Sunshine media through WARP, because the only outbound flow the client makes is TCP 443 to a public host that WARP transparently egresses.


> 정정/정밀화: Feasible with software that ships today, but the premise is oversimplified. You CAN run coturn with TURN-over-TLS on TCP 443 (turns:host:443?transport=tcp, plus a setcap/privileged-port step), point a browser WebRTC Sunshine client at it, and have Cloudflare WARP — which defaults to a full 0.0.0.0/0 tunnel egressing from Cloudflare IPs — transparently carry the client's outbound TCP 443 to that VPS; both vibeshine (a Sunshine fork with an experimental WebRTC browser path) and moonlight-web-stream (a third-party WebRTC bridge) exist and accept custom TURN/ICE config. But "the only outbound flow the client makes is TCP 443" is true ONLY if the browser client is set to iceTransportPolicy:'relay' with a TURNS/TCP-443-only ICE server. As shipped, both named clients are UDP-first (direct host candidates, example TURN on UDP:3478) and neither exposes a documented relay-only toggle — vibeshine's env only sets the iceServers list, not the transport policy — so without a patch the browser will also gather/attempt UDP paths, and egress is not actually confined to TCP 443. There is also always a separate signaling channel (vibeshine: HTTPS REST + SSE; moonlight-web-stream: WebSocket) plus DNS, so it is not literally a single flow (though those are typically TCP 443 too). And because WARP is a full L3 tunnel that also carries UDP, the TCP-443 relay's real benefit is bypassing WARP's CGNAT-style egress that breaks P2P/UDP NAT traversal — not that WARP only passes TCP 443. Bottom line: the approach works and every piece ships today if you enforce relay-only TURNS/TCP-443 (or patch the client to do so); the claim's clean single-flow guarantee is an idealization the default clients don't deliver.
