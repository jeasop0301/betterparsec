# betterparsec — 로드맵

각 마일스톤은 **실행 가능한 산출물 + 검증 기준**을 갖는다. 순서: 먼저 스트림이 돌게 → WARP 없이 붙게 → 뭉개짐을 잡게 → 화질을 올린다.

호스트: 집 Windows PC(GPU) + Sunshine. 개발/브리지: 이 리포. 프론트: `web/`.

---

## M0 — 빌드 & 스트림 베이스라인
**목표:** 포크가 그대로 빌드되고, 집 Sunshine ↔ 브라우저 스트림이 한 번 붙는다(WARP/변형 전).

- [ ] 서브모듈 init (`git submodule update --init --recursive`) — moonlight-common-c
- [ ] Rust 브리지 빌드 (nightly-2026-02-13 자동). streamer의 C 의존(moonlight-common-c) 플랫폼 확인
- [ ] `web/` 프론트 빌드 (`npm ci && npm run build`)
- [ ] 집 PC에 Sunshine 설치 + 브리지 실행, 브라우저에서 계정 로그인 → 호스트 1회 페어링 → 스트림
- **검증:** 브라우저 상태 wipe(시크릿창/캐시삭제) 후 **재로그인만으로 재페어링 없이** 재연결. 스트림 프레임 표시.

## M1 — WARP 없이 접속 (feature #2)
**목표:** 제약망에서 WARP off로 붙는다.

- [ ] 공인 VPS에 coturn, TURN-over-TLS on **TCP 443**, `use-auth-secret`(HMAC TTL)
- [ ] `ice_server_script` 작성 → 세션별 단기 TURN cred 발급 (`streamer/src/dynamic_ice_servers.rs` 훅)
- [ ] `web/`에 `iceTransportPolicy:'relay'` 토글 + 클라 ICE 구성
- [ ] 방화벽 규칙으로 "UDP 차단 + 443만 허용" 환경 재현
- **검증:** WARP off + UDP 차단 상태에서 접속 성공. 릴레이 **RTT/jitter/throughput 실측** → TCP-릴레이 지연 실사용성 판정(핵심 리스크). 실패 시 M2 이전에 전송 재설계.

## M2 — 적응형 비트레이트 (feature #1, flagship)
**목표:** 대역 급락 시 뭉개지는 대신 비트레이트가 따라 내려간다.

- [ ] TWCC(transport-cc) 피드백 활성 (SDP 협상 + webrtc-rs)
- [ ] AIMD/GCC 컨트롤러: REMB/TWCC → target_kbps 평활 (latency>fps>quality), `maximum_bitrate_kbps` 상한 준수
- [ ] **경로 B(MVP)**: client-driven 런타임 target → 브리지가 최소 hitch로 반영
- [ ] **경로 A(목표)**: Sunshine 런타임 비트레이트 제어 패치 + `moonlight-common-rust` sender → `video.rs:159` seam에서 실제 반영
- **검증:** 호스트에서 `tc netem`으로 대역/지터/손실 주입 → target 그래프 추종 + frame drop 억제. 동일 조건 Parsec과 뭉개짐 A/B.

## M3 — 코덱 / 화질 (feature #3)
**목표:** 같은 비트레이트에서 더 선명.

- [ ] AV1 협상·HW 디코드 실동작 검증(`chrome://gpu`), 안 되면 HEVC/H.264 폴백 유지
- [ ] HEVC 4:4:4 협상 경로 노출(색 텍스트 fringing 제거)
- [ ] `nvenc_vbv_increase` 등 rate-control 튜닝(모션 스파이크)
- **검증:** 동일 delivered bitrate에서 AV1 4:2:0 / HEVC 4:2:0 / HEVC 4:4:4 정지·모션·색텍스트 채점.

## M4 — (선택) 하드닝
- [ ] 보안 감사(ARCHITECTURE §보안 6항: 서명·짧은토큰·상수시간·replay·안전인코딩·revocation)
- [ ] 입력(Gamepad/Keyboard Lock) secure-context 동작, 오디오, 재접속 안정성
- [ ] upstream 병합 전략 정리

---

### 지금 위치
M0 진행 중 — 포크 완료, 문서 작성 완료. 다음: 서브모듈 init + 빌드 베이스라인.
