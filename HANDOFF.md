# HANDOFF — betterparsec

_이 문서 하나로 맥락을 이어받을 수 있게 쓴 인수인계서. 새 세션(GitHub Copilot / 새 Claude 등)에 이 파일을 그대로 붙여넣고 시작하면 된다._

## 이 프로젝트가 뭐냐

제약된 공용 네트워크(구체적으로 한국군 **사지방** 공용 PC)에서 **집 Windows PC(GPU)** 로 붙는 **저지연·저비트율 원격 데스크톱**. [moonlight-web-stream](https://github.com/MrCreativ3001/moonlight-web-stream)(GPL-3.0) **포크**로, Parsec엔 있고 오픈 스택엔 없던 3가지를 직접 얹는다: **적응형 비트레이트 · WARP 없이 접속 · AV1/화질**.

## 어쩌다 여기까지 왔나 (핵심 결론만)

3차 리서치([docs/research/](docs/research/))로 검증된 사실:
- **Parsec은 closed-source, SDK가 파생물 금지 → 포크 불가.** 반드시 오픈 스택(Sunshine/Moonlight) 위에 지어야 함. `[CONFIRMED]`
- 사용자의 실제 불만 = **화질 “뭉개짐”**. 주범은 코덱이 아니라 **WARP 전송(대역폭 ~50%↓, jitter, UDP차단시 TCP443 폴백) + 적응형 비트레이트 부재**. `[CONFIRMED]`
- **재페어링(사용자가 유일하게 번거로워한 것)은 이미 해결된 문제** — moonlight-web-stream이 페어링을 서버측 저장 → 브라우저는 로그인만. `[CONFIRMED]`
- **적응형 비트레이트를 커플링한 Sunshine-계열 프로젝트가 하나도 없음** `[CONFIRMED]` → 여기가 우리가 만들 진짜 novelty이자 뭉개짐의 근본 해법.
- 정직한 build-vs-buy: 화질만이면 Parsec Warp 유료($8.33–9.99/mo)가 이미 계정인증+4:4:4+BUD적응형을 줌. **DIY의 정당성 = 구독 회피 · 자가호스트 소유 · 무료 4:4:4/AV1 · WARP 의존 제거.** 사용자는 “직접 만든다”를 택함.

전체 근거·판정은 [docs/research/](docs/research/), 시각 요약은 [docs/design-memo.html](docs/design-memo.html).

## 지금 상태 (2026-07)

- ✅ `MrCreativ3001/moonlight-web-stream` 포크 완료. 브랜치 **`betterparsec`**, upstream 리모트 보존.
- ✅ 문서: [BETTERPARSEC.md](BETTERPARSEC.md), [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/ROADMAP.md](docs/ROADMAP.md), [docs/research/](docs/research/).
- ✅ Rust 워크스페이스 resolve 확인(macOS): `common` / `streamer` / `web-server` 2.10.0, nightly-2026-02-13.
- ⏳ **M0 진행 중** — 아직 풀빌드·실스트림 미검증. streamer는 moonlight-common-c(C) 의존이라 빌드 타깃은 사실상 **집 Windows 호스트**.

## 무엇을 만드나 — 3 features와 정확한 코드 seam

| # | 기능 | seam (file:line) | 요지 |
|---|------|------------------|------|
| 1 | **적응형 비트레이트** (flagship, 난이도 상) | `streamer/src/transport/webrtc/video.rs:159` — REMB 수신 후 `// Moonlight doesn't support dynamic bitrate changing :(` 로 버림. 비트레이트는 `streamer/src/main.rs:742`에서 시작 시 1회 설정. 상한 클램프 `common/src/lib.rs:29`. | REMB/TWCC → target 산출 → **Sunshine 호스트 런타임 비트레이트 변경 패치** 필요(C++). MVP는 client-driven, 목표는 GCC 커플링. |
| 2 | **WARP 없이 접속** (난이도 중) | `streamer/src/dynamic_ice_servers.rs` — 이미 `ice_server_script`로 동적 TURN cred 주입. ICE env는 `src/cli.rs:23`. | 공인 VPS coturn TURN-over-TLS(**TCP 443**) + relay-only ICE(`web/`에 `iceTransportPolicy:'relay'`). 기존 훅 재사용. |
| 3 | **AV1 / 화질** (난이도 중하, 이미 부분 배선) | `streamer/src/transport/webrtc/video.rs:21,28` — `MIME_TYPE_AV1`·`Av1Payloader` **import됨**. 협상 `streamer/src/main.rs:747`. | AV1 협상·HW디코드 검증·활성화 + HEVC 4:4:4 + `nvenc_vbv_increase` 튜닝. |

## 다음 스텝 (권장 순서)

로드맵 [docs/ROADMAP.md](docs/ROADMAP.md) 참조. 요약:

1. **M0 완료** — 집 Windows PC에 Sunshine 설치 + `build-windows.ps1`로 브리지 빌드·실행 → 브라우저 로그인 → 1회 페어링 → 스트림. **검증: wipe 후 재로그인만으로 재페어링 0.**
2. **M1 (WARP 없이 접속)** — coturn 배포 + `ice_server_script`(단기 TURN cred) + 클라 relay 토글. **검증: WARP off + UDP차단 환경에서 접속 + 릴레이 RTT/지터 실측** (TCP-릴레이 지연 실사용성이 최대 리스크).
3. **M2 (적응형)** — TWCC 활성 → AIMD/GCC 컨트롤러 → (MVP) client-driven 반영 → (목표) Sunshine 패치. **검증: `tc netem`으로 대역 조이며 target 추종 + frame drop 억제, Parsec과 A/B.**
4. **M3 (코덱/화질)** — AV1·4:4:4 검증·튜닝.

## 빌드 · 실행 · 테스트

- **개발(이 저장소, Mac/Linux 가능):** `web/` 프론트(TS, `npm run build` — 단 `generate-bindings`가 cargo 호출), 브리지 로직 편집·유닛테스트.
- **배포/실행(집 Windows PC = Sunshine 호스트):** `build-windows.ps1` 또는 [docker/](docker/). 브리지는 Sunshine과 같은 머신에서 돎.
- **툴체인:** Rust nightly-2026-02-13(rust-toolchain.toml 자동), Node 24+. 서브모듈: `git submodule update --init --recursive`.
- **upstream 갱신:** `git fetch upstream && git merge upstream/master`.

## 리스크 (순위)

1. **WARP-only + TCP-443 릴레이의 실사용 지연** — M1에서 최우선 실측. 안 되면 전송 재설계.
2. **적응형은 Sunshine(C++) 패치 필요** — 가장 무거운 작업. MVP(client-driven)로 먼저 증명.
3. **잠금 브라우저 HW 디코드 불가 → SW H.264** — `chrome://gpu` 확인, 폴백 유지.
4. **AI/실험 코드 인증 취약점** — 계정/토큰 코드 감사(ARCHITECTURE §보안 6항: 전필드 서명·짧은토큰+refresh·상수시간비교·replay·안전인코딩·revocation).

## ⚠️ 규정 리스크 (반드시 유지)

사지방 이용수칙은 원격접속·필터 우회를 명시 금지, 위반 시 징계·계정삭제 소지(국방부 훈령 「사이버지식정보방 운영 및 관리에 관한 훈령」). **기술적 성공 ≠ 사용 허용.** 실사용은 규정 리스크 없는 환경 기준으로.

## 라이선스

GPL-3.0-or-later (베이스 상속, 카피레프트). 배포 시 소스 공개 의무. 베이스 저작권·LICENSE 유지.
