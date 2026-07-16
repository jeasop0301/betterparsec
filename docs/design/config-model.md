# BetterParsec 통합 앱 — 설정(Config) 모델 설계

**Date**: 2026-07-16
**Owner 방향**: Sunshine/Moonlight 틀을 벗어나 **우리 코드·우리 앱**으로 통합
(unified-app-architecture.md D1). 설정도 그 앱의 것으로 통합한다.
**제약 (owner)**: "너무 어려우면 사용하기 쉽지 않다" — **일반 사용자가 설정
파일이나 env를 만질 일이 0이어야 한다.**

---

## 0. 북극성 — 제로컨피그 기본 경로 (Parsec 패리티)

일반 사용자의 전체 여정에서 **타이핑하는 건 비밀번호 하나뿐**이어야 한다.

```
다운로드 → 실행 → 로그인 → 내 호스트 목록에서 클릭 → 스트림
```

- 설정 파일 편집 없음. env 없음. 배치파일 없음.
- 튜닝 노브는 전부 **기본값이 "그냥 됨"** + 필요 시 인앱에서 조정.
- 이 경로가 깨지면 어떤 고급 기능도 의미 없다. **모든 설정 결정은 이
  경로를 해치지 않는지로 먼저 판정한다.**

현재 이 경로의 실제 마찰(2026-07-16 실측):
- 접속폼 프리필은 됐다(`betterparsec.conf`, 9def901) — 반은 온 셈.
- 그러나 저지연/화질 레버가 `BP_*` env에 갇혀 있어 더블클릭 배포판에서
  도달 불가였다(exclusive 오디오 등 — 오늘 인앱 토글로 이관 시작).
- 호스트는 여전히 Sunshine `sunshine.conf`를 따로 만져야 함.

---

## 1. 현재 설정 표면 — 파편화 진단

| 레이어 | 위치·형식 | 누가 만지나 | 문제 |
|---|---|---|---|
| 클라 접속 | `betterparsec.exe` 옆 `betterparsec.conf` (key=value) | 패키저가 생성, 사용자 안 만짐 | 배포 시드용으로만 OK. 계정·다중 호스트엔 부족 |
| 클라 기능 플래그 | `BP_*` env (`BP_AUDIO_EXCLUSIVE`, `BP_SHARPEN`, `BP_NV12`, `BP_CLIENT_CURSOR`, …) | 개발자만 | **더블클릭 배포판에서 도달 불가** — 필드 판정 블로커 |
| 클라 인앱 | egui 토글 (sharpen·NV12·exclusive audio) | 사용자 | 방향 맞음 — 여기로 수렴시켜야 |
| 서버 | `./server/config.json` (human-json) | 호스트 운영자 | bind/port/계정·페어링. 통합 앱이 임베드(D3) |
| 호스트 인코드/캡처 | Foundation `sunshine.conf` | 호스트 운영자 | capture_cursor·slice_aligned_fec·encoder — **별도 틀** |
| 웹 클라 | 브라우저 로컬 | 사용자 | 네이티브와 갈라짐 |

**핵심 문제**: 같은 개념(예: 비트레이트·코덱·커서 정책)이 3~4곳에 흩어져
있고, 저지연/화질 레버가 사용자 손이 안 닿는 env에 있다.

---

## 2. 목표 모델 — "하나의 앱, 하나의 설정, 접힌 복잡도"

### 2-1. 레이어 & 우선순위 (명시적, 단일 방향)

```
built-in 기본값  (코드, "그냥 됨")
  ▼ override
배포 시드        (betterparsec.conf — 패키저가 심음, 최초 실행 시 사용자
                  스토어로 1회 이주 후 역할 종료)
  ▼ override
사용자 스토어    (%APPDATA%/betterparsec/settings.json — 인앱 UI만 기록)
  ▼ override
연결별 기억값    (호스트마다 마지막 선택 프리셋/해상도 등)
  ▼ override
dev/CI env       (BP_* — 개발·자동화 전용, 일반 사용자엔 비노출·불필요)
```

- **사용자 스토어는 인앱 UI만 쓴다.** 손편집을 기대하지 않는다(가능은 하되
  문서화된 정식 경로가 아님).
- **env는 개발 override로 강등.** 모든 사용자용 기능은 인앱 토글이 정식.
  (오늘 exclusive audio가 그 첫 이관. 잔여: `BP_CLIENT_CURSOR` →
  커서 설정, `BP_SHARPEN`/`BP_NV12`는 이미 토글 존재해 env는 시드로만.)

### 2-2. 프리셋 우선, 노브는 접기 (progressive disclosure)

일반 사용자에게 15개 노브를 던지지 않는다. **이름 있는 프리셋**이 노브
묶음을 정한다:

| 프리셋 | 겨냥 | 묶는 것 |
|---|---|---|
| **자동 (기본)** | 대부분 | 링크 측정으로 적응(동적 해상도·FEC 비율·비트레이트 CC) |
| **게임 (최저지연)** | 경쟁 게임 | exclusive 오디오·NV12 프레젠트·present 페이싱=최저지연·FEC 낮게·immersive 유도 |
| **화질 우선** | 영상·데스크톱 | 코덱 HEVC/AV1·preset 상향·AQ on·비트레이트 여유·페이싱=지터흡수 |
| **나쁜 네트워크** | 제약망 | FEC 비율 높게·동적 해상도 공격적·비트레이트 보수적·TCP 폴백 관대 |
| **배터리 절약** | 랩탑 | 디코드 부하↓·fps 상한·샤픈 off |

- 기본 = **자동**. 사용자는 프리셋 하나만 고르면 끝.
- "고급" 아코디언을 펼치면 개별 노브(코덱·비트레이트·FEC·해상도 사다리·
  오디오 모드·페이싱·샤픈)가 프리셋 값을 시드로 노출. 만지는 순간
  "커스텀"으로 분기.
- **프리셋은 클라 단독 관심사와 호스트 협상 관심사를 나눠 담는다** —
  아래 3절.

---

## 3. 호스트 설정의 통합 (Sunshine 탈출의 핵심)

지금 호스트 노브(encoder preset·AQ·codec·capture_cursor·slice_aligned_fec·
멀티모니터·프라이버시·서라운드)는 `sunshine.conf`에 산다. 통합 앱 방향은
이걸 **우리 앱의 "호스트" 탭 + 사용자 스토어**로 가져오는 것이다.

**단계적 이주 (D2: Phase A=Sunshine 서브프로세스 → Phase B=자체 인코더)**:

- **Phase A (지금)**: 사용자 스토어가 **단일 진실 소스**. 앱이 호스트 롤로
  뜰 때 스토어의 호스트 설정 → **`sunshine.conf`를 생성**(hand-edit 금지,
  생성 산출물로 강등). `app-native/src/host.rs`가 이미
  `./server/config.json`을 로드하니, 동일 지점에서 sunshine.conf도
  스토어에서 렌더링. 사용자는 Sunshine conf를 절대 직접 안 만진다.
- **Phase B (자체 인코더)**: sunshine.conf 자체가 사라지고 호스트 설정이
  네이티브로 앱 안에서 산다. 스키마는 그대로 재사용(아래 3-1).

### 3-1. 협상되는 값 vs 로컬 값

설정을 두 부류로 분리해 저장·전달을 단순화:

- **협상 값 (클라 프리셋 → 세션 시작 시 호스트에 요청)**: 코덱·해상도·
  fps·비트레이트 상한·HDR·서라운드 채널수·4:4:4. 이미 `StartStream`이
  `supported_codecs`·`bitrate_kbps`를 나른다(streamer). 여기에 프리셋이
  녹아든다 — **클라가 원하는 프로파일을 보내고 호스트가 능력과 교집합**.
- **호스트 로컬 값 (호스트 스토어에만)**: 어떤 모니터를 캡처·프라이버시
  블랭크·encoder preset/AQ·capture_cursor. 클라가 정할 수 없는 것.

이 분리로 "클라 설정"과 "호스트 설정"이 UI에서도 자연히 갈린다(클라 탭 /
호스트 탭), 사용자 혼란 감소.

---

## 4. 스키마 스케치 (사용자 스토어)

```jsonc
// %APPDATA%/betterparsec/settings.json  (human-json: 주석 허용)
{
  "schema": 1,
  "account": {                 // 재로그인-only 북극성 지원
    "base_url": "https://…:8080",
    "username": "…",
    // 비밀번호는 절대 저장 안 함 (OS 자격증명 저장소 옵트인은 별도)
    "remember": true
  },
  "hosts": [                   // 다중 호스트, 클릭-투-커넥트
    { "id": 2062835576, "app_id": 881448767, "label": "집 데스크톱",
      "last_preset": "game", "last_resolution": "1440p" }
  ],
  "client": {
    "preset": "auto",          // auto|game|quality|badnet|battery|custom
    "custom": {                // preset=custom일 때만 유효
      "codec": "auto",         // auto|h264|hevc|av1
      "max_bitrate_kbps": 0,   // 0=자동
      "resolution": "auto",
      "audio_exclusive": false,
      "nv12_present": false,
      "sharpen_pct": 0,
      "pacing": "balanced"     // lowlatency|balanced|smooth
    }
  },
  "host": {                    // 호스트 롤일 때만
    "capture_monitor": 0,
    "privacy_blank": false,
    "encoder": { "preset": "auto", "spatial_aq": true },
    "capture_cursor": false    // clientCursor와 짝
  },
  "dev": {}                    // env override 미러(개발 편의), 비어있음이 정상
}
```

- **`schema` 버전**으로 마이그레이션. 미지 필드는 보존(포워드 호환).
- 파싱 실패는 **표면화**(silent 기본값 override 금지 — host.rs가 이미
  이 원칙). 단 사용자 스토어는 손상 시 백업 후 기본값 재생성 옵션.

---

## 5. 실행 계획 (증분, 북극성 안 깨기)

1. **[소형·지금 이관 중] `BP_*` → 인앱 토글**: sharpen·NV12·exclusive
   audio 완료. 잔여 = `BP_CLIENT_CURSOR`를 커서 설정 토글로. env는 dev
   override로 남기되 문서에서 "개발용"으로 명시.
2. **[소형] `betterparsec.conf` → 사용자 스토어 이주**: 최초 실행 시
   conf를 읽어 `settings.json` 시드 → 이후 인앱 계정/호스트 목록이 주도.
   conf는 배포 시드 역할만.
3. **[중형] 프리셋 엔진**: `transport-core`에 순수 `Preset → 노브 묶음`
   매핑(테스트 가능, dyn-resolution/fec_ratio 컨트롤러와 결선). egui/웹
   공통 — 한 소스, 두 프론트(session-ux 패턴).
4. **[중형] 호스트 설정 → 스토어 → sunshine.conf 생성**: host.rs가
   스토어에서 sunshine.conf 렌더. 사용자는 Sunshine conf 미접촉.
5. **[대형·Phase B] 자체 인코더 전환 시** 호스트 스키마 재사용, sunshine
   conf 소멸.
6. **[선택] 계정 동기화**: 사용자 스토어를 계정에 저장 → 새 기기
   다운로드 시 프리셋/호스트 목록 복원(무설치 재로그인 강화).

---

## 6. 판정 기준 (이 설계가 "쉬운가")

- 신규 사용자가 **설정 파일/​env를 한 번도 안 만지고** 스트림에 도달하는가? (필수)
- 저지연/화질 레버가 **전부 인앱에서** 켜지는가? (env 잔존 = 실패)
- 프리셋 하나로 "게임/영상/나쁜망"이 **의도대로** 바뀌는가?
- 호스트 운영자가 **sunshine.conf를 직접 안 만지고** 모니터/프라이버시/
  인코더를 바꾸는가?
- 새 기기에서 **재로그인만으로** 이전 프리셋·호스트가 돌아오는가? (선택)
