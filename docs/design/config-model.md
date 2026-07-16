# BetterParsec 통합 앱 — 설정(Config) 모델

**Date**: 2026-07-16 (구현 반영본 — ralplan run 019f68d6 G005/G006)
**Owner 방향**: Sunshine/Moonlight 틀을 벗어나 우리 앱으로 통합. 설정도 그 앱의 것.
**제약 (owner)**: "너무 어려우면 안 된다" — **일반 사용자가 설정 파일이나 env를 만질 일이 0**이어야 한다.

> 이 문서는 이전의 5-프리셋 안을 **폐기**하고 shipped 구현(`transport-core/src/mode.rs`,
> `app-native/src/settings.rs`)의 **3단 모드 + free-lunch 기본 ON** 모델로 대체한다.

---

## 0. 북극성 — 제로컨피그 기본 경로

전체 여정에서 **타이핑하는 건 비밀번호 하나뿐**: 다운로드 → 실행 → 로그인 → 호스트 클릭 → 스트림.
설정 파일 편집 없음, env 없음, 배치파일 없음. 튜닝 노브는 전부 기본값이 "그냥 됨" + 필요 시 인앱 조정.

---

## 1. 두 부류로 접힌 설정

### 1-1. 화질 무료 점심 = 코드 기본값 (토글 아님)
지연/비트레이트 비용이 사용자가 체감할 수 없을 만큼 ~0인 화질 레버는 **항상 켜진 상수**다.
`transport-core/src/mode.rs`의 private `free_lunch` 모듈: `SPATIAL_AQ_ON=true`, `PRESET`,
`WEIGHTED_PRED_ON=true`. `ModeKnobs`에 슬롯이 없고 모드/사용자로 바뀌지 않는다. (인코더 소비는
후속 wiring goal — 현재는 예약 상수.)

### 1-2. 사용자 선택 = 진짜 트레이드오프만
`StreamMode { Fast, Medium, Quality }` 3단 + 4개 per-field override
(`UserTradeoffs { bitrate_kbps, width, height, fps }`). 모드가 기본을 정하고, **0이 아닌
사용자 필드만** 그 필드를 덮어쓴다(free-lunch 상수는 불변). `pub fn knobs_for(mode, user) -> ModeKnobs`.

**shipped 모드 기본값** (`mode.rs::mode_defaults`, `canonical_vectors()`로 웹과 락스텝 검증):

| 모드 | codec_pref | 해상도 | bitrate | fps | audio_exclusive_default |
|---|---|---|---|---|---|
| Fast (최저지연) | H264 | 1280×720 | 5 Mbps | 60 | on |
| Medium (기본) | HEVC | 1920×1080 | 8 Mbps | 60 | on |
| Quality | AV1 | 3840×2160 | 25 Mbps | 60 | off |

- 상수 값은 Gate-B/C 튜닝 대상(resolution.rs/fec_ratio.rs 선례).
- `codec_pref`는 `CodecPref{H264,Hevc,Av1}` 순수 서수 enum — `FlowConfig::supported_codecs`
  비트마스크 flip은 **호스트+라이브 후속 goal**(현재는 전부 H264_BIT로 collapse, 문서화됨).
- 웹 미러: `web/stream/mode.ts` `knobsFor`/`canonicalVectors` — `tests/mode.test.mjs`가 Rust
  `canonical_vectors()` 값과 **정확 일치** 검증(단일 공유 벡터 테이블).

---

## 2. 사용자 스토어 (`settings.rs`)

`%APPDATA%/betterparsec/settings.json` (비Windows: `$XDG_CONFIG_HOME`/`$HOME/.config/betterparsec/`).
**인앱 UI만 기록**한다. human-json(주석 허용, `web_server::human_json::preprocess_human_json` 재사용).

```jsonc
{
  "schema": 1,
  "client": {
    "mode": "medium",        // fast|medium|quality (미지값 → medium)
    "bitrate_kbps": 8000,    // 0 = 모드 기본 사용
    "width": 1920, "height": 1080, "fps": 60,
    "present_10bit": false,  // BP_PRESENT_10BIT env가 위에서 덮음
    "client_cursor": false   // BP_CLIENT_CURSOR env가 위에서 덮음
  }
  // 알 수 없는 top-level 키는 보존(#[serde(flatten)] — 구/신 빌드 왕복에 필드 유실 없음)
}
```

- **schema 버전 + 미지 필드 보존**으로 마이그레이션·포워드 호환.
- **손상 파일**: `settings.json.bak-<unix_ts>`로 백업 후 기본값 재생성 — 채워진 파일을 백업
  없이 조용히 덮지 않는다(host.rs 원칙 계승; 단 user 스토어라 hard-error 대신 백업+재생성).
- 파일 부재 = 최초 실행 = 기본값(에러 아님). 사용 가능한 base dir 없으면(헤드리스/CI)
  디스크 미접촉 in-memory 기본값.
- `to_flowconfig_fields(&ClientSettings)`가 `knobs_for`로 FlowConfig 필드 파생 → 스토어 편집과
  fresh install이 mode 엔진과 비트단위 일치.

---

## 3. 우선순위 (명시적, 단일 방향)

**낮음 → 높음**: builtin 기본값 < 배포 시드 `betterparsec.conf` < 사용자 스토어 <
연결별 < **dev env (`BP_*`, 최상)**.

- env가 **최상**(dev override) — shipped `ConnectForm::default`("env > conf > fallback")와
  `present_10bit_from_env` / `BP_CLIENT_CURSOR`가 스토어 값 위에서 OR로 folding.
- 스토어는 자기 슬롯만 소유 — `betterparsec.conf`나 `BP_*`를 스스로 읽지 않고, 호출부가
  precedence를 위에 얹는다.
- **일반 사용자는 env를 만질 일이 0**: sharpen·NV12·exclusive audio·10-bit·client cursor는
  전부 인앱 토글(env는 개발용 override로 잔존).

---

## 4. 호스트 설정 통합 (후속)

호스트 노브(encoder preset/AQ·capture_cursor·멀티모니터·프라이버시)는 아직 Sunshine
`sunshine.conf`에 있다. 통합 방향(Phase A: Sunshine 서브프로세스): 사용자 스토어가 단일
진실 소스 → 앱 호스트 롤이 `sunshine.conf`를 **생성**(hand-edit 금지). Phase B(자체 인코더):
conf 소멸, 호스트 스키마를 스토어에 흡수. 코덱 HEVC/AV1 기본 전환·host AQ는 라이브/호스트 goal.

---

## 5. 판정 기준

- 신규 사용자가 **설정 파일/​env 없이** 스트림 도달? (필수 — 충족)
- 화질/지연 레버가 **전부 인앱**에서? (env 잔존은 dev-only — 충족: sharpen/NV12/exclusive/10bit/cursor)
- **모드 하나**로 게임/영상 바뀜? (fast/medium/quality — **부분**: 해상도·비트레이트·
  audio_exclusive 기본은 모드 전환 시 실제로 바뀜(웹·네이티브 락스텝, 0-센티널 override).
  코덱 flip은 H264로 collapse(호스트+라이브 후속))
- 호스트 운영자가 **sunshine.conf 미접촉**으로 모니터/프라이버시/인코더? (후속)
- 새 기기 **재로그인만으로** 이전 프리셋 복원? (계정 동기화 후속)
