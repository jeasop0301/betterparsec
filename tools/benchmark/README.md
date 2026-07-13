# BetterParsec benchmark harness

이 디렉터리는 BetterParsec, Parsec Web, Parsec Native를 같은 조건에서 비교하기 위한
반복 실행 하네스다. 측정 결과는 기본적으로 저장소 루트의 `benchmark-results/`에
run 단위로 저장되며 Git에는 포함되지 않는다.

## 무엇을 자동화하나

`Run-Benchmark.ps1`은 한 번의 실험 run에 대해 다음을 묶는다.

- profile과 deterministic network trace 사본
- Git SHA, dirty 상태, OS/CPU/GPU/driver/toolchain 정보
- Windows 네트워크 어댑터 byte/packet counter 전후값과 delta
- Windows `pktmon` ETL 및 변환된 PCAPNG
- SSH를 통한 Linux router `tc netem` phase 적용과 정확한 event timeline
- 선택적으로 staged Foundation Sunshine 전환, watchdog 복구, stock 파일 hash 검증
- 선택적으로 BetterParsec web server 등 실행 파일의 stdout/stderr와 SHA-256
- BetterParsec browser benchmark JSON 사본
- 성공/실패 상태와 모든 artifact 경로를 포함한 `manifest.json`

이 하네스는 브라우저나 Parsec UI를 임의로 클릭하지 않는다. 후보 프로그램과 동일
content trace를 준비한 뒤 Enter를 누르는 지점은 의도적으로 사람 확인 단계로 남겨
잘못된 앱, 코덱, ICE 경로를 측정하는 것을 막는다.

## 요구 사항

- Windows PowerShell 7 (`pwsh`)
- 패킷 캡처나 Foundation 교체 시 관리자 PowerShell
- shaping 사용 시 `tc`가 있는 Linux router와 SSH 접속
- router에서 지정 인터페이스에 대해 passwordless `sudo tc` 권한
- 모든 후보에서 동일하게 재생할 deterministic content trace

두 인터페이스에 같은 trace를 적용하면 router 양방향 경로를 대칭 shaping할 수 있다.
인터페이스 하나만 지정하면 해당 egress 방향만 shaping된다.

## 먼저 dry-run

아무 서비스, router, packet capture도 건드리지 않고 manifest 생성과 명령을 검사한다.

```powershell
pwsh ./tools/benchmark/Run-Benchmark.ps1 `
  -Candidate BetterParsec `
  -SkipPacketCapture `
  -SkipNetworkTrace `
  -NoPrompt `
  -DryRun
```

20→8→15 Mbps trace 명령까지 확인하려면:

```powershell
pwsh ./tools/benchmark/Run-Benchmark.ps1 `
  -Candidate BetterParsec `
  -RouterSshTarget bench@192.168.1.2 `
  -RouterInterfaces eth0,eth1 `
  -SkipPacketCapture `
  -NoPrompt `
  -DryRun
```

## 실제 BetterParsec run

아래 예시는 web server를 실행하고, Linux router에 20→8→15 Mbps trace를 적용하고,
`pktmon`으로 캡처한다. `BrowserExportPath`는 브라우저의 **Export Benchmark** 저장
경로와 같아야 한다. 측정 구간이 끝나면 shaping과 packet capture를 먼저 종료한 뒤
JSON export를 기다리므로 export UI 트래픽이 측정 PCAP에 섞이지 않는다.

```powershell
pwsh ./tools/benchmark/Run-Benchmark.ps1 `
  -Candidate BetterParsec `
  -ApplicationPath ./target/debug/web-server.exe `
  -ApplicationArguments '--config-path','./server/config.json' `
  -RouterSshTarget bench@192.168.1.2 `
  -RouterInterfaces eth0,eth1 `
  -ContentTracePath D:\bench\deterministic-motion-v1.mp4 `
  -BrowserExportPath D:\bench\browser-export.json
```

Foundation Sunshine을 실제 기본 포트에서 사용하려면 관리자 PowerShell에서 다음을
추가한다.

```powershell
-UseFoundation -FoundationStageRoot D:\foundation-sunshine\stage
```

Foundation helper는 stock config/state/certificate/key hash를 기록하고, staged binary만
종료한 뒤 stock `SunshineService`와 포트 47989 소유권 및 파일 hash 복구를 검증한다.
10분 watchdog도 별도로 stock 서비스를 복구한다.

## Parsec run

Parsec은 UI에서 직접 실행해도 된다. `ApplicationPath`를 생략하면 runner는 측정과
artifact 수집만 담당한다.

```powershell
pwsh ./tools/benchmark/Run-Benchmark.ps1 `
  -Candidate ParsecNative `
  -RouterSshTarget bench@192.168.1.2 `
  -RouterInterfaces eth0,eth1 `
  -ContentTracePath D:\bench\deterministic-motion-v1.mp4
```

## randomized ABBA plan

각 cell/repetition에서 ABBA 또는 BAAB를 seed 기반으로 무작위 선택한다.

```powershell
pwsh ./tools/benchmark/New-AbbaPlan.ps1 `
  -CandidateA BetterParsec `
  -CandidateB ParsecNative `
  -Repetitions 5 `
  -Seed 20260713
```

출력 plan에는 각 run의 순번, cell, repetition, candidate, sequence position이 들어간다.
각 run의 `runId`를 `Run-Benchmark.ps1 -RunId`에 넘겨 결과와 plan을 연결한다.

## 결과 디렉터리

```text
benchmark-results/<run-id>/
  manifest.json
  profile.json
  network-trace.json
  system.json
  network-before.json
  network-after.json
  network-delta.json
  network-trace-events.json
  browser-benchmark.json          # 제공된 경우
  capture/
    wire.etl
    wire.pcapng
  logs/
    application.stdout.log
    application.stderr.log
    foundation-start.json
    foundation-cleanup.json
    foundation.sunshine.log
```

## 해석 제한

- `network-delta.json`은 어댑터 전체 counter다. 백그라운드 트래픽이 섞일 수 있다.
- PCAPNG는 동일 capture point에서 후보 간 비교할 때 가장 유용하다. Ethernet preamble,
  inter-frame gap 등 모든 물리계층 byte를 뜻하지 않는다.
- BetterParsec `track.write` 성공 byte는 여전히 RTP payload 경계이며 PCAP byte가 아니다.
- browser export의 ICE route, codec, sequence 연속성, freshness가 profile과 다르면 run을
  reject해야 한다.
- input-to-photon은 이 하네스만으로 측정되지 않는다. 고속 카메라 또는 GPIO+photodiode
  artifact를 같은 run 디렉터리에 추가해야 한다.
