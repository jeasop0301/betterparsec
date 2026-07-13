# 세션 플레이북 (스킬 대용)

> [AGENTS.md](../../AGENTS.md)가 가리키는 **재사용 절차**. 반복 작업은 매번 즉흥으로 하지 말고 여기 해당 절차를 **읽고 그대로** 따른다.
> **B·C(워크트리·서브세션)는 네이티브 서브에이전트가 없는 호스트(ChatGPT/DevSpace 등)의 대체 수단이다.** 서브에이전트가 있는 호스트(Claude Code 등)는 그걸 직접 쓰고 B·C는 격리가 필요할 때만 참고한다.
> 오케스트레이션 세부와 **모델 등급 배분**(어떤 작업에 어떤 급 모델)은 [model-tiering.md](model-tiering.md) 참조.
> `DEVSPACE_SKILLS`는 기본 활성이므로, 이 폴더의 절차는 나중에 `SKILL.md` 형식으로 승격해 네이티브 스킬로 만들 수 있다.

---

## A. 코딩 세션 절차 (모든 세션 공통)

1. **읽기** — [HANDOFF.md](../../HANDOFF.md) "지금 상태" → 건드릴 seam 파일 `read`(크면 `grep_files`).
2. **계획** — 목표 1개 + 파일 목록 + 3~7줄 단계. 범위 밖은 안 건드린다.
3. **작업** — 작은 단위로. 한 파일 바꾸면 관련 테스트를 머릿속이 아니라 실제로 돌릴 준비.
4. **검증** — `cargo test --workspace --locked` / `npm run test:stats` 등 **실행하고 출력 확인**. 실패 → 고치고 재실행. "될 것 같다" 금지.
5. **자기 리뷰** — `show_changes`로 diff 확인: 의도 밖 변경·디버그 잔여물·시크릿 없나.
6. **기록** — [HANDOFF.md](../../HANDOFF.md) "지금 상태" 갱신 + [worklog.md](worklog.md) 한 줄 추가.
7. **커밋** — `betterparsec: <무엇을 왜>` 로 commit → `push origin betterparsec`.

## B. 위험/병렬 작업 → 워크트리 절차

리팩터·의존성 bump·위험 패치처럼 본 체크아웃을 깨뜨릴 수 있는 작업:

1. `open_workspace`로 **새 worktree**를 연다 (본 체크아웃 오염 방지).
2. 거기서 A 절차대로 작업·검증.
3. **끝나면 반드시 커밋해 브랜치로 남긴다.** 워크트리는 백업이 아니다 — 커밋 안 하면 정리 시 소멸.
4. 검증 통과분만 `betterparsec` 브랜치에 반영. 실패 실험은 브랜치째 버린다.

## C. 큰 과제 → 서브세션 브리핑 (subagent 대용)

과제가 한 대화에 담기엔 크면, 좁게 스코프한 **새 대화**를 연다. 새 대화 첫 메시지 템플릿:

```
AGENTS.md와 HANDOFF.md를 먼저 읽어라.
목표(이 세션 하나만): <한 문장>
건드릴 파일: <경로 목록>
하지 말 것: <범위 밖 명시>
끝나면: 테스트 실행 결과 + HANDOFF/worklog 갱신 + 커밋.
```

이렇게 하면 각 대화가 컨텍스트 한도 안에 머물고, 세션끼리 서로 오염되지 않는다.

## D. ABR / 프로토콜 변경 체크리스트 (회귀 위험 높음)

`streamer/src/abr.rs`, `bitrate_apply.rs`, `transport/webrtc/video.rs` 등을 건드릴 때:

- [ ] `sent_unacknowledged`(client)와 encoder-applied(host)를 **분리해서** 다뤘나
- [ ] 비트레이트 "적용됨" 주장을 host log/ACK로 **실증**했나 (client 관측만으로 단정 금지)
- [ ] down-fast / up-slow 경계와 capability gate가 그대로인가
- [ ] 성능/우열 주장을 넣지 않았나 (2대 실측 전까지 금지)
- [ ] TURN secret이 client 아티팩트로 새지 않나
