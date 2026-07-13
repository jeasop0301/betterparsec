# HANDOFF — betterparsec

_이 문서 하나로 맥락을 이어받을 수 있게 쓴 인수인계서. 새 세션(GitHub Copilot / 새 Claude 등)에 이 파일을 그대로 붙여넣고 시작하면 된다._

## 🔧 세션 워크플로 규칙 (요약 — 전문은 [AGENTS.md](AGENTS.md))

ChatGPT/DevSpace 세션은 **컨텍스트가 작고 세션 간 기억이 없다.** 규칙은 [AGENTS.md](AGENTS.md)(DevSpace가 매 세션 자동 로드)가 authoritative이며, 핵심만:

1. **시작** — 이 "지금 상태" 절 → 건드릴 seam 파일을 실제로 `read` 후 착수. 추측 코딩 금지.
2. **한 세션 = 한 논리 단위.** 못 끝낼 규모면 먼저 쪼갠다. 큰 과제는 새 대화로 분리(서브세션, [docs/agent/session-playbook.md](docs/agent/session-playbook.md) C).
3. **검증** — 코드 바꾸면 빌드/테스트를 **실제로 돌려** 출력 확인. "될 것 같다" 금지. 성능 주장은 2대 실측 전까지 금지.
4. **커밋** — 논리 단위마다, **세션 종료 전 반드시** `commit` + `push origin betterparsec`. 워크트리 작업도 커밋해 남긴다(백업 아님).
5. **기록** — 종료 전 이 "지금 상태" 갱신 + [docs/agent/worklog.md](docs/agent/worklog.md) 한 줄. 결정은 대화가 아니라 파일에 남긴다.

반복 절차·워크트리·서브세션 사용법은 [docs/agent/session-playbook.md](docs/agent/session-playbook.md) 참조.

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
- ✅ Windows에서 Rust 전체 테스트 **161/161**, browser telemetry 테스트 **12/12**, `cargo build -p web-server -p streamer`, 실제 H.264 1080p60 브라우저 스트림 검증.
- ✅ Foundation Sunshine paired stream에서 BetterParsec ABR 요청 → host capture-thread → NVENC H.264 runtime bitrate apply를 실제 로그로 확인하고 stock 서비스·포트·config/state/cert/key hash 복구 검증.
- ✅ sender write success/failure/skip, queue wait, write latency, in-flight, selected ICE pair/RTT와 benchmark schema v2 구현.
- ✅ 외부 `../vendor-mlc-rust` 의존 제거. 고정 revision + `patches/` + `tools/bootstrap-dependencies.{ps1,sh}`로 clean clone/CI 재현 가능.
- ✅ `tools/benchmark/`에 run manifest, OS counters, pktmon PCAPNG, SSH `tc netem`, Foundation watchdog/restore, randomized ABBA plan 하네스 구축. dry-run 검증 완료.
- ⏳ 아직 별도 host/client 2대에서 deterministic trace와 Parsec Native/Web 비교를 실제 실행하지 않았다. 현재 결과는 기능·계측 smoke이며 superiority baseline이 아니다.

## 무엇을 만드나 — 3 features와 정확한 코드 seam

| # | 기능 | seam (file:line) | 요지 |
|---|------|------------------|------|
| 1 | **적응형 비트레이트** (flagship) | `streamer/src/abr.rs`, `streamer/src/bitrate_apply.rs`, `streamer/src/transport/webrtc/video.rs`, `streamer/src/main.rs` | REMB+RR loss → bounded down-fast/up-slow target → capability-gated encrypted `0x5506`. Foundation NVENC apply는 host log로 실증했지만 protocol ACK는 아직 없다. |
| 2 | **WARP 없이 접속** | `streamer/src/dynamic_ice_servers.rs`, `common/src/turn.rs`, `src/bin/ice_servers.rs`, `web/stream/transport/webrtc.ts` | coturn REST 단기 credential과 relay policy 경로를 구현. 공인 VPS TCP/TLS 443 실제 제약망 실측은 남아 있다. |
| 3 | **AV1 / 화질** | `streamer/src/transport/webrtc/video.rs`, `web/stream/video.ts` | host probe에서 H.264/HEVC/AV1, HEVC 10-bit YUV444, AV1 10-bit 4:2:0을 확인. AV1 YUV444는 이 RTX 4070 host에서 미지원이므로 광고 금지. |

## 다음 스텝 (권장 순서)

로드맵 [docs/ROADMAP.md](docs/ROADMAP.md) 참조. 요약:

1. **2대 실측 첫 run** — `tools/benchmark/Run-Benchmark.ps1`로 동일 content trace, 20→8→15 Mbps shaping, pktmon/OS bytes, browser JSON, Foundation host log를 한 run manifest에 수집.
2. **Parsec randomized ABBA** — `New-AbbaPlan.ps1`의 recorded seed/order로 BetterParsec vs Parsec Native를 cell당 5회 실행하고 invalid ICE/codec/run을 reject.
3. **계측 truth gate** — PCAP RTP cadence와 payload→candidate-pair→OS byte 차이를 계산하고 host latency를 외부 marker와 대조. `sent_unacknowledged`와 encoder applied를 계속 분리.
4. **비교 결과가 hard gate를 통과한 뒤** TWCC/GCC 정교화, HEVC 4:4:4, AV1 4:2:0, HDR10 순으로 확장.
5. **별도 제품 gate** — coturn TLS/TCP 443을 실제 UDP 차단 환경에서 direct/relay cell로 측정.

## 빌드 · 실행 · 테스트

- **patched dependency bootstrap:** Windows `pwsh ./tools/bootstrap-dependencies.ps1`, Linux/macOS/WSL/Git Bash `bash tools/bootstrap-dependencies.sh`. 정확한 revision과 patch 정책은 [docs/DEPENDENCIES.md](docs/DEPENDENCIES.md).
- **개발:** `cargo test --workspace --locked`, `npm run test:stats`, `npm run build`. `build-windows.ps1`은 bootstrap을 자동 실행한다.
- **배포/실행(Windows Sunshine host):** `build-windows.ps1` 또는 [docker/](docker/). 브리지는 현재 Sunshine과 같은 머신 배치를 전제로 한다.
- **벤치:** [tools/benchmark/README.md](tools/benchmark/README.md). 결과는 ignored `benchmark-results/<run-id>/`.
- **툴체인:** Rust nightly-2026-02-13, Node 24+, PowerShell 7. shaping은 별도 Linux router의 `tc netem` 사용.
- **upstream 갱신:** base repo는 `git fetch upstream && git merge upstream/master`; patched dependency revision 변경은 DEPENDENCIES 절차를 따른다.

## 리스크 (순위)

1. **2대 실측 전까지 성능 주장을 할 수 없음** — loopback 수치와 internal metric을 input-to-photon/wire efficiency로 오인하지 않는다.
2. **WARP-only + TCP/TLS-443 relay 지연** — 실제 UDP 차단 환경에서 별도 cell로 검증. 안 되면 전송 재설계.
3. **동적 bitrate protocol ACK 부재** — client는 `sent_unacknowledged`만 알 수 있고 encoder apply는 host log/ACK patch가 필요하다.
4. **브라우저 HW decode 증명** — capability preference가 아니라 decoder implementation, CPU/GPU/power 증거로 확인하며 폴백 유지.
5. **인증/배포 보안** — 계정/토큰과 forwarded-header trust boundary를 계속 감사하고 TURN secret을 client artifact에 저장하지 않는다.

## ⚠️ 규정 리스크 (반드시 유지)

사지방 이용수칙은 원격접속·필터 우회를 명시 금지, 위반 시 징계·계정삭제 소지(국방부 훈령 「사이버지식정보방 운영 및 관리에 관한 훈령」). **기술적 성공 ≠ 사용 허용.** 실사용은 규정 리스크 없는 환경 기준으로.

## 라이선스

GPL-3.0-or-later (베이스 상속, 카피레프트). 배포 시 소스 공개 의무. 베이스 저작권·LICENSE 유지.
