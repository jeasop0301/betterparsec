# BetterParsec benchmark truth gate

`analyze-benchmark.mjs`는 benchmark run을 비교 데이터셋에 넣기 전에 자동으로
`accepted` 또는 `rejected`로 판정한다. 단순 요약기가 아니라 잘못된 artifact 조합과
실험 조건 위반을 막는 truth gate다.

## 실행

```powershell
node ./tools/benchmark/analyze-benchmark.mjs `
  --run D:\betterparsec\benchmark-results\20260713-betterparsec-lan `
  --output D:\betterparsec\benchmark-results\20260713-betterparsec-lan\analysis.json
```

사람이 읽는 요약 대신 전체 JSON을 stdout으로 보려면 `--json`을 추가한다.

Exit code:

- `0`: accepted
- `2`: rejected
- `1`: 파일, JSON, CLI 등 실행 오류

## 판정 범위

다음 조건을 검사한다.

- run manifest가 `completed`인지
- browser export가 schema v2인지
- sample sequence가 정수·연속이고 elapsed time이 단조 증가하는지
- profile의 codec, 해상도, FPS, HDR 요청과 실제 export가 일치하는지
- manifest와 browser export의 시작·종료 시각이 같은 run envelope인지
- profile measurement window의 기본 95% 이상을 실제로 수집했는지
- ICE route, UDP/TCP protocol, selected candidate pair가 profile 조건과 일치하는지
- 측정 중 ICE pair transition이 없었는지
- transport/streamer telemetry freshness가 허용 범위인지
- sender queue, frame rejection, RTP write failure가 hard gate를 넘지 않는지
- browser decode p95, processing p95, FPS p50, jitter-buffer target/minimum p95가
  profile의 선택적 gate를 통과하는지
- packet loss와 freeze 조건을 통과하는지
- profile이 요구할 경우 shaping과 packet capture가 실제 enabled인지

다음은 경고로 남긴다.

- dirty repository
- shaping 미적용
- packet capture 미적용
- deterministic content trace SHA-256 부재
- freshness source 또는 artifact timestamp가 없어 검증 불가능한 경우

경고는 현재 run을 자동 거부하지 않지만 Parsec superiority 데이터셋에서는 별도 정책으로
거부하는 것이 좋다.

## profile validity gates

```json
{
  "validityGates": {
    "requireCleanRepository": true,
    "requireContentTraceHash": true,
    "requiredIceRoute": "direct",
    "requiredCandidateProtocol": "udp",
    "rejectIcePathTransitions": true,
    "maximumArtifactClockSkewMs": 5000,
    "minimumMeasuredDurationPercent": 95,
    "maximumSourceFreshnessMs": 2500,
    "maximumSenderQueueFrames": 2,
    "maximumHealthyLinkFrameRejectionPercent": 0.5,
    "requireZeroSenderWriteFailures": true,
    "minimumBrowserFpsP50": 55,
    "maximumBrowserDecodeP95Ms": 5,
    "maximumBrowserProcessingP95Ms": 5,
    "maximumJitterBufferTargetP95Ms": 50,
    "maximumJitterBufferMinimumP95Ms": 50,
    "requireZeroHealthyLinkFreezes": true,
    "maximumPacketLossPercent": 0.5,
    "requireNetworkTraceApplied": true,
    "requirePacketCapture": true
  }
}
```

모든 gate를 모든 network cell에 동일하게 적용하면 안 된다. 예를 들어 clean LAN의
zero-freeze/low-loss 기준과 impaired recovery cell의 기준은 달라야 한다. 각 profile은
그 cell이 증명하려는 것을 명시해야 한다.

## 지표 의미

분포는 sample별 interval metric에 nearest-rank p50/p95/p99를 적용한다. cumulative
counter는 browser가 제공한 delta를 우선 합산하고 delta가 없으면 마지막 cumulative
값을 사용한다.

- sender queue wait와 RTP write await는 내부 pipeline 지연이다.
- browser decode/processing/jitter buffer는 receiver-side 통계다.
- payload bitrate는 wire bitrate가 아니다.
- 이 analyzer는 input-to-photon을 계산하지 않는다.
- VMAF/SSIMULACRA2 같은 화질 점수와 PCAP wire bytes는 별도 artifact analyzer가
  추가되어야 한다.

## 테스트

```powershell
node --test ./tools/benchmark/analyze-benchmark.test.mjs
```

현재 회귀 테스트는 valid run, sequence gap, ICE mismatch, queue/write/decode/freeze
regression, browser artifact 누락, 다른 run의 artifact 혼합, 짧은 measurement window,
FPS/processing/jitter-buffer gate, strict clean-source/content-hash/PCAP gate, stale telemetry와
ICE transition을 다룬다.
