# AGENTS.md — betterparsec 작업 지침

> **이 파일은 DevSpace가 매 세션 자동 주입한다 (앞 32,000자).**
> ChatGPT는 Codex/Claude보다 컨텍스트가 작고 **세션 간 기억이 없다.**
> 따라서 이 파일 + [HANDOFF.md](HANDOFF.md) 가 유일한 "지속 기억"이다. 여기 없는 결정은 다음 세션엔 존재하지 않는다.
> 규칙을 어기면 조용히 넘어가지 말고 사용자에게 알린다.

---

## 0. 세션 시작 — 반드시 이 순서로

1. **[HANDOFF.md](HANDOFF.md) "지금 상태" 절을 먼저 읽는다.** 현재 진행점·마지막 결정·미해결 리스크가 거기 있다.
2. 건드릴 영역의 **seam 파일을 실제로 `read` 한다.** 파일명·시그니처·동작을 추측으로 쓰지 않는다. 크면 `grep_files`로 좁혀 읽는다.
3. **이번 세션 = 한 논리 단위.** 시작 전 할 일을 3~7줄 계획으로 적는다. 범위 밖은 손대지 않는다.

## 1. 컨텍스트 유지 (맥락 유실 방지)

ChatGPT는 대화가 길어지면 앞부분을 잊는다. 그러니 **기억을 대화가 아니라 파일에 남긴다.**

- **큰 작업은 쪼갠다.** 한 세션에서 끝낼 수 있는 단위로만 착수한다. 못 끝낼 규모면 먼저 계획만 HANDOFF/ROADMAP에 적고 분할한다.
- **세션 종료 전 반드시:** ① [HANDOFF.md](HANDOFF.md) "지금 상태" 갱신 ② [docs/agent/worklog.md](docs/agent/worklog.md)에 `날짜 — 한 줄 무엇을 했고 다음은 무엇` 추가 ③ commit + push (§4).
- **결정·근거·막힌 지점은 즉시 파일에 적는다.** "나중에 정리"는 다음 세션이 못 본다.
- 파일을 다 못 읽었으면 "다 안다"고 가정하지 않는다. **모르면 읽거나 물어본다.**

## 2. 서브에이전트가 없다 → 이렇게 대체한다

ChatGPT+DevSpace에는 네이티브 subagent(spawn/task)가 **없다.** 병렬·격리·전문화는 아래 세 수단으로 얻는다.

- **병렬·위험 실험 = 워크트리.** 본 체크아웃을 오염시킬 작업(리팩터, 의존성 bump, 위험한 패치)은 `open_workspace`로 **새 worktree**를 열어 거기서 한다. 워크트리는 **격리 수단이지 백업이 아니다** — 끝나면 반드시 커밋해 브랜치로 남긴다(§4).
- **반복 절차 = 플레이북(스킬 대용).** 커밋·벤치·ABR 변경처럼 반복되는 절차는 매번 즉흥으로 하지 말고 [docs/agent/](docs/agent/)의 플레이북을 **읽고 그대로** 따른다. 새 반복 절차가 생기면 플레이북을 하나 추가한다.
- **큰 과제 = 서브세션 브리핑.** 하나의 큰 과제는 대화 하나에 다 밀어넣지 말고, **좁게 스코프한 새 대화**를 연다. 새 대화 첫 메시지에 반드시: `AGENTS.md와 HANDOFF.md를 먼저 읽어라` + 그 세션의 목표 1개 + 건드릴 파일 목록. 이렇게 하면 각 대화가 ChatGPT 컨텍스트 한도 안에 머물고 서로 오염되지 않는다.

## 3. 완료 게이트 (품질)

- **"될 것 같다 / 아마" 금지.** 완료 주장은 **이번 세션의 tool 실행 결과**로만 한다. 실행 안 했으면 "미검증"이라고 명시한다.
- 코드를 바꿨으면 **빌드/테스트를 실제로 돌리고 출력을 본다** → 실패하면 고친 뒤 다시 돌린다. 정적으로 "잘 짜였다"는 검증이 아니다.
- **성능 주장은 2대 실측 전까지 금지** ([HANDOFF.md](HANDOFF.md) 리스크 #1). loopback 수치·internal metric을 wire efficiency/input-to-photon으로 오인하지 않는다.
- **커밋 전 `show_changes`로 diff를 자기 리뷰한다.** 의도 밖 변경·디버그 잔여물·시크릿이 없는지 확인한다.

## 4. 커밋 · git 규율

- **논리 단위마다 커밋.** 그리고 **세션 종료 전 반드시** `commit` + `push origin betterparsec`. 미커밋 작업은 워크트리 정리·리셋 한 번에 사라진다.
- 워크트리에서 한 작업도 반드시 커밋해 브랜치/원격으로 남긴다.
- 커밋 메시지 형식: `betterparsec: <무엇을 왜>` (예: `betterparsec: ABR up-slow 램프에 RTT 게이트 추가`).
- upstream 반영은 `git fetch upstream && git merge upstream/master`. 패치 의존성 revision 변경은 [docs/DEPENDENCIES.md](docs/DEPENDENCIES.md) 절차를 따른다.

## 5. 이 프로젝트 불변 규칙 (절대 위반 금지)

- **GPL-3.0-or-later.** 카피레프트. 배포 시 소스 공개 의무, 베이스 LICENSE·저작권 표기 유지.
- **TURN/coturn secret을 client artifact(웹 번들·로그·커밋)에 저장 금지.** 단기 credential만 서버가 발급한다.
- **`sent_unacknowledged`(client 관측)와 encoder-applied(host)를 혼동 금지.** 비트레이트가 "적용됐다"는 host log/ACK로만 확정한다.
- **AV1 YUV444는 이 RTX 4070 host에서 미지원** → 지원한다고 광고·구현 가정 금지.
- **규정 리스크:** 사지방 원격접속·필터 우회는 훈령 위반 소지(징계·계정삭제). **기술적 성공 ≠ 사용 허용.** 설계·실사용 전제는 규정 리스크 없는 환경(집 외부망 등).

## 6. 빌드 · 테스트 (근거는 실행으로만)

- bootstrap(패치 의존성): Windows `pwsh ./tools/bootstrap-dependencies.ps1`, 그 외 `bash tools/bootstrap-dependencies.sh`
- 테스트: `cargo test --workspace --locked`, `npm run test:stats`, 빌드 `npm run build`
- 툴체인: Rust nightly-2026-02-13, Node 24+, PowerShell 7. 상세는 [HANDOFF.md](HANDOFF.md) "빌드·실행·테스트", 의존성 정책은 [docs/DEPENDENCIES.md](docs/DEPENDENCIES.md).

---

_이 파일을 고치면 커밋한다 — 그래야 워크트리와 이후 clone에도 자동 반영된다._
