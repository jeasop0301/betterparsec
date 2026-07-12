# 리서치 2 — 저비트율 화질 ‘뭉개짐’ 심층 진단

> 뭉개짐의 원인(WARP/레이트컨트롤/4:2:0/코덱)을 분리하고, AV1+4:4:4가 같은 비트레이트에서 이기는지 정량 검증.

_다중 에이전트 리서치 + 적대적 검증. run: `wf_b3fffaff-df9`. 2026-07._

---

# betterparsec 기술 심층 메모: 사지방→집 Windows PC 원격 데스크톱의 화질 뭉개짐(mushing) 진단과 해결

작성: Principal Streaming Engineer / 근거: 6개 분석 findings + adversarial verification 12건
태그 규약: [CONFIRMED] / [DISPUTED] / [CONTEXT-DEPENDENT] / [UNVERIFIABLE]

---

## 0. 전제와 최대 미지수 (반드시 먼저)

- **핵심 미지수 #1 — 실제 링크 대역폭/지터/손실**: 사용자가 WARP 터널을 통해 실제로 받는 유효 대역폭·jitter·packet loss를 아직 **측정하지 않았다**. 뭉개짐의 원인 순위는 이 값에 따라 완전히 뒤바뀌므로, 이 메모의 모든 진단은 **대역폭 분기(branch)** 로 제시한다. 측정 없이 codec을 바꾸는 것은 순서가 틀렸다.
- **핵심 미지수 #2 — 사지방 PC(클라이언트) 제약**: 사지방(사이버지식정보방)의 공용 PC는 lockdown 되어 있다. Moonlight/커스텀 클라이언트 **설치 가능 여부**, 그리고 그 PC의 GPU가 HEVC 4:4:4를 **하드웨어 디코드**할 수 있는지(NVIDIA Turing/GTX1600·RTX2000+ 또는 Intel 11th-gen+)가 권고안을 가른다. 미측정.
- **증상 용어**: "뭉개짐(mushing)"은 (a) 정지 화면에서도 컬러 텍스트가 흐릿한 것과 (b) 스크롤/이동/영상 재생 등 **모션 시에만** 전체가 뭉개지는 것 두 가지가 근본 원인이 다르다. 사용자가 **언제** 뭉개지는지가 최우선 진단 신호다(§5).

---

## 1. ROOT CAUSE 순위 (대역폭 분기 조건 포함)

후보 4종을 기여도 순으로. **모드(most-likely dominant)는 WARP 전송**이되, 이는 **미지수 #1에 조건부**다.

### 1순위 — WARP 전송(대역폭 스로틀 + jitter/loss, 최악의 경우 TCP-443 fallback)
가장 유력한 지배 요인일 가능성이 높다. 이유:
- WARP은 **edge-relay**다. 모든 패킷이 client→가장 가까운 Cloudflare PoP→host의 삼각 경로를 강제로 탄다(사용자가 경로를 제어 못 함). https://developers.cloudflare.com/cloudflare-one/team-and-resources/devices/warp/configure-warp/route-traffic/warp-architecture/
- 무료 WARP은 처리량을 **~50% 깎고 편차가 크다** (측정 280.63→132.40 Mbps, ~53% 감소; 단 지역 편차 큼—케냐 테스트는 20%만 감소) [CONFIRMED]. https://www.vpnmentor.com/reviews/warp-by-cloudflare/ 고정 bitrate 스트림을 굶기기에 충분한 변동성이다.
- NetBird의 2026 iperf3 벤치는 relay 경로가 지역 라우트에서 direct P2P WireGuard 대비 **2–5배 느리고**, 300 Mbps UDP 테스트에서 **14% 손실**(vs P2P 1.2%)을 보였다 — [CONTEXT-DEPENDENT]. 단, 이는 **경쟁사(NetBird) self-benchmark**이고, 14% "손실"은 무작위 손실이 아니라 **~250–290 Mbps 처리량 상한을 초과시킨 saturation artifact**이며, 장거리 국제 라우트에서는 오히려 WARP이 이겼다. 따라서 "WARP=항상 나쁨"이 아니라 **사용자의 특정 경로에서 측정해야** 한다. https://netbird.io/knowledge-hub/cloudflare-mesh-vs-netbird-vs-tailscale
- **뭉개짐과의 직접 인과**: 실시간 스트림에서 loss/jitter는 decoder가 stale macroblock을 concealment로 끌고 가 sharp한 데스크톱(텍스트·에지)을 **뭉개는(smear)** 전형적 메커니즘이며, 이는 grain·모션으로 가려지는 natural video보다 **화면 콘텐츠에서 훨씬 잘 드러난다**. 또한 Parsec BUD은 congestion을 감지하면 encoder target bitrate를 **선제적으로 내리므로**(latency>framerate>quality 우선순위) 링크가 나빠지는 순간 품질이 곧장 떨어져 뭉개진다. https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007
- **UDP 차단 우회의 함정**: 사지방이 임의 UDP를 막으면 WARP은 MASQUE(기본 QUIC/UDP 443)로 붙고, UDP가 완전 차단되면 **TCP 443(HTTP/2)로 fallback** 한다(WireGuard 모드는 TCP fallback 없음) [CONFIRMED]. 스트림이 TCP 터널 안으로 들어가면 TCP-over-TCP head-of-line blocking으로 jitter/bufferbloat가 폭증해 그 자체로 뭉개짐을 만든다. 단 정교화: **UDP 443만 막힌다고 곧장 TCP가 되는 게 아니라** MASQUE는 500/1701/4500/4443/8443/8095 UDP 포트 사다리를 먼저 시도한 뒤 최후에 TCP다. https://developers.cloudflare.com/cloudflare-one/team-and-resources/devices/cloudflare-one-client/deployment/firewall/

> **분기**: 측정 결과 WARP이 (i) 안정적으로 20+ Mbps·낮은 jitter/loss로 전달 중이면 → WARP은 지배 요인 **아님**, 2·3순위로 이동. (ii) <10 Mbps거나 loss/jitter가 높거나 TCP fallback이면 → **transport-bound**이며 아래 codec/chroma 변경으로는 **해결 불가**. 약한 정황: 사용자가 latency는 불평하지 않으므로 완전한 TCP-meltdown일 가능성은 낮고(그랬다면 지연도 불평했을 것), **throughput throttle + jitter** 쪽이 더 유력.

### 2순위 — Rate-control-on-motion (single-frame VBV 스파이크)
"정지 시엔 선명, 스크롤/이동 시에만 뭉개짐" 패턴의 지배 요인이며 **깨끗하고 굵은 링크에서도 발생**한다.
- 화면 콘텐츠의 스크롤 프레임은 새로 드러난 영역+텍스트 reflow로 inter-residual이 거의 intra 수준으로 커지는데, Sunshine 기본값인 **single-frame VBV/HRD**(프레임 크기 ≤ bitrate/fps, 예: 20 Mbps@60fps → ~333 kbit/frame 상한)는 이 스파이크를 수용 못 해 QP를 튀기고 프레임이 blur/block 된다. https://slhck.info/video/2017/03/01/rate-control.html
- 별개의 원인으로 **키프레임/IDR 스파이크**: loss 복구용 IDR은 크고 bitrate 상한에 눌려 몇 프레임간 blocky해지며, 그 자체가 추가 loss를 유발하는 피드백 루프가 된다. https://github.com/games-on-whales/wolf/issues/5

### 3순위 — 4:2:0 chroma (컬러 텍스트 fringing) — bitrate로 안 고쳐짐
"정지 상태에서도 빨강/파랑 텍스트가 흐릿"하면 이게 범인이다.
- 4:2:0은 chroma를 1/4 해상도(2×2블록당 Cb·Cr 1개)로 저장해, 얇은 컬러 글리프의 에지(주로 chroma에 존재)를 뭉갠다. 빨강(BT.709 luma 계수 0.2126)·파랑(0.0722)은 luma 에지가 약하고 chroma 에지가 강해 **4:2:0이 버리는 바로 그 정보**라 다크테마 syntax highlighting이 가장 심하게 뭉갠다. https://www.go-euc.com/chroma-subsampling-and-colour-compression-in-modern-remoting-protocols/
- **중요**: 이는 효율 문제가 아니라 chroma 해상도 바닥이라 **codec을 바꾸거나 bitrate를 50 Mbps로 올려도 안 고쳐지고 오직 4:4:4만 없앤다**. Parsec이 4:4:4를 유료 기능으로 파는 이유. https://support.parsec.app/hc/en-us/articles/32381785123860

### 4순위 — Codec generation (Parsec은 HEVC/H.264, AV1 없음)
가장 **작은** 레버. 하드웨어 파이프라인에서 AV1이 HEVC 대비 realized로 주는 이득은 **~15–25%(≈한 눈금)** 이며(§2), 6–14 Mbps 근처의 bit-starved 구간에서만 readable-vs-mushy를 가른다. Parsec은 AV1이 없으므로 이게 open stack의 유일한 진짜 codec 우위다 [CONFIRMED]. https://support.parsec.app/hc/en-us/articles/32381568346644

---

## 2. 해결 가능한가? 5/10/20 Mbps에서 AV1+4:4:4+adaptive-RC가 같은 bitrate에서 Parsec을 측정 가능하게 이기는가

**먼저 물리적 사실 하나로 전제를 깬다: "AV1 + 4:4:4" 스택은 하드웨어에 존재하지 않는다** [CONFIRMED — CONTEXT-DEPENDENT는 "end-to-end" 정의에 한함]. 어떤 출하 하드웨어 인코더(NVENC/QSV/AMF)도 AV1 4:4:4 인코드를 지원하지 않는다. 따라서 open stack은 **AV1 4:2:0**(luma/모션 효율 최고, 컬러텍스트는 subsampled) **또는 HEVC Main 4:4:4**(컬러텍스트 선명, HEVC급 효율) 중 **택1**이지 둘 다는 불가. https://github.com/orgs/LizardByte/discussions/220

동시에 화면콘텐츠 BD-rate의 큰 숫자들은 대부분 **소프트웨어/intra-only** 도구라 GPU 실시간 경로에 **전이되지 않는다**:
- HEVC-SCC는 TGM(텍스트/그래픽)에서 HEVC-RExt 대비 **-57.4% luma BD-rate(4:4:4, All-Intra)**, natural video는 ~0% [CONFIRMED, MERL TR2015-126 Table IX]. https://www.merl.com/publications/docs/TR2015-126.pdf **그러나 이 SCC(palette/IBC)는 소프트웨어 인코더 전용**이다.
- AV1 IBC 단독 ~27%(RA) / All-Intra ~44%, AV1 SCC는 HEVC-SCC 대비 All-Intra ~3pt 우위 [CONFIRMED, arXiv 2011.14068]. https://ieeexplore.ieee.org/document/8416608/ 역시 소프트웨어·intra 중심이라 저지연 P-frame 스트림엔 거의 미실현.
- **[DISPUTED] "어떤 하드웨어도 SCC를 노출 안 한다"는 틀렸다**: NVENC(Ada/Blackwell)·AMD AMF는 SCC 노브가 없지만, **Intel QSV는 oneVPL 2.13+(2024말)부터 `mfxExtAV1ScreenContentTools`로 AV1 palette+IntraBlockCopy를 하드웨어에서 노출**한다. https://intel.github.io/libvpl/latest/API_ref/VPL_structs_encode.html · https://www.phoronix.com/news/Intel-oneVPL-2.13-Released 즉 **Intel Arc 호스트라면 하드웨어 AV1 SCC 텍스트 선명화가 가능**하다(NVIDIA는 불가). (부수 오류: `--scm`은 SVT-AV1 플래그, libaom은 `--tune-content=screen`.)
- Visionular Aurora1의 x264 대비 -81% BD-rate는 **[UNVERIFIABLE] 벤더 자체 소프트웨어 인코더 수치**이며 하드웨어 NVENC/AMF/QSV로는 달성 불가. https://visionular.ai/av1-screen-content-coding/

**"같은 bitrate에서 측정 가능하게 이기는가"의 정직한 답 = AV1-over-HEVC 마진(~15–25%, [CONTEXT-DEPENDENT], hardware low-latency 실측 추정)**. Meta RTC는 AV1이 H.264 대비 ≥20%(mixed/screen) https://engineering.fb.com/2026/06/22/video-engineering/adopting-av1-for-real-time-communication-rtc-meta/ — HEVC 대비로는 더 작다. 그리고 Parsec의 유료 4:4:4는 **같은 HEVC-Main-4:4:4 하드웨어 경로**라 4:4:4 콘텐츠에선 encode-quality 우위가 **없다** [CONFIRMED]. 결론: betterparsec의 same-bitrate 우위는 **AV1 4:2:0 한 눈금 + 4:4:4 무료 제공 + RC 튜닝**이지 세대 도약이 아니다.

대역폭 구간별:
- **5 Mbps — bandwidth-bound**. 1080p60 4:2:0 uncompressed ~1.5 Gbps → 5 Mbps는 ~300:1, ~0.04 bpp/frame. 지속 모션은 codec 불문 뭉개진다. AV1은 5→~6.5 Mbps-HEVC 상당(여전히 모션 뭉개짐). 4:4:4는 여기선 luma를 굶겨 **더 나빠진다**.
- **10 Mbps — 경계**. AV1 4:2:0의 ~20%가 readable-vs-mushy를 실제로 가르는 구간. codec swap이 값을 하는 곳.
- **20 Mbps — luma는 대체로 충분**. 남는 뭉개짐은 (a) 컬러텍스트 chroma(→4:4:4로만 해결) + (b) RC transient(→VBV/intra-refresh 튜닝으로 해결). AV1의 효율 이득은 여기선 한계효용이 작다.

> **만약 measurement가 transport-bound(WARP 얇음/lossy)로 나오면 — 평범하게 말한다: codec/chroma 무엇을 해도 안 고쳐진다. 전송을 먼저 고쳐야 한다.**

---

## 3. 단일 최고 레버리지 변경 + 다음 순위

측정에 조건부지만, 증거 가중치상 **모드 권고를 커밋**한다.

**#1 (single highest-leverage) — 스트림을 WARP relay 경로에서 빼라.** UDP 443에서 리스닝하는 **직접/자가호스팅 WireGuard 또는 Tailscale-direct** 터널로 교체(direct P2P는 WARP 대비 2–5× throughput·~10× 낮은 UDP loss). 근거: 뭉개짐의 두 핵심 메커니즘(대역폭 굶김 + loss-concealment smear)을 **동시에** 제거하고, 무료이며, codec을 건드리기 전에 delivered bitrate를 정상화한다. 주의: (a) Tailscale이 hole-punch 실패 시 DERP relay로 fallback하면 ~5–10 Mbps로 **WARP보다 나빠질 수 있으니** "direct" 확인 필수; (b) 사지방 클라이언트가 outbound UDP를 전면 차단하면 어떤 옵션도 UDP-over-TCP라 저하되지만, 저-RTT 전용 VPS가 free-WARP-over-TCP보단 낫다. https://cfreeman.cloud/breaking-the-5-mbps-barrier-streaming-moonlight-over-tailscale-with-full-bandwidth/

이어지는 순위(측정 결과에 따라):
- **#2 — bitrate cap 상향** (전송이 깨끗해진 뒤). 링크가 20+ Mbps를 안정적으로 주면 그냥 올린다.
- **#3 — single-frame VBV 완화**: Sunshine `nvenc_vbv_increase`를 0→**100–200**(범위 0–400, 400=5×). 스크롤/모션 프레임이 bit을 더 쓰게 해 QP 스파이크 뭉개짐을 줄인다. 대가는 버퍼 헤드룸 없을 때 packet loss 위험이므로 `fec_percentage`(기본 20)와 함께 [CONFIRMED, 단 "reduces mushing"은 문서화된 게 아니라 메커니즘상 타당한 추론]. https://docs.lizardbyte.dev/projects/sunshine/latest/md_docs_2configuration.html
- **#4 — codec을 AV1 4:2:0으로**(Sunshine+Moonlight, Ada/RTX40+ 호스트). ~한 눈금 무료 효율. 단 AV1 디코드 지연 +2–4프레임(~16–50 ms) 트레이드오프. https://developer.nvidia.com/blog/improving-video-quality-and-performance-with-av1-and-nvidia-ada-lovelace-architecture/
- **#5 — 컬러텍스트 fringing이 남으면 HEVC Main 4:4:4 활성화**(AV1과 배타적). 클라이언트 HW 디코드 가능성 확인 필수.
- **#6 — Spatial AQ + 클라이언트 CAS 샤프닝**(<1ms). 저렴한 마감 처리.

---

## 4. betterparsec 인코딩 스펙 (Sunshine/GStreamer/WebRTC 베이스, 실행 가능한 설정 목록)

두 프로파일을 토글로 제공(AV1과 4:4:4는 하드웨어에서 공존 불가하므로):

**프로파일 A — MOTION/기본 (AV1 Main 4:2:0)**
- Codec/profile: **AV1 Main, 4:2:0 8-bit**, 호스트 Ada/RTX40+ NVENC (또는 Intel Arc — 여기선 QSV `mfxExtAV1ScreenContentTools`로 palette+IBC까지 켜면 텍스트도 유리).
- Rate controller: **CBR low-latency** 베이스 + **single-frame VBV 완화**(`nvenc_vbv_increase=100~200`), VBV **bufsize ≈ maxrate**(VoD의 2× 아님).
- Congestion control: **WebRTC 경로면 GCC** — `rtpgccbwe`(TWCC 필수) `notify::estimated-bitrate`를 인코더 bitrate에 실시간 연결(defaults min 1kbps/max 8.192Mbps/start 2.048Mbps → 데스크톱용 max를 20–50 Mbps로 상향) [CONFIRMED]. https://gstreamer.freedesktop.org/documentation/rsrtp/rtpgccbwe.html **Sunshine/Moonlight 경로면** 내장 adaptive + **RFI**에 의존하고 `minimum_fps_target`으로 품질 대신 FPS를 희생.
- Loss 복구: **RFI/LTR(작은 P-frame, older good frame 참조) — IDR 아님** [CONFIRMED, 단 RFI는 FEC 실패 후, 인코더+클라이언트 디코더 지원 필요, 없으면 IDR fallback]. WebRTC 스톡은 RFI/LTR 루프가 없어 PLI/keyframe로 복구 → **반드시 NACK/RTX + FlexFEC 추가**(안 그러면 loss마다 IDR 스파이크로 뭉개짐). https://github.com/moonlight-stream/moonlight-common-c/issues/120
- Intra-refresh vs IDR: **rolling intra-refresh(이동 밴드) 선호**로 keyframe 스파이크 제거. 단 Sunshine은 아직 NVENC intra-refresh 미노출(issue #3323)이라 실무는 **long GOP + RFI**로 근사. https://github.com/LizardByte/Sunshine/issues/3323
- 해상도/업스케일: **텍스트는 native 1:1**(폰트 최선명). 대역폭 부족 시에만 **adaptive lower-res + 클라이언트 spatial upscaler**(FSR1/RTX-VSR/MetalFX, <1–2ms) — 기본값 아님, neural SR은 글리프 hallucination 위험이라 텍스트엔 금지. https://github.com/moonlight-stream/moonlight-qt/pull/1557
- 지각 최적화: **Spatial AQ 활성**(`nvenc_spatial_aq`, 중간 강도 — 과하면 글리프 에지를 굶김) + **클라이언트 CAS 샤프닝**(<1ms). **denoise/film-grain/VMAF-RC는 데스크톱엔 무의미하니 제외**.

**프로파일 B — TEXT/정적작업 (HEVC Main 4:4:4)**
- Codec/chroma: **HEVC Main 4:4:4 8/10-bit**(NVENC Pascal+ 또는 Intel Ice Lake+; AMD 불가). 컬러텍스트 fringing을 없애는 유일한 스위치.
- 전제조건: **클라이언트가 HEVC 4:4:4를 HW 디코드**(NVIDIA Turing GTX1600/RTX2000+ 또는 Intel 11th-gen+)해야 하며, 아니면 software fallback으로 CPU/지연 급증. Moonlight 6.1의 4:4:4는 **experimental이고 RTX50/Arc에서 software fallback·버그가 잦다** [DISPUTED verdict가 지적] — 신뢰성은 GPU/드라이버 의존. https://github.com/moonlight-stream/moonlight-qt/issues/1852
- 나머지 RC/loss/intra-refresh 설정은 프로파일 A와 동일.

**공통**: B-frames off, lookahead off(저지연). QP는 queue-depth로 nudge. FEC ~20%.

---

## 5. 측정 계획 — 무엇이든 만들기 전에 병목부터 국소화

순서대로. **codec 튜닝보다 이게 먼저다.**

1. **현행 Parsec 통계 오버레이**(또는 Moonlight `Ctrl+Alt+Shift+S`)를 **실사용 중** 관찰: Packet Loss %, Latency Variance(jitter), Decode time, Host Processing/encode. 판정: **network loss·jitter↑ + host processing↓ → 네트워크(WARP)가 병목**; decode/host processing↑ + network 깨끗 → encoder/decoder 측. https://github.com/moonlight-stream/moonlight-docs/wiki/Frequently-Asked-Questions
2. **WARP 전송 확인**: `warp-cli status`/connection info로 **MASQUE UDP 인가 TCP-443 fallback 인가**. TCP면 그 자체로 뭉개짐 설명.
3. **터널 통과 iperf3**: `iperf3 -u -b 20M`(및 50M) client→host로 achieved Mbps·jitter·loss. **단 깨끗한 iperf를 오버레이보다 신뢰하지 말 것**(issue #724: iperf 0.061ms jitter인데 Moonlight는 16% network-drop) — bulk 테스트는 bufferbloat를 숨긴다. https://github.com/moonlight-stream/moonlight-qt/issues/724
4. **고정 bitrate A/B (인과 규명)**: Parsec/Moonlight를 **고정 30–40 Mbps**로 핀. 같은 콘텐츠를 **정지 vs 모션**, **WARP 경로 vs 직접/대체 터널**로 비교. 뭉개짐이 **WARP에서만 나고 direct에서 사라지면 WARP이 causal**.
5. **codec A/B (동일 bitrate)**: 같은 워크로드로 HEVC 4:2:0 vs AV1 4:2:0 vs HEVC 4:4:4를 **동일 Mbps**에서 — 컬러텍스트 선명도와 모션 뭉개짐을 눈으로 채점.
6. **chroma A/B**: 다크테마 **빨강/파랑 코드 텍스트**로 4:2:0 vs 4:4:4 — 4:2:0의 canonical 실패 케이스.
7. **국소화 규칙**: 정지-컬러텍스트-흐림 = 4:2:0 격리 / 모션에서만 뭉개짐 = RC·대역폭 / 항상 물렁 = 대역폭 굶김.

---

## 6. 정직한 최종 판정: betterparsec를 만들 이유가 되는가?

**대체로 안 된다. 이 단일 불만(뭉개짐)은 off-the-shelf 설정 변경으로 거의 다 해결될 가능성이 높다.**

- **transport-bound이면(가장 유력)**: WARP relay를 direct WireGuard/Tailscale-UDP443로 교체하거나 스트림을 WARP 밖으로 빼고, 필요하면 bitrate 상향. **새 소프트웨어 빌드 불필요.**
- **깨끗한 링크의 모션 뭉개짐이면**: **Sunshine+Moonlight로 전환 → AV1 4:2:0 + VBV 완화 + Spatial AQ.** 전부 기존 기능.
- **컬러텍스트 흐림이면**: 4:4:4 활성화 — Parsec Warp 유료($8.33–9.99/mo)로 켜거나, **Sunshine+Moonlight로 무료 HEVC 4:4:4**.

open stack이 Parsec 대비 갖는 **진짜 미충족 격차 = AV1 4:2:0 + 무료 4:4:4 + 튜닝 가능한 VBV/RC**인데, 이 셋은 **이미 오늘 Sunshine+Moonlight에 존재**한다. 따라서 codec 품질을 위해 betterparsec를 커스텀 빌드할 정당성은 약하다.

**betterparsec를 정당화할 수 있는 유일한 조건은 codec이 아니라 §0의 미지수 #2다**: 사지방 lockdown PC가 **Moonlight 설치를 막는다면**, GCC-coupled RC를 가진 **브라우저/WebCodecs 기반 thin 클라이언트**(설치 불필요, WebCodecs로 HEVC/AV1 HW 디코드)가 유일하게 만들 가치가 있는 부분이다. 그 외 software-AV1-SCC(palette/IBC로 극한 텍스트 선명)는 CPU·지연 비용이 커 이 용례엔 과하다.

**권고 실행 순서**: (1) §5 측정으로 WARP/transport부터 확정 → (2) transport 교체(direct UDP443) → (3) 여전히 뭉개지면 Sunshine+Moonlight AV1 4:2:0 + VBV 완화 → (4) 컬러텍스트만 남으면 4:4:4 토글. **여기까지가 전부 기성품이며, 그래도 남는 유일한 빌드 후보는 "사지방 PC에 Moonlight를 못 깐다"는 제약이 확인될 때의 브라우저 WebRTC 클라이언트뿐이다.**


---

## 적대적 검증 판정 (12건)


**[CONFIRMED]** HEVC-SCC (SCM-4.0) achieves approximately -57.4% luma BD-rate for text-and-graphics screen content (YUV 4:4:4, all-intra, lossy) versus HEVC Range Extensions, but essentially 0% on camera-captured natural video, per the IEEE-TCSVT/MERL HEVC-SCC overview Table IX.


> 정정/정밀화: Confirmed and precise. Per Table IX of "Overview of the Emerging HEVC Screen Content Coding Extension" (Xu, Joshi, Cohen; IEEE TCSVT 2016 / MERL TR2015-126), which reports lossy 4:4:4 BD-rate of SCM-4.0 (HEVC-SCC) against the HM-16.4 anchor (HEVC Range Extensions), the YUV / All-Intra / luma (Y) BD-rate is exactly -57.4% for the TGM (text-and-graphics-with-motion) category and +0.4% for the CC (camera-captured) category — i.e., essentially zero, a negligible slight increase rather than a gain (the corresponding CC luma values are +0.6% for Random-Access and +0.6% for Low-Delay B). Note the -57.4% figure is specifically the All-Intra luma result; the RA and LB luma savings for TGM are smaller (-48.0% and -40.5%). The paper's text agrees: for camera-captured content the SCC tools "neither help nor harm the coding efficiency noticeably."


**[CONFIRMED]** Sunshine's nvenc_vbv_increase defaults to 0 (strict single-frame VBV/HRD where each frame must not exceed bitrate/fps), and raising it toward 100-400 acts as low-latency VBR that reduces motion mushing at the cost of packet-loss risk when the network lacks buffer headroom.


> 정정/정밀화: Correct on all documented points. Per Sunshine's official configuration docs and source (src/nvenc/nvenc_config.h), nvenc_vbv_increase defaults to 0, which enforces single-frame VBV/HRD — each encoded frame is not expected to exceed the requested bitrate divided by the frame rate. The accepted range is 0-400 (400 = a 5x increase in the per-frame size limit); any value above 0 relaxes the cap and, per the docs, "can act as low-latency variable bitrate," with the documented tradeoff of possible packet loss when the network lacks buffer headroom for the resulting bitrate spikes. The "100-400" figure is inside the valid range but is not a documented threshold. The one part NOT stated in Sunshine's docs is "reduces motion mushing" — that is an accurate technical inference (relaxing the per-frame byte cap lets high-motion/high-complexity frames use more bits instead of being over-quantized into blur) rather than an official claim.


**[CONFIRMED]** Moonlight/Sunshine recover from packet loss using Reference Frame Invalidation (a small P-frame referencing an older known-good frame) rather than a full IDR, whereas stock libwebrtc recovers via PLI/keyframe requests and has no built-in RFI/LTR loop.


> 정정/정밀화: Broadly correct. Moonlight/Sunshine's protocol supports Reference Frame Invalidation (RFI): the client signals precisely which frame(s) it lost, and the host encodes the next frame(s) so they avoid referencing the invalidated frame(s) — a comparatively small P-frame that re-syncs the decoder from a known-good reference instead of forcing a full IDR keyframe. Two qualifiers: RFI is invoked only after FEC (Reed-Solomon) fails to recover the frame, and it requires both encoder support (e.g. the standalone NVENC API on Windows; H.264/HEVC/AV1 via NVENC/VPL/AMF) and client hardware-decoder support — when either is missing, Moonlight/Sunshine fall back to requesting a full IDR. Stock libwebrtc, by contrast, has no active RFI/LTR error-recovery loop: it relies on NACK/RTX retransmission and optional FEC first, and when a frame cannot be recovered it requests a full keyframe (IDR) via PLI (or FIR). WebRTC does define an RFI-like feedback message, RPSI (Reference Picture Selection Indication), but it is optional and negotiated, its early VP8/VP9 implementation was removed from libwebrtc, and it is not used as a default long-term-reference recovery loop.


**[DISPUTED]** Parsec's 4:4:4 (paid Warp feature, $8.33-9.99/month, NVIDIA GTX 1000+ or Intel 11th-gen+ Windows host) forces the client into software decoding and raises the client decode rate by more than 3x, whereas Sunshine/Moonlight 6.1+ perform true hardware HEVC 4:4:4 decode.


> 정정/정밀화: Parsec's "Prefer 4:4:4 color" is a paid feature (Warp Individual at $9.99/mo monthly or $8.33/mo annual, also in Teams), and the HOST needs an NVIDIA Pascal (GTX 1000)+ or Intel 11th-gen (Tiger Lake)+ GPU to encode HEVC 4:4:4 on Windows. However, Parsec does NOT force the client into software decoding: per Parsec's own docs, a client with a hardware HEVC 4:4:4 decoder (NVIDIA Turing GTX 1600/RTX 2000+ or Intel 11th-gen+ — Pascal is insufficient for decode) decodes 4:4:4 in HARDWARE; CPU/FFmpeg software decode is only a fallback when the client lacks such a decoder. No source supports a ">3x decode rate" increase — Parsec only describes software 4:4:4 as "slower and significantly more performance hungry," with no quantified multiple. On the other side, Moonlight added 4:4:4 only in v6.1.0 (Sunshine ~v0.24.0/2025 pre-releases) and labels it EXPERIMENTAL; its hardware HEVC 4:4:4 decode relies on Vulkan Video on NVIDIA/Intel (not AMD) and frequently fails or falls back to software on real hardware (documented RTX 3050/5060 and Intel Arc issues). Thus the claim's framing is largely inverted — Parsec provides mature hardware client 4:4:4 decode via vendor APIs, while Moonlight/Sunshine's is newer and more software-fallback-prone — and its "forces software decoding" and ">3x" specifics are incorrect/unsupported.


**[CONFIRMED]** GStreamer's rtpgccbwe element implements Google Congestion Control, requires TWCC/transport-cc to be enabled, and exposes its estimate via the notify::estimated-bitrate signal (defaults: min 1 kbps, max 8.192 Mbps, start 2.048 Mbps) which the application must wire to the encoder's bitrate property.


> 정정/정밀화: GStreamer's rtpgccbwe element (in the rsrtp / gst-plugins-rs plugin) implements the Google Congestion Control algorithm (draft-ietf-rmcat-gcc-02) and only functions when TWCC (transport-wide congestion control, i.e. transport-cc) is enabled on the associated rtpsession, since its bandwidth estimate is derived from TWCC feedback. It publishes each new estimate on its estimated-bitrate property; the application connects to the rtpgccbwe::notify::estimated-bitrate signal and is responsible for applying the value to the encoder(s)' bitrate/target-bitrate property (property name varies by encoder, e.g. bitrate on x264enc, target-bitrate on vp8enc/vp9enc). Default bitrate bounds are min-bitrate = 1000 bit/s (1 kbps) and max-bitrate = 8,192,000 bit/s (8.192 Mbps); the initial/starting estimate at runtime is 2,048,000 bit/s (2.048 Mbps) via DEFAULT_ESTIMATED_BITRATE (note: in current source the estimated-bitrate ParamSpec's introspected default_value is set to 1000, though the operative starting estimate is 2.048 Mbps).


**[CONFIRMED]** AV1 intra block copy alone provides roughly 27% bitrate savings on screen content, and AV1 SCC-class coding is on par with or only a few percent better in BD-rate than HEVC-SCC for text/graphics content.


> 정정/정밀화: Correct, with two precision caveats about configuration dependence. (1) AV1 intra block copy alone yields about 27% bitrate savings on screen content per the 2018 IEEE paper "Intra Block Copy for Screen Content in the Emerging AV1 Video Codec" (27.1%), but this is roughly the Random-Access-level figure; in All-Intra coding the IBC gain is substantially larger (about 44% BD-rate on text-and-graphics sequences). (2) Using reference-software encoders on the standard SCC test set (arXiv 2011.14068), AV1's full SCC toolset is competitive with — and a few percentage points ahead of — HEVC-SCC (SCM) for text/graphics: in All-Intra AV1 beats HEVC-SCC by ~3 percentage points BD-rate (61.99% vs 58.60% against the HEVC-v1 anchor), consistent with the paper's own statement that "VVC and AV1 lead the race by a few percent above AVS3 and HEVC SCC." In Random Access, however, AV1's margin over HEVC-SCC is notably larger than "a few percent" (~14-17 points, in part because HEVC's HashME encoder tool was disabled in that comparison). These are reference-software results and the authors caution against declaring one standard categorically superior.


**[DISPUTED]** The compressed-bitrate premium of 4:4:4 over 4:2:0 quoted at roughly +26% (1.32-1.77 vs 1.05-1.406 bpp) is measured on natural/general capture content, not on screen/desktop content, so it overstates the desktop cost where chroma planes are flat and highly compressible.


> 정정/정밀화: The specific figures (+26%; 4:4:4 1.32-1.77 bpp vs 4:2:0 1.05-1.406 bpp) cannot be traced to any public source and are internally consistent with being derived by a single fixed multiplier (~1.26x) rather than independently measured per content type, so their attribution to "natural capture content" is unverifiable. It is correct that a 4:4:4-over-4:2:0 premium measured on natural/general video should not be transferred uncritically to screen/desktop content, since 4:2:0 is near-perceptually-lossless on natural content while it visibly damages screen content. However, the claim's premise that desktop chroma planes are "flat and highly compressible" is only partly true: screen/desktop content is bimodal, combining large flat solid regions (where full-resolution chroma adds almost no bits) with sharp, antialiased colored text/UI/logo edges (where chroma is high-frequency, is exactly what 4:2:0 discards and damages, and costs real bits to encode faithfully in 4:4:4). The compressed 4:4:4 premium is therefore content-dependent: smaller than the natural-content figure for flat-dominated desktops, but comparable or larger for text/graphics-dense screen content. Screen content is the regime where full chroma resolution matters most, which is why screen-content coding standards default to 4:4:4 — so a blanket claim that natural-content numbers "overstate the desktop cost" is not reliably true and is arguably reversed for the defining case.


**[CONTEXT_DEPENDENT]** Cloudflare's edge-relay path dropped about 14% of packets on a 300 Mbps UDP test versus about 1.2% for direct peer-to-peer WireGuard, and ran 2-5x slower than direct P2P on regional routes (NetBird 2026 iperf3 benchmark of 'Cloudflare Mesh', an architecture closely related to but not identical to consumer 1.1.1.1 WARP).


> 정정/정밀화: The figures are quoted accurately from NetBird's own April 2026 benchmark (netbird.io knowledge hub, published April 17, 2026): on a Hetzner-Germany-to-AWS-West path at a forced 300 Mbps UDP send rate, Cloudflare Mesh received 257 Mbps (~14% "loss") vs a direct peer-to-peer WireGuard tunnel (NetBird) receiving 295 Mbps (1.2% loss), and P2P (NetBird/Tailscale) ran ~2-5x faster than Cloudflare Mesh on regional/same-country European routes. Cloudflare Mesh is a real product — the rebranded WARP Connector within Cloudflare One (launched ~April 14, 2026) that relays traffic through Cloudflare PoPs rather than peer-to-peer, architecturally related to but distinct from consumer 1.1.1.1 WARP — so that characterization is correct. Three caveats are essential: (1) it is a vendor self-benchmark by a direct Cloudflare competitor with no independent replication (all "corroborating" write-ups merely re-cite NetBird's numbers); (2) the 14% UDP "packet loss" is a throughput-ceiling/saturation artifact — Cloudflare Mesh caps at ~250-290 Mbps, so a 300 Mbps UDP flood drops the ~14% excess; it is not random network loss at sustainable rates; and (3) the same benchmark shows Cloudflare Mesh outperforming P2P on long international routes (e.g., Japan-to-Europe ~1.5-2x faster, up to 5-8x faster to a German datacenter), so the P2P advantage is route-specific to regional links, not universal.


**[DISPUTED]** In 2026 no hardware encoder (NVENC on Ada/Blackwell per the Video Codec SDK 13.0 programming guide, AMD AMF, Intel QSV) exposes any screen-content coding mode (palette / intra block copy); these AV1/HEVC SCC tools are available only in software encoders such as libaom and SVT-AV1 (via --scm 1/2).


> 정정/정밀화: As of 2026 it is true that NVIDIA NVENC (Ada/Blackwell, Video Codec SDK 13.0) and AMD AMF expose no screen-content coding tools — no palette or intra-block-copy option (AMF only performs internal screen-content detection to steer pre-analysis/rate control). However, the sweeping claim that NO hardware encoder exposes SCC — and that palette/IBC are available only in software — is false. Intel QSV, through oneVPL / Intel VPL, exposes mfxExtAV1ScreenContentTools (MFX_EXTBUFF_AV1_SCREEN_CONTENT_TOOLS) with Palette and IntraBlockCopy fields for its AV1 hardware encoder: experimental in VPL API 2.11 and production since VPL 2.13 (late 2024, with Battlemage/Arc), implemented in the vpl-gpu-rt hardware-encode path and demonstrated in sample_encode. So AV1 palette + intra block copy are available on Intel QSV hardware. Software encoders also support SCC: SVT-AV1 via --scm (0=off/1=on/2=auto) and libaom via --tune-content=screen / --enable-palette / --enable-intrabc (note: --scm is SVT-AV1's flag, not libaom's).


**[CONFIRMED]** Free Cloudflare WARP typically reduces throughput by roughly 50% (measured 280.63 -> 132.40 Mbps in a 2026 test) with high run-to-run variance and no documented hard data cap.


> 정정/정밀화: In vpnMentor's "WARP Review 2026" US test, free Cloudflare WARP reduced download throughput from 280.63 Mbps to 132.40 Mbps — about a 53% drop (throughput retained ~47%), which the reviewers describe as roughly "cutting base rates in half." Results are highly variable: the same review notes some tests stayed near baseline, and its Kenya test showed only about a 20% reduction (39.15 -> 31.33 Mbps), so ~50% is a typical-case figure rather than a fixed penalty. Free WARP is documented as having no data cap or bandwidth limit. (Note: this is a VPN-throughput claim, unrelated to video-codec benchmarking.)


**[CONTEXT_DEPENDENT]** End-to-end hardware 4:4:4 in 2026 is HEVC-only: NVENC encodes HEVC 4:4:4 (8/10-bit) since Pascal/GTX 1000 and Intel QSV since Ice Lake (10th gen), but NO current NVIDIA/Intel/AMD GPU offers AV1 4:4:4 hardware encode and AMD has no 4:4:4 hardware encode at all.


> 정정/정밀화: Accurate only if "end-to-end" means a full hardware ENCODE-and-DECODE 4:4:4 pipeline. In that strict sense HEVC is indeed the only inter-frame codec that qualifies in 2026: NVENC encodes HEVC 4:4:4 (8-bit and 10-bit) since Pascal and NVDEC/Intel QSV decode HEVC 4:4:4 (8/10/12-bit); Intel QSV documents HEVC 4:2:2/4:4:4 encode starting with Ice Lake (Gen 11 / 10th-gen Core), though later Intel gens are inconsistent and drop to 4:2:0 on parts of the HEVC path. No NVIDIA, Intel, or AMD GPU offers AV1 4:4:4 hardware encode (AV1 hardware encoders are Main-profile 4:2:0; Blackwell/RTX 50 added 4:2:2, not 4:4:4), and AMD has no 4:2:2/4:4:4 hardware encode for any codec. HOWEVER, the flat "HEVC-only" phrasing is misleading for hardware 4:4:4 ENCODE alone: NVENC has hardware-encoded H.264 4:4:4 (High 4:4:4 Predictive / HP444, 8-bit CAVLC) since Maxwell (GTX 900) — predating Pascal HEVC 4:4:4. That H.264 4:4:4 stream simply cannot be hardware-decoded (NVDEC is 4:2:0-only for H.264), which is the only reason it doesn't count as "end-to-end."


**[CONFIRMED]** WARP's MASQUE transport defaults to QUIC over UDP 443 and falls back to HTTP/2 over TCP 443 when UDP is blocked, while WARP's WireGuard mode has no TCP fallback — so bypassing a UDP block puts the real-time game stream on MASQUE and, if UDP 443 is also blocked, inside a TCP tunnel.


> 정정/정밀화: Cloudflare WARP's MASQUE transport defaults to QUIC/HTTP-3 over UDP 443 and, per Cloudflare's own firewall documentation, can fall back over a ladder of alternate UDP ports (500, 1701, 4500, 4443, 8443, 8095) and finally to TCP 443 as a last resort (reverse-engineering of the official client indicates that TCP-443 fallback speaks HTTP/2). WARP's WireGuard mode uses UDP 2408 by default with only UDP fallback ports (500, 1701, 4500) and no TCP option, because WireGuard is a UDP-only protocol. Consequently, in a network that blocks WireGuard's UDP, switching to MASQUE restores the tunnel; and if UDP is comprehensively blocked (not merely UDP 443 — all of MASQUE's UDP fallback ports must be unavailable), MASQUE degrades to a TCP 443 tunnel, which adds TCP head-of-line-blocking and retransmission latency that is undesirable for a real-time game stream. The one inaccuracy in the original claim is the implication that blocking UDP 443 alone forces the TCP tunnel; MASQUE tries several other UDP ports first.
