# 라이브 스모크 체크리스트 — 2026-07-16 빌드 (한 세션 최대 검증)

**배포**: `https://betterparsec.kje12e4.workers.dev` (비번 게이트 다운로드 — R2). **실행 후
타이틀바가 `build 07-16f (kb-capture + 8M default + file log)`인지 먼저 확인** — 아니면 구버전.
**로그**: `run-incheon.bat` 실행 시 `RUST_LOG=info,betterparsec::input=debug` 자동. 스톨/이상 시 콘솔 캡처.

## ⚠️ 먼저 알 것
- **07-16f: 기본 비트레이트를 검증된 8 Mbps로 회귀**(Medium 1080p60/8M — 20M 기본이
  30초 내 끊김의 유력 원인이라 라이브 A/B 전까지 미검증 상향 금지. Fast=5M, Quality=25M).
- **07-16f: exe 옆 `betterparsec.log` 상시 기록**(append) — 끊김/스톨 나면 그 파일 통째로 확보.
- **07-16f: immersive Alt+Tab 수리** — 훅 게이트가 마우스 relative에 걸려 있어 호스트 커서가
  보이는 동안(데스크톱) Alt+Tab이 클라에서 먹던 버그 → immersive 세션 전체로 게이트 교체.
- **접속시점 반영**(바꾸면 재접속 필요): 모드/비트레이트/해상도/fps · 10-bit · Exclusive audio.
- **라이브 토글**(세션 중 즉시): sharpen 슬라이더 · NV12 · Immersive · client cursor.
- 설정은 `%APPDATA%/betterparsec/settings.json`에 자동 저장 — 앱 재시작에도 유지.

## 세션 1 — 기본값 (Medium) : 가장 많은 항목
1. [ ] **제로컨피그**: zip 풀고 exe 더블클릭 → 폼 프리필 → 비번만 → 접속
2. [ ] **픽처/색/체감지연** 정상 (Medium 1080p/20M)
3. [ ] **present 히칭 소멸**: 주기적 뚝뚝(~0.3s 단위) 사라졌는지 몇 분 관찰
4. [ ] **한글 IME**: 호스트 메모장에 한글 입력 (호스트 조합 정상)
5. [ ] **sharpen 슬라이더** 라이브 A/B (0 ↔ 50~100)
6. [ ] **NV12 fast present** 라이브 A/B (색 동일 + 부하/지연 체감)
7. [ ] **host-authority 커서**: 게임=숨김+상대, 메뉴/데스크톱=표시+절대 자동 전환
8. [ ] **Immersive 진입** → 게임 → **Alt+Tab이 호스트로 전달**(작업전환창 호스트에 뜸)
9. [ ] **Immersive 탈출 ① 버튼** → 탈출 직후 호스트에서 타이핑 → **Alt/Ctrl/Shift 스턱 없음**
10. [ ] **Immersive 재진입 → 탈출 ② Ctrl+Alt+Shift+Q** → 다시 스턱 없음 확인
11. [ ] (가능하면) immersive 중 UAC/시스템 팝업 유발 → **자동 탈출 + 커서 언클립** 확인
12. [ ] **Ctrl+Alt+` 하드 디스커넥트 (Parsec식)**: immersive 풀스크린에서 Ctrl+Alt+` →
    **즉시 세션 종료 + 풀스크린 해제 + connect 화면 복귀**, 호스트에 Ctrl/Alt 스턱 없음
    (윈도우 모드에서도 스트림 클릭 후 동일 동작 — 훅 없이도 작동하는 이중 경로)

## 세션 2 — Exclusive audio ON (사이드바 체크 → 재접속)
13. [ ] 오디오 체감 지연 A/B (세션 1 대비 — 목표 ~200ms→~50ms 이하 체감)
14. [ ] 오디오 안 죽음(포맷 미지원이면 자동 shared 폴백 — 콘솔에 fallback 로그)

## 세션 3 — 10-bit present ON (접속화면 체크 → 접속)
15. [ ] 화면 정상(무회귀 — 호스트가 8-bit라 화질이득은 아직 없음; 검증 = 깨짐/블랙스크린 없음)
16. [ ] 미지원 GPU였다면 콘솔에 R8 폴백 로그 + 정상 렌더

## 세션 4 — 모드 셀렉터 실효 (P1 픽스 검증)
17. [ ] **Fast** 선택 → 재접속 → 720p/5M로 실제 변경(사이드바 stats/체감)
18. [ ] (선택) **Quality** → 4K/25M 요청 — 호스트/망이 못 받으면 동작 관찰만
19. [ ] 앱 종료 → 재실행 → **설정 유지** 확인(모드/토글)

## 옵션 (호스트 설정 가능할 때)
- [ ] **서라운드**: 호스트 Windows 5.1 + Sunshine surround 설정 → 오디오 정상(5.1→stereo 다운믹스, 콘솔에 decoder reopen 로그)
- [ ] **클라 커서**: 호스트 `capture_cursor=false` 세션에서 "Client-rendered cursor" 토글 → 단일 커서 + 모양 일치
- [ ] **스톨 재현 시**: 콘솔 로그 전체 캡처 + 직전 행동 메모 (근원 추적 재료)

## 판정 기록
각 항목 PASS/FAIL/미시도 + 특이사항 한 줄이면 충분. FAIL은 콘솔 로그 조각 붙여주면 바로 잡는다.
