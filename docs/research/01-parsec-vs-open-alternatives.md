# 리서치 1 — Parsec vs Moonlight/Sunshine, 그리고 브라우저 무설치 옵션

> “Moonlight/Sunshine이 Parsec보다 낫다”를 검증하고, 사지방 제약(무설치·재인증) 하의 옵션을 조사.

_다중 에이전트 리서치 + 적대적 검증. run: `wf_b16739f8-469`. 2026-07._

---

# betterparsec 리서치 메모 (최종)

작성 관점: principal engineer / 소스는 6인 리서치 + 적대적 검증(verification) 결과에 한정. 검증되지 않은 것은 UNVERIFIABLE로 명시함.

---

## 0. 한 줄 결론

- **"Moonlight/Sunshine가 Parsec보다 낫다"는 통념은 조건부이며, 당신의 실제 시나리오(제약된 인터넷 + 손 안 대는 자동화 + 브라우저 전용)에서는 오히려 Parsec 아키텍처가 더 적합하다.** 다만 Parsec은 사지방 방화벽(UDP 차단)을 못 뚫어서 탈락한다.
- **사지방 → 집 Windows PC, 브라우저 전용의 현실적 최적해는 Chrome Remote Desktop(주력)**, 차선이자 betterparsec의 토대는 **self-host moonlight-web-stream(Sunshine 호스트 + WebRTC/WebSocket fallback)**.
- **Parsec 본체는 포크 불가**(closed-source + SDK 라이선스가 리버스엔지니어링/파생물/경쟁제품 금지). betterparsec은 반드시 **Sunshine(GPL-3.0) 계열 오픈소스 위에** 지어야 한다.
- 사지방에서 원격제어는 **사지방 이용수칙 위반 소지**가 있다(계정 삭제/징계 리스크). 기술 이전에 규정 리스크를 먼저 판단해야 한다.

---

## 1. "Moonlight/Sunshine outperforms Parsec"는 사실인가?

지연(latency)과 저비트레이트 화질을 **분리해서** 봐야 하고, 결론이 반대로 갈린다.

### 1-A. 지연 (latency) — [CONTEXT-DEPENDENT] · Moonlight의 보편적 우위는 미검증

- **Parsec의 LAN 추가 지연 ~7 ms는 [CONFIRMED].** Parsec 자체 DIY 테스트가 1000fps급 카메라(Sony RX100 IV)로 클릭→화면을 측정: 스트리밍 클라이언트 23 ms − 로컬 베이스라인 16 ms = **추가 7 ms**. 단 검증 결과, 이 특정 글에는 해상도/코덱 표기가 없고 "1080p/H.264"는 별도 240fps 글(총 파이프라인 4–8 ms)에서 온 값이다. Parsec은 이를 "glass-to-glass"가 아니라 "input/protocol latency"로 표현함. (https://parsec.app/blog/testing-game-streaming-input-latency-on-parsec-with-diy-instructions-49ae838f45a7 , https://parsec.app/blog/parsec-game-streaming-total-latency-at-240-frames-per-second-c0818cc0daa5)
- **Moonlight/Sunshine의 "~5 ms(4–6 ms)"는 카메라 측정이 아니라 앱 내부 오버레이 통계다 — [CONFIRMED].** Moonlight 공식 FAQ가 명시: 이 수치들은 network/decode/frame-queue/render 같은 파이프라인 구성요소 값이며 compositor/display/input 지연은 못 잡고, **"다른(비-Moonlight) 클라이언트와 비교에 쓸 수 없다"**고 못 박음. 즉 Parsec의 카메라 7 ms와 **동일 척도가 아니다**. 5 ms를 진짜 end-to-end로 볼 수 없다. (https://github.com/moonlight-stream/moonlight-docs/wiki/Frequently-Asked-Questions , https://news.ycombinator.com/item?id=40405214)
- **"VPN 터널링이 10–25 ms 더하고 Parsec은 이를 피한다"는 메커니즘은 [CONTEXT-DEPENDENT]로 상당 부분 반박됨.** 검증 결과: Tailscale/ZeroTier(WireGuard)의 **직결(direct) 터널은 겨우 <1–3 ms** 추가일 뿐이고, 10–25 ms는 **먼 릴레이(DERP)로 fallback됐을 때만** 발생한다. Parsec도 동일하게 direct-first → 실패 시 relay(HPR) fallback 구조라 릴레이 시 지연이 붙는다. **양쪽 다 direct면 전송 지연은 비슷하고 코덱 파이프라인이 지배적**이다. 10–25 ms 숫자의 출처는 SEO 블로그이지 벤치마크가 아니다. Parsec의 실질 우위는 "무설정 NAT 트래버설로 비전문가도 좋은 경로에 붙을 확률이 높다"는 **편의성**이지, VPN이 본질적으로 느려서가 아니다. (https://tailscale.com/docs/reference/connection-types , https://support.parsec.app/hc/en-us/articles/32381482907156-Deployment-Considerations-and-Options)
- **정성 평가는 갈린다.** XDA는 LAN/Wi-Fi에서 Sunshine+Moonlight가 "약간 더 부드럽고 아티팩트 적다"고 했고(https://www.xda-developers.com/sunshine-moonlight-vs-parsec/), airgpu 등은 반대로 Parsec이 "조금 더 반응성 좋다"고 함(https://airgpu.com/blog/parsec-vs-moonlight/). 둘 다 ms 데이터 없음.
- **독립적·동일조건 glass-to-glass ms 벤치마크(양쪽 모두)는 존재를 못 찾음 — [UNVERIFIABLE].** 후보였던 YouTube 2건은 본문 회수 불가, tech-insider 기사는 403. 신뢰 불가. (https://www.youtube.com/watch?v=dkQ3ySLry5M , https://tech-insider.org/parsec-vs-moonlight-vs-steam-link-2026/)

**지연 판정: Moonlight의 보편적 지연 우위는 검증되지 않았다.** LAN에서는 셋업 노이즈 수준 차이, WAN에서는 설정에 따라 Parsec이 동등하거나 낮을 수 있음. "Moonlight가 빠르다"는 흔한 벤치는 **척도가 다른 숫자를 비교한 착시**에 상당 부분 기댄다.

### 1-B. 저비트레이트(~3–15 Mbps, 인터넷) 화질 — 통념과 반대. 여기선 **Parsec이 낫다**

- **Moonlight/Sunshine은 혼잡 기반 적응형 비트레이트가 없다 — [CONFIRMED].** 사용자가 고정 타깃 비트레이트를 직접 정하고, moonlight-qt의 동적 비트레이트/혼잡제어 요청(issue #802)은 **"not planned"으로 종료(2022-06-05)**. 대안 요청(#1618)도 미구현. FEC/프레임드롭은 있으나 이는 오류복원이지 비트레이트 적응이 아니다. → 지터/비대칭 링크에서 고정 비트레이트가 순간 대역을 넘으면 매크로블로킹/스터터. (https://github.com/moonlight-stream/moonlight-qt/issues/802 , https://github.com/moonlight-stream/moonlight-qt/issues/1618)
- **반면 Parsec의 저비트레이트 강점은 코덱이 아니라 네트워크 계층(BUD)이다 — 잘 뒷받침됨.** BUD는 커스텀 UDP(DTLS 1.2)로 **비디오 버퍼가 전혀 없고**, 혼잡을 "발생 전에" 감지해 마이크로초 단위로 비트레이트를 조정, "12 Mb/s 링크를 손실·지연 없이 가장자리까지" 채운다. 우선순위: latency > framerate > quality. **손 안 대도 일관된 그림**을 유지하는 구조적 이유가 이것이다. (https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007 , https://parsec.app/technology)
- **코덱 효율은 Moonlight 쪽으로 유리 — [CONFIRMED].** Parsec은 **AV1 미지원(H.264/H.265만)**, Moonlight/Sunshine은 RTX 40 / Intel Arc / AMD RX 7000에서 **AV1 지원**(호스트가 인코딩, Moonlight가 디코딩). AV1은 저비트레이트에서 유리(뒤 3-4 참조). (https://github.com/orgs/LizardByte/discussions/245 , https://support.parsec.app/hc/en-us/articles/32381785123860-Improve-Stream-Quality-and-Color-Accuracy)
- **텍스트 선명도용 4:4:4 비교는 [DISPUTED](통념 절반이 낡음).** Parsec 4:4:4(H.265, Warp/Teams 유료, NVIDIA/Intel + Windows 호스트, AMD 미지원)는 사실. 그러나 **"Sunshine은 4:2:0만"은 틀렸다**: Sunshine은 **2025-01-18 stable v2025.118.151840에서 Windows Intel/NVIDIA용 YUV 4:4:4를 정식 출시**했고 2026 릴리스에서 Linux NVIDIA로 확대됨. AMD 미지원은 여전히 사실. (https://github.com/LizardByte/Sunshine/releases/tag/v2025.118.151840 , https://github.com/orgs/LizardByte/discussions/220)

**저비트레이트 판정:** 실제 제약된 인터넷에서 **손 안 대는 일관된 그림 + 텍스트**는 **Parsec 우위**(BUD 적응형 + 4:4:4). Moonlight는 **깨끗하고 수동 튜닝 가능한 링크 + AV1 하드웨어**일 때만 코덱 효율로 앞선다. 즉 "Moonlight가 저비트레이트에 낫다"는 통념은 **AV1 등장 이전 명성의 잔재**이며, 당신의 시나리오에는 대체로 부합하지 않는다.

---

## 2. 브라우저 전용(무설치) 옵션 비교 — 사지방 공용 PC → 집 PC

**사지방의 구속 조건**(§5 근거): ① 클라이언트 설치 불가·재부팅 시 스냅샷 초기화, ② UDP/비표준 포트 상당수 차단(TCP 443이 사실상 유일한 신뢰 경로), ③ 강제 로그아웃(약 120분)으로 매 세션 재인증. 이 3개를 동시에 만족하는지가 관건이다.

| 옵션 | 클라 설치 불필요 | 지연 | 저비트레이트 화질 | self-host | 인증 마찰 | 사지방 방화벽(UDP차단) 통과 | 집 Windows PC 직접 도달 |
|---|---|---|---|---|---|---|---|
| **Chrome Remote Desktop** | ✅ (순수 브라우저) | 데스크톱·경게임 양호(게임 전용 코덱 아님, VP8) | 수용 가능(AV1/HEVC 아님) | ❌ (Google 중개) | **낮음**: Google 로그인+호스트 PIN | **✅ TCP 443 fallback** | ✅ |
| **moonlight-web-stream (+Sunshine)** | ✅ | WebRTC 양호(네이티브보다 소폭 열위) | AV1/HEVC 가능하나 **적응형 비트레이트 없음** | ✅ | 중간: PIN 페어링+계정, HTTPS 필요 | **가능**: WebRTC(UDP) 우선, **WS fallback(443)** 있음 | ✅ |
| **Parsec Web App (web.parsec.app)** | ✅ (Chrome 전용) | 네이티브보다 나쁨(**SW 디코드**) | BUD 이점 있으나 브라우저라 열화 | ❌ | 낮음(계정) — 단 stateless라 매 세션 재로그인 | **✕ 대체로 실패**: 비디오 UDP-only, 6013, TCP fallback 없음 | ✅(Windows), macOS 호스트 ✕ |
| **Apache Guacamole** | ✅ (HTML5) | RDP/VNC 사무용, **빠른 모션 부적합** | 저모션/텍스트 OK, 영상 나쁨 | ✅ | 중간(자체 auth/LDAP/TOTP) | ✅ (WebSocket 443) | ✅ (RDP 경유) |
| **RustDesk web** | ✅ | **릴레이 홉(P2P 없음)→ 높음** | WebCodecs(VP8/9/H264/265) | ✅ (서버) | 중간(ID/PW, 2FA) | 가능(WS 릴레이) | ✅ |
| **Selkies** | ✅ | 양호(코덱 전부 AV1 포함) | 좋음 | ✅ | 중간(basic/역프록시) | 조건부(WebRTC, 컨테이너면 TURN 필수) | **✕ (Linux X11 데스크톱만)** |
| **neko** | ✅ | <300ms | AV1 파이프라인 있으나 **동작 안 함** | ✅ | 낮음(공유 PW) | 조건부(WebRTC/TURN) | **✕ (호스트=컨테이너, 집 게이밍 PC 아님)** |

근거: CRD (https://support.google.com/chrome/a/answer/16364503?hl=en , https://en.wikipedia.org/wiki/Chrome_Remote_Desktop) · Parsec Web [CONFIRMED: Chrome 전용·무설치·macOS 호스트 불가·브라우저 HW 디코드 없음] (https://support.parsec.app/hc/en-us/articles/32381650129300-Use-the-Web-App-browser) · Parsec UDP-only/6013 (https://support.parsec.app/hc/en-us/articles/32381914838804-Error-Codes-6013-UDP-Blocked-By-Your-Network) · moonlight-web-stream (https://github.com/MrCreativ3001/moonlight-web-stream) · Guacamole [WebRTC 아님·게이밍 부적합] (https://en.wikipedia.org/wiki/Apache_Guacamole) · RustDesk web [CONFIRMED: WebRTC/P2P 없음, WS 릴레이 홉] (https://deepwiki.com/rustdesk/rustdesk/4.3-web-client) · Selkies (https://github.com/selkies-project/selkies) · neko [AV1 동작 안 함] (https://github.com/m1k1o/neko) · Wolf는 **네이티브 Moonlight 클라 필요, 웹 UI는 관리용 [CONFIRMED]** 이라 애초에 브라우저 스트리밍이 아님(https://games-on-whales.github.io/wolf/stable/index.html) · Rainway는 2022-10-26 서비스 종료(제외, https://en.wikipedia.org/wiki/Rainway).

### 추천

- **최적(주력): Chrome Remote Desktop.** 사지방 3대 제약을 **동시에** 만족하는 유일 옵션이다 — (a) 무설치라 스냅샷 초기화와 무관, (b) **UDP 차단 시 TCP 443 fallback**으로 방화벽 통과, (c) 신뢰 앵커가 **로컬 인증서/설치가 아니라 Google 계정**이라 매 세션 재인증이 "Google 로그인 + 호스트 PIN" 뿐. 집 Windows PC 직접 도달. 한계: VP8 기반이라 빠른 모션 게임 손맛은 Parsec/Moonlight보다 못하지만, **애초에 붙기라도 하는 건 이것뿐**이다.
- **차선(그리고 betterparsec의 토대): self-host moonlight-web-stream(집 Sunshine 호스트 + 브라우저 클라).** 게이밍/코덱(AV1/HEVC)이 CRD보다 낫고 여전히 무설치이며, **WebSocket fallback이 HTTPS로 제약 네트워크를 뚫을 여지**가 있다. 단점: experimental, 집에 서버+TURN+HTTPS 구축 필요, 세션마다 PIN 페어링 마찰, 하드닝 미완.
- **Parsec Web App은 왜 탈락?** 브라우저 게이밍 손맛 자체는 계정형 중 최고에 가깝지만, **비디오가 UDP-only이고 TCP fallback이 없어(6013)** 사지방 방화벽에서 신뢰성 있게 붙지 못한다. 이 특정 유스케이스에서 CRD 아래로 내려간다.

---

## 3. Parsec 자체를 포크/개선할 수 있는가? → **불가**

- **proprietary/closed-source — [CONFIRMED].** 게임스트리밍 Parsec(Unity 소유, 2021년 $320M 인수)은 애플리케이션 소스가 비공개이고, 공개된 유일 산출물인 Parsec SDK조차 **닫힌 바이너리**다. (동명의 parsec.cloud는 무관한 파일공유 제품이니 혼동 금지.) (https://en.wikipedia.org/wiki/Parsec_(software))
- **SDK 라이선스가 포크를 원천 금지 — [CONFIRMED].** 실제 라이선스 원문 검증 결과: 리버스엔지니어링/디컴파일, **파생물(derivative works) 생성**, **경쟁 제품/서비스 구축**을 명시적으로 금지하고, 사용 목적을 **"Parsec의 Service와의 상호운용성 개발"로만** 한정한다. (https://github.com/MalfoyJW/parsec-sdk/blob/master/LICENSE.md)

**결론: "Parsec을 고쳐서 더 낫게"는 법적으로 불가.** betterparsec은 반드시 오픈소스 스택 위에 새로 지어야 하며, 현실적 토대는 **Sunshine(GPL-3.0) + Moonlight 프로토콜/입력 스택**이다. (Sunshine이 GPL-3.0이므로 포크 기반 호스트는 **카피레프트 의무**를 상속함 — https://github.com/lizardbyte/sunshine)

---

## 4. betterparsec 아키텍처 제안

핵심 원칙: **인코더/입력주입/Moonlight 스택을 새로 짜지 말 것.** 이미 통합된 포크를 재사용한다.

**베이스 후보:** Nonary **Vibeshine/Vibepollo**(Sunshine 포크 + 실험적 WebRTC 브라우저 클라: H.264/HEVC/AV1 자동 협상, Moonlight 입력 스택을 WebRTC 데이터채널로 재사용, PCM→WebRTC 오디오, ICE=env). 또는 **moonlight-web-stream**(WebRTC + WebSocket/WebCodecs fallback). (https://github.com/Nonary/vibeshine/blob/vibe/architecture.md , https://github.com/MrCreativ3001/moonlight-web-stream)

| 계층 | 제안 | 근거/주의 |
|---|---|---|
| Transport | WebRTC(libwebrtc in-fork 또는 GStreamer webrtcsink), 입력은 unreliable data channel. **UDP 차단 대비 WebSocket+WebCodecs fallback을 필수로 유지** | 사지방류 UDP 차단 네트워크에서 이게 생명줄. TURN(coturn/Cloudflare) 필요 |
| Codec | **AV1 우선**(RTX 40+ NVENC) + **HEVC/H.264 fallback 필수**, 클라 `RTCRtpSender.getCapabilities`로 per-client 협상 | AV1은 H.264 대비 **~40% 비트레이트 절감(+1.5–2 dB PSNR) — [CONFIRMED]** (https://developer.nvidia.com/blog/improving-video-quality-and-performance-with-av1-and-nvidia-ada-lovelace-architecture/). 단 **클라 브라우저의 AV1 HW 디코드가 비보편적**이라 fallback 없으면 저사양/모바일에서 CPU 폭증 |
| Host | Sunshine 포크(NVENC) + Moonlight 입력주입 재사용 + PCM 오디오 | GPL-3.0 카피레프트 상속 |
| Browser client | RTCPeerConnection + `<video>`/WebCodecs, Keyboard Lock/Pointer Lock/Gamepad | **Keyboard Lock은 Chromium 전용·전체화면·권한 필요(Chrome 131+) — [확인]**, FF/Safari/모바일은 열위 (https://developer.chrome.com/docs/capabilities/web-apis/keyboard-lock) |
| **Auth(세션마다 재페어링 회피)** | Moonlight의 **기기 인증서 페어링을 버리고 계정 기반(OIDC/세션 토큰)**으로. 신뢰 앵커를 서버측에 영속화(=CRD 모델) | stateless 공용 PC에서 로컬 인증서가 매번 초기화돼 재페어링을 강제하는 게 Moonlight 최악의 마찰 — [확인]. **이 항목이 betterparsec의 진짜 차별점** (https://github.com/moonlight-stream/moonlight-docs/wiki/Setup-Guide) |

**타당성 판정(솔직):** **가능하되 "조립"이 아니라 "포크"로.** 포크 기반 PoC는 **수 주**, "any-browser/any-device 프로덕션"은 **수 개월**(신규 엔지니어링이 아니라 통합·하드닝 지배). 모든 현존 브라우저 경로가 스스로 **experimental**이라고 표기한다는 점이 핵심 리스크.

**Top 3 리스크**
1. **사지방 방화벽/규정.** WS fallback + TCP 443이라도 DPI·egress 필터가 막을 수 있고, 무엇보다 **이용수칙 위반**(§5). 기술 성공 ≠ 사용 허용.
2. **크로스브라우저 입력 캡처.** Keyboard Lock이 Chromium 전용·전체화면·권한 게이트라 "아무 브라우저나" 약속이 깨진다. Safari/모바일은 반쪽.
3. **TURN 인프라 + egress 비용 + 미성숙.** 게임 비트레이트(10–20 Mbps)에서 릴레이 세션 비용이 큼(coturn ~$180–220/월 + ~$0.09/GB), 그리고 모든 브라우저 경로가 experimental이라 신뢰성 하드닝이 실제 일의 대부분. (보조 리스크: GPL-3.0 의무, AV1 클라 디코드 파편화.) (https://callsphere.ai/blog/vw3e-webrtc-turn-scaling-coturn-vs-cloudflare-2026)

---

## 5. 사지방 군 보안/규정 (사실 정리)

- **원격제어는 사지방 이용수칙 위반 소지.** 1차 출처(현역 개발자 기록)의 이용수칙에 **"원격접속 / ftp, telnet 등 자료 통신 프로그램 사용 금지"**가 명시. 위반 시 **사지방 계정 삭제·징계** 사례 언급. 근거 규정은 국방부 훈령 **"사이버지식정보방 운영 및 관리에 관한 훈령"**. (https://github.com/dohan0930/TIL_ROKA/blob/master/etc/coding_in_SAJIBANG.md , https://www.law.go.kr/LSW/admRulInfoP.do?admRulSeq=2000000016242)
- **환경 제약:** 전원 종료 시 스냅샷 초기화, 약 120분 강제 로그아웃, 우클릭/설치 제한, 방화벽이 다수 목적지·포트 차단(이 값들은 **부대별 상이·1차 증언 기반이라 confidence medium**).
- **리스크 성격:** 사지방은 **국방망(인트라넷)이 아니라 별도 인터넷망**이므로, 주된 위험은 인트라넷 침해가 아니라 **규정 위반/opsec + 계정·징계 처분**이다. 단속 강도는 부대별로 다름.
- **판단:** 기술적으로 CRD가 "붙는다"는 것과 "써도 된다"는 별개다. **사용 장소를 재고하는 것이 안전** — betterparsec을 만들더라도 실사용은 규정 리스크가 없는 환경(예: 집 외부망, 휴가지 등)을 기준으로 설계·사용 권장.

---

## 6. 무엇을 먼저 할지 (다음 1–2 스텝)

1. **오늘: 집 PC에 Chrome Remote Desktop 호스트 설치 → 외부에서 브라우저로 접속 검증.** 특히 **UDP가 막힌 네트워크에서 TCP 443 fallback으로 실제로 붙는지**를 사지방과 유사한 제약 네트워크에서 확인한다(무료·즉시·가장 확실). 단, 사지방 실사용 전 §5 규정 리스크를 먼저 판단.
2. **betterparsec을 진지하게 갈 경우: 집 Sunshine 호스트 + Vibeshine/Vibepollo(또는 moonlight-web-stream) 포크로 브라우저 PoC 구축.** 최소 목표는 (a) **WebSocket fallback으로 UDP 차단 통과**, (b) **계정 기반 인증으로 세션마다 PIN 재페어링 제거**, (c) AV1↔HEVC/H.264 per-client 협상. 이 3개가 CRD 대비 유일한 실질 우위이자 검증 대상이다.


---

## 적대적 검증 판정 (12건)


**[CONFIRMED]** Parsec does not support AV1 encoding (only H.264 and H.265/HEVC), whereas Moonlight/Sunshine added AV1 support for RTX 40-series, Intel Arc, and AMD RX 7000 GPUs.


> 정정/정밀화: As of mid-2026, Parsec streams only with H.264 and H.265/HEVC and offers no AV1 support. The Sunshine/Moonlight pair does support AV1: the Sunshine host performs AV1 hardware encoding, added in 2023, on GPUs that have AV1 encoders — NVIDIA RTX 40-series (and newer), Intel Arc, and AMD RX 7000-series (and newer) — while the Moonlight client decodes the AV1 stream. These GPU generations are required because they are the first with hardware AV1 encoders, not because of an arbitrary Sunshine limitation.


**[CONFIRMED]** Parsec's own 1000 fps high-speed-camera LAN test measured about 7 ms of added glass-to-glass input latency (23 ms streamed vs 16 ms local baseline) at 1080p H.264 on gigabit ethernet.


> 정정/정밀화: Parsec's own DIY input-latency test — recording a mouse-click-to-display response with a 1000 fps-capable camera (Sony RX100 IV) over a gigabit LAN — measured 7 ms of added input latency: 23 ms on the Parsec client (streamed) minus a 16 ms direct/local baseline (C-H = 7 ms). The test ran Counter-Strike: Source above 240 Hz. Note: that specific blog post does NOT state the resolution or codec, so "1080p H.264" is not confirmed by the article that produced these numbers (it comes from Parsec's separate 240 fps test, which reported 4-8 ms of total LAN pipeline latency). Parsec framed the 7 ms as click-to-display "input/protocol latency" rather than using the term "glass-to-glass."


**[CONFIRMED]** Moonlight/Sunshine community latency figures of about 5 ms (4-6 ms) over ethernet are in-app overlay pipeline stats, not camera-measured glass-to-glass, and Moonlight's docs state these values are not directly comparable to other streaming tools.


> 정정/정밀화: Correct. Community reports of ~5 ms (4-6 ms) Sunshine+Moonlight latency over ethernet (e.g., Hacker News item 40405214) are Moonlight's own in-app overlay figures, which the docs define as pipeline-component metrics — network/ping, decode, frame-queue, and render latency — not a camera-measured glass-to-glass number. The Moonlight FAQ explicitly notes these figures omit unmeasurable system latency ("compositor latency, display latency, input device latency, etc"), and states they "can't be used to compare to other non-Moonlight clients, since each value may not be measuring the same thing, despite potentially having the same name." One caveat: the community posters do not themselves label their numbers as overlay readings — that is an inference (a firm one, since ~5 ms is only interpretable as an overlay pipeline stat, most likely the network-latency line, and is far too low to be a true glass-to-glass measurement). The docs' wording is "other non-Moonlight clients" rather than "other streaming tools," but the meaning is equivalent.


**[CONFIRMED]** Moonlight/Sunshine does not implement congestion-driven adaptive/dynamic bitrate — it uses a fixed user-set target bitrate, and the moonlight-qt feature request for dynamic bitrate/congestion control (issue #802) was closed as 'not planned.'


> 정정/정밀화: Correct. Mainline Moonlight/Sunshine does not implement congestion-driven adaptive or dynamic bitrate; the user sets a fixed target bitrate (the H.264/HEVC encoder target, to which ~20% FEC overhead is added on the wire), and it is not automatically lowered or raised in response to network congestion, delay, or packet loss. The moonlight-qt feature request "Dynamic bitrate/congestion control" (issue #802) was closed as "not planned" on 2022-06-05 (with no written explanation), and a separate open request (#1618) plus community idea #217 for auto-adjusting bitrate remain unimplemented. Note: Moonlight/Sunshine do use FEC and drop/recover frames (IDR requests) under packet loss for error resilience, but that is distinct from congestion-based bitrate adaptation.


**[CONFIRMED]** RustDesk's web client has no direct browser WebRTC/P2P path and routes all traffic through a WebSocket relay, adding a relay hop to latency.


> 정정/정밀화: RustDesk's browser web client cannot open raw TCP/UDP sockets, so it has no WebRTC or NAT-hole-punched direct-P2P path. In the normal rendezvous flow it talks WebSocket to RustDesk's servers (HBBS ID/rendezvous on port 21118, relay HBBR on 21119) and its session data is always relayed through HBBR, whereas native clients establish a direct peer-to-peer connection in most cases. The web client therefore incurs an extra relay hop and typically higher latency than native direct connections. Minor caveat: "all traffic" is slightly overstated because RustDesk has a "direct IP access" feature, but for a browser this is not the default — it requires the peer to be directly reachable on a WebSocket port or a local WS-proxy workaround — and RustDesk has stated it has no plans to adopt WebRTC for the web client.


**[CONFIRMED]** Wolf (Games-on-Whales) requires a native Moonlight client to stream and its web UI is management-only, so it is not a zero-client-install browser streaming solution by itself.


> 정정/정밀화: Wolf (Games-on-Whales) is a Moonlight streaming server: actual game/desktop video is delivered over the Moonlight protocol, so a user must connect with a separately installed Moonlight client to stream. Wolf's built-in web interfaces (the TypeScript management/config UI and the PIN-pairing page) are for setup, pairing, and app launching — they do not stream video in the browser. Therefore Wolf, by itself, is not a zero-client-install, browser-based streaming solution; in-browser Moonlight streaming requires separate, unofficial third-party bridges (e.g. moonlight-web / moonlight-web-stream, which are typically built against Sunshine) that are not part of Wolf.


**[DISPUTED]** Parsec offers 4:4:4 chroma subsampling (H.265, for Warp/Teams paying customers on NVIDIA/Intel Windows hosts) while stable Sunshine encodes 4:2:0 only, with 4:4:4 still in nightly/testing and unsupported on AMD.


> 정정/정밀화: Parsec offers a 4:4:4 color mode over HEVC/H.265 as a paid feature (Teams and Warp customers), requiring an NVIDIA or Intel GPU with hardware encoding on a Windows host (AMD hosts unsupported; guests without a compatible NVIDIA/Intel decoder fall back to software decoding). Sunshine, however, is no longer limited to 4:2:0: native YUV 4:4:4 encoding for Intel and NVIDIA GPUs on Windows shipped in the stable release v2025.118.151840 on 2025-01-18 (PR #2533), and Linux NVIDIA (CUDA/CUDA-GL) 4:4:4 including HDR followed in stable 2026 releases (v2026.704.34109 and v2026.709.11046). 4:4:4 on AMD GPUs remains unsupported in Sunshine due to a hardware limitation through the Radeon RX 7900 series. Thus the claim's "stable Sunshine encodes 4:2:0 only, 4:4:4 still in nightly/testing" is outdated/incorrect as of mid-2026; the Parsec and AMD-unsupported details are correct.


**[CONTEXT_DEPENDENT]** Over the internet, tunneling Moonlight/Sunshine through a VPN such as Tailscale or ZeroTier adds roughly 10-25 ms on top of base RTT, whereas Parsec's built-in NAT traversal/relay avoids this, so Parsec often achieves equal or lower real-world WAN latency.


> 정정/정밀화: A DIRECT Tailscale/ZeroTier (WireGuard-based) tunnel adds only sub-millisecond to ~3 ms over base RTT, not 10-25 ms; the 10-25 ms penalty appears only when the VPN falls back to a relayed (DERP) path through a distant server. Parsec does not inherently avoid this: it uses the same direct-first, relay-fallback model (UDP hole-punching plus HPR relay servers), and its relay also adds latency when direct P2P fails. When both Moonlight/Sunshine (with a direct connection - via a direct VPN tunnel or plain port-forwarding, with no VPN overhead) and Parsec achieve direct connections, real-world WAN transport latency is comparable, with the codec/encode-decode pipeline dominating. Parsec's practical advantage is convenience and reliability - automatic, zero-config NAT traversal and a streaming-optimized relay network make a good direct or low-latency path more likely for non-expert users, so Parsec can end up equal or lower in real-world latency mainly when a naive VPN setup gets stuck on a distant relay, not because VPN tunneling inherently costs 10-25 ms.


**[CONFIRMED]** Parsec's game-streaming application source code is not publicly available and the software is proprietary/closed-source (as opposed to the unrelated parsec.cloud file-sharing project which is BUSL-1.1 open source).


> 정정/정밀화: Correct. The game-streaming Parsec (by Parsec Cloud Inc., now part of Unity) is proprietary/closed-source: its application code is not published, and even its official SDK ships as a closed binary under a proprietary "Parsec SDK License" (open-source projects like ParsecSoda/OpenParsec are third-party wrappers around this closed SDK, not Parsec's own source). The unrelated parsec.cloud (by Scille) is a separate encrypted file-sharing product whose source is public on GitHub under BUSL-1.1 — which Scille markets as "100% Open Source," though BUSL-1.1 is more precisely a source-available license with commercial-use restrictions rather than an OSI-approved open-source license.


**[CONFIRMED]** The Parsec SDK license prohibits reverse engineering, creating derivative works, and building competitive products, and only permits building integrations that connect to Parsec's own service.


> 정정/정밀화: The Parsec API and SDK Terms of Use Agreement (Parsec Cloud Inc.) grants only a limited, non-exclusive, non-transferable, non-sublicensable license to use the API/SDK "solely to develop interoperability between your application and the Service" (Section 1). Section 2 expressly prohibits modifying or creating derivative works of, and reverse engineering/decompiling/disassembling, the API/SDK/Service (both "except as permitted by applicable law"), and prohibits using the API, SDK, or Service "to build a competitive product or service," while also barring access to the Service by any means other than the API/SDK. The claim is accurate; the only caveats are that the reverse-engineering and derivative-works bans include a statutory carve-out ("except as permitted by applicable law"), and that broader capabilities (e.g., managing End User accounts) require a separately negotiated Enterprise SDK License rather than the standard terms.


**[CONFIRMED]** Parsec's BUD protocol is UDP-based, DTLS 1.2-encrypted, and adds ~7ms of latency on a LAN with a claimed 97% NAT traversal success rate.


> 정정/정밀화: Per Parsec's own documentation, BUD ("Better User Datagrams") is a proprietary peer-to-peer protocol that is UDP-based and encrypted with DTLS 1.2 (via OpenSSL, AES128/AES256 per packet), and Parsec claims a 97% NAT traversal success rate for it. The "~7ms on a LAN" figure is accurate to Parsec's stated "adds only 7 milliseconds of latency" on a LAN ethernet test setup, but Parsec attributes that number to its overall native-code streaming pipeline rather than to the BUD protocol in isolation. All of these are vendor self-reported figures from parsec.app, not independently verified benchmarks (the same page also separately cites 95% of users successfully co-playing, a distinct user-outcome metric).


**[CONFIRMED]** Parsec's official Web App at web.parsec.app can connect to your own Parsec host PC with only a Chrome/Chromium browser and no client install, but cannot join macOS hosts and has no hardware decoding in the browser.


> 정정/정밀화: Correct as stated. Per Parsec's official documentation, the Web Client at web.parsec.app lets you access your own host computer using only a Google Chrome or Chromium browser with no Parsec app install. As of current documentation, it cannot join macOS hosts ("Joining macOS computers is not possible with the web app at this time"), and it has no access to hardware decoding in the browser because browsers do not expose direct access to it (it relies on Media Source Extensions and WebRTC instead of Parsec's native BUD protocol). Note this effectively means the web app connects to Windows hosts, that Parsec officially supports only Chrome/Chromium-based browsers, and that Parsec frames the macOS limitation as a present-time ("at this time") restriction rather than a permanent one.
