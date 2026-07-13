# 리서치 5 — 네이티브 HW-디코드 클라이언트 + "Parsec×GFN×Moonlight×DCV 장점 결합" 판정 (2026-07)

> 질문: HW 디코드 데스크탑 앱으로, 4개 제품의 장점을 다 합치고 개선까지 더한 "미친 성능" 앱이 가능한가?
> 무엇을 각 제품에서 받아오나? DCV의 인상적인 웹 작업 품질의 정체는?

_subagent 2트랙(DCV teardown / 네이티브 클라 스택) + 리서치 4 종합. 2026-07-14._

---

## 0. 한 줄 결론

**가능하다. 네 제품의 강점이 서로 다른 레이어에 있어서 충돌 없이 합성된다.**
Parsec=전송+입력, Moonlight/Sunshine=코덱 스택(이미 우리 베이스), GFN=클라 프레젠트/업스케일 아이디어,
DCV=데스크톱 화질 레이어(dirty-region 타일 + build-to-lossless + 클라 커서). 네이티브 클라이언트 결정으로
리서치 4의 최대 제약 2개(브라우저 컴포지터 +4–8ms, HEVC 4:4:4 SW 디코드)가 소멸한다.
게다가 **아무 경쟁사도 출하하지 않은 3개 무기**(VRR/tearing 프레젠트, 서브프레임 슬라이스,
WASAPI exclusive 오디오)가 네이티브에서 열린다.

---

## 1. 결정적 신규 사실 (subagent 검증)

### 1-A. HEVC 4:4:4 클라이언트 HW 디코드 — 가능, 단 CUVID로만
- **NVDEC(CUVID) 경로로 Turing(RTX 20xx/GTX 16xx)+ 에서 HEVC 4:4:4 HW 디코드 가능.**
  D3D11VA에는 NVIDIA용 HEVC Rext 프로파일이 아예 없음(전 드라이버). Parsec 4:4:4도 이 경로(추정 근거:
  Turing+ 요구 조건 일치). moonlight-qt PR #1282(2024-07 머지)가 4:4:4 협상+2× 비트레이트 배수 구현.
- Intel: Tiger Lake+ HEVC 4:4:4 디코드 가능(벤더 확장; FFmpeg 메인라인 반영은 PR #20334 대기).
- AMD: 전 세대 VCN 4:2:2/4:4:4 디코드·인코드 전무 — AMD 클라는 SW 폴백.
- AV1 Profile 1(4:4:4) HW 디코드는 전 벤더 부재 — AV1은 4:2:0 전용 유지.

### 1-B. GFN "CQS = AV1 4:4:4"는 마케팅 착시
- NVENC AV1 인코드는 Blackwell 포함 전 세대 4:2:0 전용(공식 지원 매트릭스).
- CQS의 실체(추론, 근거 강함): **4:4:4/10-bit 축 = HEVC**, **적응 스트리밍 축 = AV1 4:2:0**,
  + AI HUD 샤프닝. 즉 CQS의 기둥들은 **RTX 4070 + Sunshine 포크로 오늘 재현 가능**
  (호스트 probe에서 HEVC 10-bit 4:4:4 인코드 이미 성공). GFN 최대 100 Mbps — LAN에선 우리가 초과 가능.

### 1-C. DCV의 "선명함"의 정체 (모방 대상)
1. **Build-to-lossless(QU)**: 모션 중 H.264/AV1 lossy → 화면 정지 시 정적 영역을 GPU 프레임버퍼의
   픽셀-퍼펙트 무손실로 2차 패스 치환(`enable-qu`, `qu-bandwidth`). 정지 콘텐츠는 문자 그대로 무손실.
2. **Dirty-region 타일 인코딩**: 변경 영역만 인코드(`use-grabber-dirty-region`),
   변경률 <3%면 타일 모드, 초과 시 풀프레임(`full-frame-threshold=(3,3)`).
3. **클라이언트 사이드 커서**: 커서를 스트림 밖 메타데이터로 → 체감 커서 지연 0.
4. QUIC datagram(디스플레이) + stream(제어/QU) 이중 모드, `frames-in-transit` 적응 윈도우.
- **게이밍 기준은 아님**: 기본 25fps 캡, 120fps 부재, 지연 실측 미공개. 화질 기법만 가져온다.
- **법적**: dirty-region/타일/점진적 무손실은 RemoteFX Progressive(2008)·Citrix ThinWire 등
  광범위한 prior art. 기법 모방은 업계 관행. 출시 전 US 8,520,734 클레임만 변리사 검토.

### 1-D. 네이티브 클라 최저지연 레시피 (아무도 다 안 함)
- **프레젠트**: `FLIP_DISCARD` + `ALLOW_TEARING` + waitable swapchain(MaxLatency=1) —
  실측 ~3ms(PresentMon), tearing 모드는 서브 프레임. VRR 연동 프레젠트를 출하한 스트리밍
  클라이언트는 2026-07 현재 **0개**(Moonlight 미지원 확인, Parsec은 2017 blog의 FLIP_SEQUENTIAL).
- **디코드**: CUVID `ulMaxDisplayDelay=0` + `CUVID_PKT_ENDOFPICTURE`(기본값 4 대비 표시 큐
  4프레임=66.7ms@60fps 제거). Ada NVDEC 처리량 1080p HEVC 1641fps(엔진 점유 ~0.6ms/frame).
- **오디오**: WASAPI exclusive event-driven 128프레임 = 2.67ms(총 2–5ms). Moonlight은 shared
  ~15ms — **오디오에서 5–10ms 공짜 우위**. Opus 2.5ms 프레임이면 알고리즘 지연 5ms.
- **입력**: `GetRawInputBuffer`+`RIDEV_INPUTSINK`(+캡처 중 `RIDEV_NOLEGACY`) — 8kHz 마우스
  플러드 대응. GameInput v3.4(2026-05): kbd/mouse/pad 통합 타임스탬프. Win키/Alt-Tab은
  WH_KEYBOARD_LL로, Ctrl+Alt+Del까지는 커널 드라이버(Parsec HID Mode 동급, 후순위).

### 1-E. 클라이언트 아키텍처 비교
| 옵션 | HW 4:4:4 | VRR/tearing | 공수 | 판정 |
|---|---|---|---|---|
| moonlight-qt 포크 (C++/Qt/FFmpeg) | CUVID로 가능(경로 존재) | 없음 → 우리가 추가 | 전송 이식 2–4주(추정) | **근시일 성능 클라** |
| Rust 네이티브 (nvcodec-rs+wgpu+windows-rs) | 가능(직접 배선) | 자유 | 3–6개월(전송 제외) | **장기 본선** |
| Tauri/WebView2+WebCodecs | NVIDIA에선 불가(D3D11VA 갭) | 불가(컴포지터) | 소 | 성능 목표 부적합 — **간편/호환 클라로 유지** |

기존 native-parsec-design-track(별도 세션)의 Model B(Tauri) 권장은 **계정/무PIN wedge용으로 유효 유지**.
"미친 성능" 요구는 그 문서의 Model A 측정 게이트를 사용자 의사로 사실상 통과 — 단 Model A(순정
Moonlight 프로토콜 직결)가 아니라 **우리 커스텀 전송을 말하는 네이티브 클라**로 변형해야
전송 혁신(CC/FEC/슬라이스)이 살아남는다.

---

## 2. "장점 결합" 이식 매트릭스

| 출처 | 받아올 것 | 우리 구현 경로 |
|---|---|---|
| **Parsec** | zero-buffer + 선혼잡 비트레이트 하향 철학 | Pudica류 프레임딜레이 CC (2018 BUD보다 신형 연구) |
| | HID Mode(커널 입력, C-A-D까지) | 호스트 커널 주입 + 클라 WH_KEYBOARD_LL → 커널 드라이버(후순위) |
| | 240fps 지원, ~7ms LAN 기준선 | 1080p/1440p 240fps 프로파일 + 벤치 hard gate |
| | NAT 트래버설 UX | F2 coturn + ICE (진행 중) |
| **Moonlight/Sunshine** | AV1 10-bit/HEVC 4:4:4/HDR/150Mbps+ 코덱 스택 | 이미 베이스. 클라 CUVID 4:4:4 디코드 추가 |
| | RFI 손실 복구 | moonlight-common 포크에 이미 존재 — 유지 |
| | (Apollo) SudoVDA 가상 디스플레이, 해상도/주사율 자동 매칭 | Apollo 코드/기법 이식 검토 |
| | moonlight-qt 클라 코드베이스 | 포크 + 전송 교체 + 프레젠트/오디오 개선 |
| **GFN Ultimate** | L4S 옵트인 플레이북 | ECT(1)+SCReAM v2, 경로 감지 후 |
| | 서버측 프레임 페이싱(클라 vsync 정렬) | 캡처→인코드 타이밍 제어 (USPTO 12170801 기법 참고) |
| | AI 샤프닝/업스케일 | 클라 RTX VSR + 선택적 CAS |
| | CQS 기둥(HEVC 4:4:4 10bit + AV1 적응) | RTX 4070에서 오늘 재현 가능 |
| **DCV** | **Build-to-lossless QU** | 데스크톱 프로파일: DXGI dirty rects + 타일 안정 감지 → 무손실 타일 2차 패스(신뢰 채널), qu-bandwidth 예산 |
| | Dirty-region/full-frame threshold | 게임=풀프레임 LL / 데스크톱=타일 자동 전환 |
| | 클라이언트 사이드 커서 | 커서 분리 전송 경로 확인·보강 |
| | datagram+stream 이중 전송 | 비디오=비신뢰, QU/제어=신뢰 채널 (WebRTC DC → 추후 QUIC) |
| **우리 고유 (아무도 없음)** | VRR/tearing 프레젠트, 서브프레임 슬라이스, WASAPI exclusive, Tetrys FEC, 독립 G2G 계측 | 차별화 코어 |

---

## 3. 타깃 아키텍처

```
[집 Windows RTX 4070]                                [클라이언트]
Sunshine 포크(캡처·NVENC·입력주입)                    성능 클라: moonlight-qt 포크 → (장기) Rust 네이티브
  └ Rust streamer: 커스텀 전송                          ├ 디코드: CUVID(4:4:4/AV1) ulMaxDisplayDelay=0
     ├ 프레임딜레이 CC(Pudica류)                        ├ 프레젠트: FLIP_DISCARD+ALLOW_TEARING+waitable(1)
     ├ Tetrys 슬라이딩윈도우 FEC                        ├ 오디오: WASAPI exclusive 2.67ms
     ├ 서브프레임 슬라이스 파이프라인                    └ 입력: RawInputBuffer+GameInput(+커널 후순위)
     ├ QU(build-to-lossless) 채널                     간편 클라: 웹/Tauri(Model B) — 호환·무설치 wedge 유지
     └ 게임/데스크톱 프로파일 자동 전환
```

- 게임 모드: 풀프레임 AV1 10-bit 4:2:0 LL, 120–240fps, QU off, 슬라이스 파이프라인 on.
- 데스크톱 모드: 타일 인코딩 + HEVC 4:4:4(클라 CUVID) + QU build-to-lossless — "뭉개짐"의 구조적 해결.
- 예상 성능(가설, hard gate로 검증): LAN 120Hz G2G ~8–12ms, 240Hz ~5–8ms, 정지 화면 픽셀-퍼펙트,
  오디오 Moonlight 대비 5–10ms 우위, 정적 데스크톱 대역폭 ~0.

## 4. 리스크·미검증
1. 4K NVENC ULL 인코드 지연 문헌 상충 — P0 자체 실측(리서치 4와 동일).
2. moonlight-qt 전송 이식 "2–4주"는 추정치 — 스파이크로 검증.
3. QU 호스트 구현(무손실 타일 경로)은 Sunshine 포크에 신규 인코더 경로 — 중규모 공사.
4. US 8,520,734 특허 클레임 검토(출시 전).
5. main 워킹트리 미커밋 → clean build 불가(native-parsec-design-track 블로커) — 코드 착수 전 커밋 필요.

## 5. 결정 사항
0. **제품 티어 확정(2026-07-14, owner)**: 네이티브 앱 = 초고화질·초고효율·초저지연(ultra 티어) /
   웹 클라 = 간단 접속 + 고화질·고효율·저지연(high 티어). 호스트·전송 코어(QU, CC, FEC, 슬라이스,
   프로파일 전환)는 **단일 백엔드 공용** — 티어 차이는 클라이언트 끝단(디코드·프레젠트·입력)에만 있다.
   ultra 전용 = CUVID 4:4:4, VRR/tearing 프레젠트, WASAPI exclusive, 커널 입력.
   웹도 QU build-to-lossless 혜택은 받음(DCV가 WebSocket+WebCodecs로 증명).
1. 성능 클라 = 네이티브(근시일 moonlight-qt 포크, 장기 Rust) — 사용자의 "HW 디코드 데스크탑 앱" 지시로 확정.
2. Tauri(Model B)는 폐기가 아니라 간편/호환 클라로 역할 축소 — native-parsec-design-track과 양립.
3. 네이티브 클라는 순정 Moonlight 프로토콜 직결(Model A)이 아닌 **우리 커스텀 전송**을 말한다 — CC/FEC/슬라이스 혁신 보존 목적.
4. DCV에서 가져오는 것은 기법(QU/타일/커서)이지 지연 기준이 아님 — 지연 기준은 Parsec 7ms/240fps.
5. GFN CQS의 "AV1 4:4:4" 함의는 허상으로 판정 — 대외 비교 주장 시 HEVC 4:4:4 기준으로만.

## 6. 추가 확장 후보 (2026-07-14, owner 판정: **우선순위 아님 — 우선순위는 ultra 성능 코어(§3)**)

§2 매트릭스 밖에서 식별된 보완 축. 지금 착수하지 않으며, ultra 성능 코어(CC/FEC/슬라이스/QU/네이티브 클라)
이후 재판단한다. 단 #1은 슬라이스 파이프라인 설계 시 **결정만** 같이 내려야 재작업이 없다(구현은 별개).

| # | 확장 | 요지 | 결합 지점 |
|---|---|---|---|
| 1 | 인코더 심층 튜닝 | intra-refresh(IDR 스파이크 제거), LTR+RFI 결합 복구, NVENC ULL/AQ 표준화 | 슬라이스 파이프라인과 설계 결정 공유 |
| 2 | SVC 시간적 계층 + FEC 결합 | 혼잡 시 화질 붕괴 대신 프레임레이트 강등(L1T2/T3) — 제3의 우아한 강등 경로 | CC/FEC 뒤에 얹는 강등 정책 |
| 3 | 무중단 재접속/세션 리줌 | ICE restart → (장기) QUIC connection migration; 불안정 공용망 체감 핵심 | 전송 코어 |
| 4 | 호스트 하드닝(p99) | MMCSS/스레드 우선순위, HAGS 정책, NVENC 세션 우선순위; 벤치 hard gate에 p99 항목 추가 | 벤치마크 게이트 |
| 5 | 멀티모니터 | DCV 강점; SudoVDA 가상 디스플레이 이식과 세트 | 데스크톱 티어 |
| 6 | 클립보드/파일 전송 | 기존 reliable DataChannel로 streamer 단독 구현 가능 | 데스크톱 티어 |
| 7 | 클라 프레임 페이싱 정책 노브 | 지터 흡수 vs 최저지연 선택(게임/데스크톱 프로파일 연동) | 클라 프레젠트 |

후순위/연구 플래그(판단 보류): A/V 동기 정책 명문화(오디오 즉시 재생), NVFBC 캡처 검토(GeForce EULA 제약),
클라이언트 리프로젝션(timewarp류 — "우리 고유" 4번째 후보이나 연구 리스크 큼), 멀티패스 본딩(주 시나리오가
단일 경로라 스킵).
