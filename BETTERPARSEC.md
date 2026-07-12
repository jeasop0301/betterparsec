# betterparsec

**저지연·저비트율 원격 데스크톱.** [moonlight-web-stream](https://github.com/MrCreativ3001/moonlight-web-stream)(GPL-3.0) 포크 위에, Parsec에는 있고 오픈 스택엔 없던 세 가지 — **적응형 비트레이트 · WARP 없이 접속 · AV1/화질** — 을 직접 얹는다.

> 목표 시나리오: 제약된 공용 네트워크(예: 사지방)에서 집 Windows PC(GPU)로, **무설치 브라우저 + 1회 로그인(재페어링 없음)** 으로 접속하고, **낮은 비트율에서도 뭉개지지 않는 화질**을 얻는다.

---

## 왜 포크인가

베이스(moonlight-web-stream)가 이미 **지루한 80%** 를 해결한다 — 우리가 다시 짜지 않는다:

- 브라우저 클라이언트(WebRTC + WebSocket/WebCodecs 폴백)
- **서버측 페어링** → 클라 상태가 초기화돼도 재페어링 없음 (`src/app/host.rs`, `src/app/storage/`)
- **계정 인증**(user / role / password, per-role 권한) (`src/app/auth.rs`, `role.rs`)
- Sunshine(GameStream) ↔ 브라우저 브리지 (`streamer/`)

betterparsec = 여기에 **어려운 20%** 를 추가한다.

## 우리가 만드는 것 (3 features · 실제 코드 seam)

| # | 기능 | 베이스 seam (file:line) | 난이도 | 왜 |
|---|------|------------------------|--------|----|
| 1 | **적응형 비트레이트** (간판) | `streamer/src/transport/webrtc/video.rs:159` — REMB 수신 후 `// Moonlight doesn't support dynamic bitrate changing :(` 로 버림. 비트레이트는 `streamer/src/main.rs:742`에서 스트림 시작 시 1회만 설정 | 상 (호스트 패치) | jittery 링크의 "뭉개짐"을 잡는 유일한 근본 레버. Parsec BUD의 핵심 |
| 2 | **WARP 없이 접속** | `streamer/src/dynamic_ice_servers.rs` (이미 `ice_server_script`로 동적 TURN cred 주입) + `src/cli.rs:23` (ICE env) | 중 | coturn TURN-over-TLS(443) + relay-only ICE → 제약망에서 브라우저가 집에 직접 도달. 기존 훅 재사용 |
| 3 | **AV1 / 화질** | `streamer/src/transport/webrtc/video.rs:21,28` — `MIME_TYPE_AV1`·`Av1Payloader` **이미 import됨**. 협상은 `streamer/src/main.rs:747` | 중하 | 이미 부분 배선 → "검증·활성화·튜닝"에 가까움. + HEVC 4:4:4 |

## 정직한 스코프

- **이건 수 주짜리다.** 특히 #1 적응형은 브리지만으로 안 되고 **Sunshine 호스트(C++) + moonlight-common-rust**까지 패치해 런타임 비트레이트 변경을 넣어야 한다.
- **단기 목표는 "Parsec BUD를 이긴다"가 아니다.** MVP → 점진 개선. 우선 REMB/TWCC 신호로 client-driven 비트레이트 조정부터, 이후 진짜 congestion-coupled 제어.
- **end-to-end 검증엔 GPU 호스트가 필요.** 개발 PC(macOS)에선 브리지/프론트 빌드·유닛테스트, 실 스트림은 집 Windows+Sunshine에서.

## 라이선스 · 출처

- **GPL-3.0-or-later** (베이스에서 상속, 카피레프트). 배포 시 소스 공개 의무.
- upstream: `git remote get-url upstream` → MrCreativ3001/moonlight-web-stream. 갱신은 `git fetch upstream && git merge upstream/master`.
- 베이스 LICENSE / 저작자 표기는 그대로 유지한다.

## 문서

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — 3개 기능의 설계와 코드 seam 상세
- [docs/ROADMAP.md](docs/ROADMAP.md) — 마일스톤 M0–M3, 작업·검증 방법
- 베이스 사용법: [README.md](README.md)

## ⚠️ 규정 리스크

사지방 이용수칙은 원격접속·필터 우회를 명시 금지하며 위반 시 징계·계정삭제 소지가 있다(국방부 훈령 「사이버지식정보방 운영 및 관리에 관한 훈령」). **기술적 성공 ≠ 사용 허용.** 실사용은 규정 리스크 없는 환경(집 외부망 등)을 기준으로 설계·사용한다.
