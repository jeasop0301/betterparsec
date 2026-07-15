# 화질·효율 감사 (2026-07-16)

**질문(owner):** "압도적 네이티브급 화질 + 타 프로그램 대비 훨씬 낮은 비트레이트로
훨씬 좋은 화질(코딩 효율)." HDR은 별도 트랙(다이나믹레인지, 선명도 아님).

**결론 한 줄:** 화질/효율의 최대 지렛대들이 **capability 부재가 아니라 꺼진 채**
돌고 있다. 인코더는 P1(최속=최저화질) + 지각 최적화 OFF, 코덱은 H264 기본,
크로마 4:2:0 8-bit 기본 — 그런데 **웹 클라는 HEVC/AV1/4:4:4/10-bit 디코드를 이미
전부 지원**(WebCodecs 감지 완비, video/*_decoder_pipe.ts). 클라 후처리(샤픈/
업스케일)만 진짜 미구현. "저비트레이트 고효율"은 대부분 **켜고·기본값 바꾸고·
측정하면** 되는 튜닝이다.

---

## A. 인코더 지렛대 실측 (Foundation Sunshine `nvenc_config.h` 기본값)

| 지렛대 | 현 기본값 | 효과 | 지연 비용 | 판정 |
|---|---|---|---|---|
| `quality_preset` | **P1** (1..7, 높을수록 느림·고화질) | 프리셋↑ = 같은 비트레이트 화질↑ | P1→P4 ≈ +500µs@1080p (nvenc-slice-probe 실측, 16.6ms 예산 내) | **무료 근접** — P4~P5 상향 |
| `adaptive_quantization` (spatial AQ) | **false** | "평탄 영역에 비트 더" 지각 배분 | **0 (lookahead 불요)** | **완전 무료** — 즉시 ON 후보 |
| `weighted_prediction` | **false** | 페이드 압축 개선(CUDA) | 저 | 무료 근접 |
| `two_pass` | quarter_res | VBV/모션벡터 개선 | 저 | 이미 ON(양호) |
| `enable_temporal_aq` | false | 시간축 AQ | **lookahead 필요=지연** | Gate C 튜닝 |
| `lookahead_level`/`depth` | disabled/0 | 비트 배분 개선 | **프레임 버퍼링=지연** | Gate C 튜닝 |
| `temporal_filter` | disabled | 노이즈 감소·압축 | **B프레임 필요=지연** | 저지연 부적합 |
| `rate_control_mode` | cbr | vbr = 화질↑·가변 | 버퍼블로트 리스크 | Gate C(CC와 상호작용) |

**"무료 점심" 세트(지연 비용 ~0, Gate B 측정만 하면 켤 수 있음):**
spatial AQ ON · preset P1→P4/P5 · weighted_prediction ON.

---

## B. 코덱·크로마·비트뎁스 — capability는 이미 있다

**웹 클라 디코드 지원 (video_decoder_pipe.ts / video_element.ts — 브라우저
WebCodecs `isConfigSupported` 감지):** H264, **H264_HIGH8_444**, H265,
**H265_MAIN10, H265_REXT8_444, H265_REXT10_444**, AV1_MAIN8, **AV1_MAIN10,
AV1_HIGH8_444, AV1_HIGH10_444**. → 클라는 **4:4:4·10-bit·HEVC·AV1 전부
디코드 준비 완료.**

**그런데 `web/default_settings.ts` `videoCodec: "h264"`** — 기본이 H264.
**호스트 인코드**: NVENC YUV444 + 10-bit 경로 존재(`nvenc_utils.cpp`,
10-bit 4:4:4는 CUDA interop). Foundation는 HEVC 10-bit YUV444 encode probe
통과 확인(ROADMAP 지금위치). AV1 YUV444는 미지원(4:2:0만).

| 축 | 효율/화질 이득 | 현 기본 | 상태 |
|---|---|---|---|
| H264→**HEVC** | 같은 화질 −30% 비트 | H264 | 클라 [x] · 호스트 encode e2e 검증 [ ] · 기본 전환 [ ] |
| HEVC→**AV1** | 같은 화질 추가 −20~30% | — | 클라 [x] · 호스트 AV1 4:2:0 [x](probe) · e2e [ ] |
| 4:2:0→**4:4:4** | 색 텍스트 fringing 제거(데스크톱 필수) | 4:2:0 | 클라 [x] · 호스트 경로 [x] · e2e·협상 [ ] |
| 8→**10-bit(SDR)** | 밴딩 제거(그라디언트·어두운 씬 "네이티브급") | 8-bit | 클라 [x] · 호스트 P010 [x] · e2e [ ] |

→ **M3의 "미완"은 대부분 클라가 아니라 호스트 encode 검증 + 기본값 전환.**
클라 절반은 이미 완성돼 있다(감사 정정).

---

## C. 진짜 미구현 capability (튜닝 아님 — 신규 트랙)

1. **클라 후처리(샤픈/슈퍼레졸루션)** — 현재 네이티브 프레젠트는
   `DXGI_SCALING_STRETCH`(=bilinear) 단순 스트레치, 8-bit RTV(present.rs).
   샤픈/업스케일 셰이더 0. **GFN의 DLSS 지렛대 미사용.** 클라 샤픈 =
   체감 선명도↑ + **저해상도 전송 허용(1440p 전송→4K 업스케일 = 대형 효율)**.
   구현: D3D11 픽셀 셰이더(샤픈/CAS) — app-native 프레젠트에 헤드리스 착수 가능.
2. **대역 구동 동적 해상도 스케일링** — 현재 비트레이트만 적응(ABR/CC),
   해상도는 고정. 혼잡 시 해상도↓ + 클라 업스케일이 블록킹보다 지각적 우위.
   Sunshine `dynamic_resolution_follow_display`는 디스플레이 변경 추종이지
   대역 구동 아님. 컨트롤러(대역→해상도 사다리)는 **순수/헤드리스**
   (FecRatioController 패턴).
3. **AV1 필름그레인 합성** — 그레인 제거 인코드(저비트) + 클라 합성.
   그레인/노이즈 콘텐츠 효율.
4. **10-bit 프레젠트 경로(네이티브)** — 현재 R8G8B8A8_UNORM 스왑체인.
   10-bit/HDR엔 R10G10B10A2 + HDR 스왑체인 필요.

---

## D. 검증 계획 — "저비트레이트 고효율"을 측정으로 증명

nvenc-slice-probe가 "슬라이스 지연 세금 없음"을 숫자로 증명했듯,
**화질-효율 probe**로 각 지렛대의 **비트레이트당 화질**을 정량화한다:

- 인코드(고정 비트레이트) → 디코드(app-native video.rs 경로 재사용) →
  **SSIM/PSNR(원본 대비)** — preset(P1..P7) × spatial AQ(on/off) ×
  codec(H264/HEVC/AV1) × chroma(4:2:0/4:4:4) 스윕.
- 콘텐츠: 실 디테일·엣지·노이즈를 담은 합성 프레임(단순 그라디언트는
  프리셋 차이를 못 드러냄) 또는 실 캡처 프레임 세트.
- 출력: config | 비트레이트 | PSNR | SSIM | (VMAF는 도구 조달 후).
- **결과 = 어느 지렛대가 얼마의 화질/비트를 주는지** → 무료 점심 세트의
  실측 우선순위 + Gate B 활성 근거. SSIM은 순수 Rust 단위검증 가능.

---

## E. 권장 순서

1. **[헤드리스] 화질-효율 probe** (SSIM/PSNR vs 비트레이트, preset·AQ·codec·chroma) —
   무료 점심 세트의 정량 근거. nvenc-slice-probe 스캐폴딩 재사용.
2. **[헤드리스] 동적 해상도 컨트롤러** (대역→해상도 사다리, 순수, FEC 비율 패턴).
3. **[헤드리스] 클라 샤픈 셰이더** (D3D11 CAS/샤픈, present.rs).
4. **[라이브/호스트] 무료 점심 활성**: spatial AQ ON + preset P4 + weighted-pred ON —
   probe로 sweet spot 확정 후 Gate B VMAF 검증.
5. **[라이브/호스트] 코덱 기본 전환**: HEVC(→AV1) e2e 검증 후 기본값 h264→hevc/av1.
6. **[라이브/호스트] 4:4:4 + 10-bit SDR** e2e 협상·검증(클라 준비 완료).
7. **[대형] 클라 슈퍼레졸루션(FSR/VSR급)** + 10-bit/HDR 프레젠트 경로.

**핵심 통찰:** 우리는 지연 코어에 집중하느라 인코더를 "최속·최저화질"로 켜둔
채였다. 무료 점심(spatial AQ + preset + HEVC/AV1 + 4:4:4/10-bit)만 켜도
"타 대비 저비트레이트 고화질"의 상당 부분이 열린다 — capability가 아니라
기본값·측정·활성 문제. 진짜 신규 개발은 클라 슈퍼레졸루션 하나.
---

## F. 심층 파이프라인 추적 (2026-07-16 — "끝까지" 2차 감사)

실제 디코드→프레젠트 경로를 바이트 단위로 추적한 결과:

1. **[수리 완료] 네이티브 색 행렬/레인지 버그** — `app-native/src/video.rs`
   `frame_to_rgba`가 `sws_getCachedContext`를 **색공간/레인지 지정 없이**
   생성 → swscale 기본 = **BT.601 limited**. HD(1080p)는 BT.709이므로
   **709 콘텐츠를 601 행렬로 변환 = 색 실제 틀어짐**(피부톤·채도 이동). 이
   RGBA가 present까지 그대로 감(별도 YUV 셰이더 없음, R8G8B8A8 업로드).
   웹은 브라우저 WebCodecs가 VUI를 읽어 정상 → **네이티브 전용 버그**.
   **수정**: 프레임 `colorspace`/`color_range`로 `sws_setColorspaceDetails`
   적용(709/601/2020 + full/limited), 미지정 시 해상도 폴백(≥720p→709).
   순수 결정함수 `sws_cs_for`/`is_full_range` +2 tests, app-native 52 tests.
2. **[미해결] 8-bit 프레젠트 천장** — present.rs 스왑체인 R8G8B8A8_UNORM.
   10-bit 디코드해도 present에서 8-bit 절단 → 밴딩 잔존. 네이티브 10-bit엔
   R10G10B10A2 + HDR 스왑체인 필요(M3 신규 capability).
3. **[미해결] present용 CPU 왕복(Phase A)** — 디코드→`hwframe_transfer`(GPU→CPU)
   →swscale→D3D11 텍스처 업로드(CPU→GPU). 프레임당 GPU↔CPU 왕복 = 지연 +
   추가 변환. 제로카피 NV12 텍스처 직결은 "Phase B" 유보(present.rs 모듈 doc).
4. **[유의] 스케일링 품질** — 현재 SWS_FAST_BILINEAR지만 1:1(색 변환만)이라
   무해. **동적 해상도 스케일링 착수 시** FAST_BILINEAR→고품질(bicubic/lanczos)
   교체 필요, 아니면 업스케일이 뭉갬.
5. **[유의] 웹 색 신호 의존** — 웹은 브라우저 VUI 처리에 의존 → **Sunshine VUI
   신호 정확성** 라이브 확인 필요(특히 4:4:4/10-bit/HDR).

**2차 감사 결론:** 첫 감사(인코더 튜닝)에 더해 **클라 디코드 경로에 실제 색
버그**가 있었다(수리 완료). "네이티브급 화질"은 인코더 무료 점심 + 이 색
정확도 + 10-bit 프레젠트가 함께 가야 완성된다.
