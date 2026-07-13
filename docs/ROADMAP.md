# betterparsec — 로드맵

각 마일스톤은 **실행 가능한 산출물 + 검증 기준**을 갖는다. 순서: 먼저 계측을 믿을 수 있게 → 스트림이 돌게 → WARP 없이 붙게 → 뭉개짐을 잡게 → 화질을 올린다.

호스트: 집 Windows PC(GPU) + Sunshine. 개발/브리지: 이 리포. 프론트: `web/`.

---

## Browser-native product wedge

Moonlight 공식 FAQ는 브라우저가 raw TCP/UDP socket API를 제공하지 않아 순수 웹
클라이언트를 제공하지 않는다고 명시한다. 반면 Parsec은 Chromium 기반 웹 앱을
제공하지만, 공식 문서에서도 downloadable app보다 성능과 안정성이 낮고 네트워크 및
저수준 최적화 제어가 제한된다고 설명한다. 따라서 BetterParsec의 차별점은 단순히
"브라우저에서 열린다"가 아니라 다음의 측정 가능한 조합이어야 한다.

- 설치 없이 URL에서 첫 프레임까지 도달하고, 재방문 시 재페어링을 요구하지 않는다.
- selected ICE pair와 direct/relay, RTT, 후보 protocol을 기록해 접속 성공을 경로별로
  분리한다.
- 브라우저 decoder implementation과 CPU/GPU/power 증거를 함께 수집해 hardware decode를
  추정이 아니라 증명한다.
- 같은 실제 wire bitrate에서 화질, input-to-photon, freeze와 복구 시간을 native Parsec과
  Parsec 웹 앱 양쪽에 대해 별도 비교한다.
- Keyboard Lock, gamepad, audio/microphone, clipboard, secure-context 제약을 브라우저별
  지원표와 자동 smoke test로 관리한다.

제품 퍼널 gate는 connection success(직접/릴레이별), link-to-first-frame p50/p95,
재접속 성공률, 15분 disconnect/freeze, 입력 기능 성공률이다. 성능 gate는 아래 Parsec
hard gate를 그대로 사용한다. 이 둘을 통과하기 전에는 "Parsec보다 빠른 브라우저"라고
광고하지 않는다.

근거: Moonlight FAQ
<https://github.com/moonlight-stream/moonlight-docs/wiki/Frequently-Asked-Questions>,
Parsec web app 문서
<https://support.parsec.app/hc/en-us/articles/32381650129300-Use-the-Web-App-browser>.

---

## P0 — truth blockers (다른 최적화보다 먼저)

**목표:** 제품의 성능 수치와 코덱 capability가 실제 전송/호스트 동작을 정직하게
나타내게 한다. 아래 항목이 닫히기 전에는 Parsec 우위나 latency 개선을 주장하지 않는다.

- [x] Moonlight의 `presentationTimeUs`를 90 kHz 값으로 잘못 해석하던 RTP timestamp
  변환 수정. 60 fps timestamp 간격이 약 16,667 tick이 아니라 약 1,500 tick이 되도록
  단위 회귀 테스트 추가.
- [x] Moonlight의 0.1 ms 단위 `frameHostProcessingLatency`를 정수 나눗셈으로
  잘라내던 변환 수정. 100 us 해상도를 보존하는 회귀 테스트 추가.
- [x] 수정 후 same-PC H.264 smoke에서 browser RTP jitter가 약 168 ms에서 2-4 ms,
  jitter-buffer target이 약 11 ms, interval jitter-buffer delay가 0.03-0.04 ms로
  내려가고 loss/drop/freeze가 모두 0인지 확인.
- [ ] RTP timestamp를 packet capture로 교차 검증하고 host latency를 외부 marker/고속
  카메라 계측과 대조한다.
- [ ] AV1/HEVC/4:4:4 capability를 host encode, bridge transport, browser decode의
  3단 truth table로 만들고 실제로 끝까지 동작하는 조합만 광고한다.
- [x] encoder bitrate 요청의 client state를 `sent_unacknowledged`로 제한하고 Foundation
  host log와 대조해 실제 NVENC apply를 별도로 증명한다.
- [ ] request-id 기반 protocol ACK를 추가해 일반 run에서도 `queued`와 `applied`를
  자동 상관관계로 연결한다.
- [x] patched Moonlight dependency를 고정 revision + repository-owned patch + bootstrap/CI
  구조로 전환해 외부 로컬 작업 트리 없이 clean clone을 재현한다.

**검증:** 내부 수치가 packet capture/외부 계측과 각각 ±2 ms 또는 ±10% 안에서
일치해야 한다. 기존 2026-07-13 최초 same-PC run의 host latency와 RTP jitter는 단위
버그의 영향을 받았으므로 성능 baseline에서 제외하고 연결 안정성 smoke로만 보존한다.

## M0 — 빌드 & 스트림 베이스라인
**목표:** 포크가 그대로 빌드되고, 집 Sunshine ↔ 브라우저 스트림이 한 번 붙는다(WARP/변형 전).

- [x] `tools/bootstrap-dependencies.{ps1,sh}`로 pinned moonlight-common-rust/common-c clone+patch
- [x] Rust 브리지 빌드 (nightly-2026-02-13) 및 Windows C backend 확인
- [x] `web/` 프론트 빌드 (`npm ci && npm run build`)
- [x] Windows Sunshine + 브리지 실행, 브라우저 로그인/페어링/H.264 1080p60 스트림
- **검증:** 브라우저 상태 wipe(시크릿창/캐시삭제) 후 **재로그인만으로 재페어링 없이** 재연결. 스트림 프레임 표시.

## M1 — WARP 없이 접속 (feature #2)
**목표:** 제약망에서 WARP off로 붙는다.

- [ ] 공인 VPS에 coturn, TURN-over-TLS on **TCP 443**, `use-auth-secret`(HMAC TTL)
- [x] `ice_server_script` helper 작성 → 세션별 coturn REST 단기 credential 발급
- [x] `web/`에 `iceTransportPolicy:'relay'` 설정 경로 + 클라 ICE 구성
- [ ] 방화벽 규칙으로 "UDP 차단 + 443만 허용" 환경 재현
- **검증:** WARP off + UDP 차단 상태에서 접속 성공. 릴레이 **RTT/jitter/throughput 실측** → TCP-릴레이 지연 실사용성 판정(핵심 리스크). 실패 시 M2 이전에 전송 재설계.

## M2 — 적응형 비트레이트 (feature #1, flagship)
**목표:** 대역 급락 시 뭉개지는 대신 비트레이트가 따라 내려간다.

- [ ] TWCC(transport-cc) 피드백 활성 (SDP 협상 + webrtc-rs)
- [x] REMB + Receiver Report 손실 → bounded target_kbps (급락 즉시, 회복 완만)
- [x] WebRTC target → bridge apply task → moonlight-common-c `0x5506` sender
- [x] 10% hysteresis, 900ms 일반 제한, 20% 이상 하향은 즉시, 실패 시 baseline 미갱신
- [x] `DYNAMIC_BITRATE_V1` capability gate + stock Sunshine 기본 미지원 처리
- [x] Foundation Sunshine에 capability patch 적용 후 Windows host build/stage
- [x] staged Foundation으로 실제 BetterParsec paired stream을 연결하고 H.264 NVENC dynamic bitrate 적용 검증
- [x] benchmark runner가 Foundation start/host/stdout/stderr/cleanup log를 run artifact로 수집
- [ ] encoder request-id ACK로 client telemetry에서 applied 결과를 직접 확인
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

## Parsec 대비 hard gates

"항상 100% 더 좋다"는 환경과 콘텐츠를 한정하지 않으면 검증 불가능하다. 대신 아래
고정 조건의 **모든 hard gate를 100% 통과**해야 우위라고 판정한다. 하나의 가중 평균으로
실패 항목을 숨기지 않는다.

- 환경: clean LAN, typical WAN(30/3 ms, loss 0.1%, 20 Mbps), bad Wi-Fi
  (30/12 ms, 1% burst loss, 10 Mbps), congested WAN(50/8 ms, 2%, 5 Mbps),
  recovery(5%→0% loss 및 5→20 Mbps).
- 실험: 동일 host/client/trace에서 후보 순서를 무작위 ABBA로 배치하고 cell당 5회.
  bootstrap 신뢰구간 전체가 기준선을 넘어야 통과.
- 화질/효율: 동일 실제 wire bitrate에서 VMAF-NEG +2 이상, 또는 동일 화질에서 총
  양방향 wire bytes 20% 이상 감소. "훨씬 효율적"은 30% 이상 감소일 때만 사용.
- latency: 외부 1,000 fps 또는 GPIO+photodiode input-to-photon 기준 LAN median
  2 ms 이상, p95 5 ms 이상 개선하고 p99는 비열등.
- 네트워크: 총 양방향 wire bytes 20% 이상, static trace는 40% 이상 감소.
- 복원력: freeze/min 30% 이상, recovery p95 25% 이상 개선하고 15분 run disconnect 0.
- 안정성: connect/disconnect 100회 crash 0, 성공률 99% 이상, start p95 개선.
- 계측 진실성: 공개할 내부 metric은 외부 ground truth와 ±2 ms 또는 ±10% 이내.

우선순위는 P0 계측 진실성 → Foundation 동적 bitrate 실증 → wire/quality/latency 자동
벤치 → AV1 4:2:0·HEVC 4:4:4·HDR의 실제 end-to-end 지원 순이다. Parsec이 현재
제공하지 않는 기능은 좋은 wedge이지만, hard gate를 대신하지 않는다.

### 지금 위치
P0에서 발견한 두 단위 오류는 수정·회귀 테스트했고, Foundation paired H.264
1080p60 stream과 dynamic bitrate의 실제 NVENC 적용까지 확인했다. 75초 JSON에서
host 처리 p50/p95는 2.203/2.272 ms, streamer 처리는 1.263/1.552 ms, decode는
0.460/0.509 ms, WebRTC jitter는 1/1 ms였고 loss/drop/freeze/NACK는 모두 0이었다.
다만 이것은 loopback smoke이지 Parsec 비교 baseline이 아니다. 다음 병목은
**실제 write 성공량·full wire byte·ICE path를 포함한 계측 진실성과 deterministic
network shaping/Parsec ABBA 비교**다.

브라우저 계측은 selected ICE pair를 표준 `transport.selectedCandidatePairId`에서
video transport 우선으로 선택하고, 명확한 단일 fallback만 허용하도록 구현했다. 이제
candidate type/protocol, direct/relay, RTT, available outgoing/incoming bitrate를 매초
benchmark sample에 포함한다. 내보내기는 schema v2로 실행 시간, 보존 구간, 샘플
sequence, source freshness, 요청 stream 설정, 실제 codec/pipeline/HDR, sanitized page URL을
기록한다. 앱/호스트 build와 driver는 아직 자동 수집하지 않으므로 추측 대신
`unknown`으로 남긴다.

새 빌드의 실브라우저 smoke에서는 H.264 1920x1080/60 연결이 유지됐고,
host/UDP-to-host/UDP direct ICE pair와 0.00 ms loopback candidate-pair RTT가 확인됐다.
최종 구간의 track write는 87 packets 성공, 실패 0이었으며 queue wait 평균은
0.084 ms, write await 평균은 0.261 ms였다. schema-v2 export는 sequence 0-45의
46개 연속 샘플과 query가 제거된 page URL을 기록했다. 이는 browser-native 계측
경로의 기능 증명일 뿐 WAN 성능이나 Parsec 우위 증거는 아니다.

2026-07-13 로컬 호스트 확인: 실행 중인 설치본은 LizardByte Sunshine
`2026.516.143833` (`14ffa6fd`)이며 Foundation 빌드가 아니다. 로그상 NVENC
H.264/HEVC/AV1은 사용 가능하지만 YUV444 encode는 미지원이다. 따라서 이 설치본은
동적 bitrate capability와 HEVC 4:4:4 검증 대상이 될 수 없고, 현재 그대로 두면
bridge가 안전하게 초기 고정 bitrate를 유지하는 것이 정상 동작이다.

Foundation Sunshine capability patch의 build/stage는 성공했다. staged binary는 SHA
`b29f747b510f416db0d8075ed23b280e50ce3cbc531b766c828dedb9c2ee7dec`, version
`2026.0713.165229.杂鱼`이며, 설치된 stock 서비스를 유지한 채
alternate base port `49000`에서 startup도 확인했다. RTX 4070 encoder probe는 H.264,
HEVC, AV1을 통과했고 HEVC 10-bit YUV444도 성공했다. AV1 YUV444는 미지원으로
확인되어 그 경로는 광고하면 안 되며, fallback AV1 10-bit 4:2:0은 성공했다.

관리자 helper로 stock pairing bundle을 ACL 제한된 일회성 디렉터리에 복제하고,
Foundation을 stock 기본 포트에서 기존 host identity로 실행했다. BetterParsec 통합
controller 요청 3431→3787→4229→4814 Kbps 각각에 대해 Foundation 수신, capture-thread
apply, NVENC AVC 성공을 확인했다. 별도 host API 8000/15000 Kbps 요청은 FEC 20% 제외
후 6400/12000 Kbps로 적용됐지만 이것은 통합 shaping 증거가 아니다. 종료 후 stock
service, 기본 포트, apps/config/state/cert/key 해시가 모두 원상 복구됐음을 확인했다.

비용 효율 순서:

1. [x] sender write-success/failure/skip byte, queue wait/write latency, in-flight와 selected
   ICE candidate/RTT를 기록해 payload를 wire 사용량으로 오인하지 않게 한다.
2. [x] `tools/benchmark/`에 profile/trace, 20→8→15 Mbps SSH `tc netem`, pktmon PCAPNG,
   OS counters, browser JSON, Foundation host log를 한 run manifest로 묶는 runner와
   randomized ABBA plan generator를 구현하고 dry-run으로 검증한다.
3. [ ] 별도 host/client/router에서 첫 실제 run을 수행해 packet capture, OS byte,
   browser schema v2, host apply log의 시간축을 검증한다.
4. [ ] BetterParsec/Parsec Native를 cell당 5회 randomized ABBA로 실행하고 hard gate를
   통과한 뒤에만 HEVC 4:4:4, AV1 4:2:0, HDR10 확장을 제품 우위로 주장한다.
