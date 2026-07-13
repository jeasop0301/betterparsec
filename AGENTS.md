# AGENTS.md — betterparsec

> 이 저장소에서 일하는 코딩 에이전트를 위한 **프로젝트 지침**이다. 짧게 유지한다 — 작업 방식·오케스트레이션은 [docs/agent/](docs/agent/)에 둔다.
> 프로젝트 배경·현재 상태는 [HANDOFF.md](HANDOFF.md)와 [BETTERPARSEC.md](BETTERPARSEC.md)에 있다. 결정·근거는 대화가 아니라 파일에 남긴다.

## 세션 시작

1. **[HANDOFF.md](HANDOFF.md) "지금 상태"를 먼저 읽는다** — 진행점·마지막 결정·미해결 리스크.
2. 건드릴 **seam 파일을 실제로 읽는다**(추측 금지). 이번 세션 = 한 논리 단위, 범위 밖은 손대지 않는다.
3. 세션 끝: HANDOFF "지금 상태" 갱신 + [docs/agent/worklog.md](docs/agent/worklog.md) 한 줄, 그리고 커밋(아래).

## 완료 게이트

- **"될 것 같다 / 아마" 금지.** 완료 주장은 **이번 세션의 실제 실행 결과**로만. 안 돌렸으면 "미검증"이라 명시한다.
- 코드를 바꿨으면 **빌드/테스트를 실제로 돌리고 출력을 본다** → 실패하면 고친 뒤 다시 돌린다.
- **성능·우열 주장은 2대 실측 전까지 금지** ([HANDOFF.md](HANDOFF.md) 리스크 #1). loopback·internal metric을 wire efficiency로 오인하지 않는다.
- **커밋 전 diff를 자기 리뷰한다.** 의도 밖 변경·디버그 잔여물·시크릿이 없는지 확인.

## 커밋 · git

- **논리 단위마다 커밋**, 그리고 **세션 종료 전 `commit` + `push origin betterparsec`.** 워크트리 작업도 커밋해 남긴다(백업 아님).
- 커밋 메시지: `betterparsec: <무엇을 왜>`.
- upstream 반영은 `git fetch upstream && git merge upstream/master`. 패치 의존성 revision 변경은 [docs/DEPENDENCIES.md](docs/DEPENDENCIES.md) 절차.

## 빌드 · 테스트 (근거는 실행으로)

- bootstrap: Windows `pwsh ./tools/bootstrap-dependencies.ps1`, 그 외 `bash tools/bootstrap-dependencies.sh`
- 테스트: `cargo test --workspace --locked`, `npm run test:stats`; 빌드 `npm run build`
- 툴체인: Rust nightly-2026-02-13, Node 24+, PowerShell 7. 상세는 [HANDOFF.md](HANDOFF.md).

---

_세션 절차·워크트리·서브에이전트·모델 등급 배분 등 "작업 방식"은 [docs/agent/](docs/agent/)에 있다 — 자동 로드 대상이 아니므로, 실제로 그 작업을 할 때만 연다._
