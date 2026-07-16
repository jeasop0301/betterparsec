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

## 4. 호스트 역할/설정 스키마 (G005 구현 반영)

`role: client|host|both` 필드가 스토어 최상위에 추가됐다(`settings::Role`,
`#[serde(rename_all = "lowercase")]` → 디스크에는 `"client"`/`"host"`/`"both"`). 기본값은
`Client`(오늘의 동작과 동일) — `Settings`의 `#[serde(default)]`가 이 필드가 없는 구버전
스토어도 하드 실패 없이 파싱해 `Role::Client`로 채운다(`legacy_store_missing_role_and_host_defaults_client`
테스트로 고정). `host` 섹션(`settings::HostSettings`)이 함께 추가된다:

```jsonc
{
  "schema": 1,
  "role": "client",          // client|host|both — 기본 client, 구버전 스토어 호환
  "client": { /* §2 그대로 */ },
  "host": {
    "paths": {
      // 전부 %APPDATA%/betterparsec 아래 — HostSupervisionCore 소비 계약(IRC 합의):
      // paths.sunshine_stage_root / paths.sunshine_identity_dir / paths.logs_dir
      "config_path": "%APPDATA%/betterparsec/host/config.json",
      "sunshine_stage_root": "%APPDATA%/betterparsec/host/sunshine",
      "sunshine_identity_dir": "%APPDATA%/betterparsec/host/identity",
      "logs_dir": "%APPDATA%/betterparsec/logs",
      "updates_dir": "%APPDATA%/betterparsec/updates"
    },
    "network": { "web_port": 8080, "moonlight_port": 47989 },
    "update": { "channel": "", "url": "" }   // 자리표시자 — 실제 업데이트 로직은 G006
  }
  // 알 수 없는 top-level 키와 host 바로 아래의 알 수 없는 키는 여전히 보존
  // (§2 flatten 원칙 — Settings.unknown / HostSettings.unknown).
  // host.paths/network/update *내부*의 알 수 없는 키는 보존하지 않는다:
  // 그 구조체들은 닫힌 스키마이며, 새 노브는 명시적 필드 추가로만 들어온다.
}
```

- `paths`/`network`/`update` 세 그룹 모두 `#[serde(default)]` — 부분적으로만 채워진 스토어도
  나머지는 기본값으로 채워져 파싱된다.
- `network` 기본값은 기존 웹서버/문라이트 기본값과 동일(`common::config::WebServerConfig`
  bind 8080, `MoonlightConfig::default_http_port` 47989) — 스토어 기본값과 서버 기본값이
  갈라지지 않는다.
- 우선순위(§3)는 host 섹션에도 그대로 적용: builtin < 배포 시드 < **사용자 스토어**(host 포함)
  < 연결별 < dev env. dev env는 여전히 override 전용이며 host 섹션을 스스로 읽지 않는다.

### 4-1. 생성되는 자식 설정 (child config) — 원자적·멱등

Phase A(Sunshine 서브프로세스)의 실행 계약: **사용자는 생성된 파일을 절대 손으로 편집하지
않는다**. `settings::generate_child_config(&Settings) -> String`이 순수 함수로 실행 시
Sunshine/streamer가 읽을 설정 텍스트를 스토어에서 파생한다(포트는 `host.network`, 비트레이트/
해상도/fps는 `to_flowconfig_fields`를 재사용해 클라이언트 세션과 비트단위로 일치). 동일한
`Settings` 입력 → 항상 바이트 단위로 동일한 출력(`generated_child_config_is_deterministic`
테스트) — 타임스탬프·난수 없음.

`settings::write_child_config_atomic(path, content)`가 디스크 반영을 담당: 같은 디렉터리에
임시 파일을 쓰고 `rename`으로 교체한다(같은 볼륨이므로 POSIX/Win32 모두 원자적) — 리더가
반쪽짜리 파일을 절대 보지 않고, rename 실패 시 임시 파일을 정리한다(`write_child_config_atomic_writes_content_and_leaves_no_temp_file`).
매 호스트 시작마다 재생성해도 안전(`write_child_config_atomic_regeneration_is_idempotent`) —
호스트 롤이 매번 새로 써도 이전 파일과 최종 상태가 같다.

### 4-2. 아이덴티티 마이그레이션 — 백업 우선, 바이트 검증, 원자적 스위치

`identity.rs`가 기존(비관리) Sunshine 아이덴티티(인증서/state/apps,
`sunshine.rs::SunshineConfig`의 `identity_source`와 동일 형태)와 독립 실행 서버의
계정/페어링 설정(`host.rs::DEFAULT_CONFIG_PATH`)을 관리 레이아웃(`HostSettings.paths`)으로
이관한다. 레거시 소스가 남아 있는 한 호스트 롤 시작마다 재수행되며(멱등 — 소스가 비면
no-op), 순서(모두 실패 시 이전 단계로 롤백):

1. **(a) 스냅샷 우선**: 목적지에 이미 있던 상태(직전 이관 결과 또는 그 이전 설치)를 전부
   `.backup-lkg/`로 백업한 뒤에만 다음 단계로 진행한다.
2. **(b) 바이트 그대로 복사**: 소스를 재생성/재직렬화하지 않고 `.staging/`에 그대로 복사한다
   (state.json/apps.json/config.json도 파싱 후 재기록하지 않음 — 포맷을 소유하지 않은 파일은
   절대 건드리지 않는다).
3. **(c) 검증**: 스테이징된 모든 파일을 소스와 콘텐츠 해시로 비교하고, `.json` 확장자 파일은
   추가로 파싱 검증한다(바이트가 같아도 둘 다 손상된 경우까지 잡아낸다 —
   `corrupt_source_json_fails_verification_and_rolls_back`).
4. **(d) 원자적 스위치**: 검증을 통과한 뒤에만 `.staging/` 내용을 목적지 위로 `rename`한다.
5. **(e) 실패/중단 시 롤백**: 위 어느 단계든 실패하면 즉시 백업으로 복원한다. 크래시로 프로세스가
   죽어 정리가 못 끝난 경우엔 `.migration-journal.json` 마커가 남아 있고, 다음 시작 시
   `recover_on_start`가 마커 존재만으로 무조건 롤백한다(`interrupted_migration_rolls_back_to_last_known_good_on_next_start`
   — 저널 중간 상태를 인위적으로 재현해 검증).
6. **백업은 성공해도 삭제하지 않는다** — `.backup-lkg/`는 직전 성공 이관 시점의 목적지
   상태(last-known-good)를 담고, 재이관 성공 시에만 그 시점 상태로 교체된다
   (`successful_migration_keeps_backup_of_prior_state`).

비밀(인증서/키/페어링 토큰)은 파일 내용으로만 존재하고, 이 모듈은 경로/개수만 로그로 남긴다
(`tracing::info!(files_migrated, dest, ..)`) — 실제 바이트를 로그에 남기지 않는다. 로그
레벨의 추가 redaction(민감한 다른 필드)은 managed-host supervision lane(`host.rs`/
`sunshine.rs`)의 책임이며 이 모듈의 스코프가 아니다.

`identity.rs`는 스키마와 함수를 노출하고(`MigrationPaths`, `migrate`, `recover_on_start`,
`rollback`), `host.rs::start()`가 호스트 롤 부팅 시 이를 직접 호출한다:
`recover_on_start`(저널이 남아 있으면 무조건 롤백) → 레거시 소스가 있으면 `migrate` →
`generate_child_config`+`write_child_config_atomic`. `main.rs`의 시작 버튼이
`host::start`를 부르므로 이 배선은 G005에 이미 포함된 동작이다(역할 UI 자체는 G006).

---

## 5. 판정 기준

- 신규 사용자가 **설정 파일/​env 없이** 스트림 도달? (필수 — 충족)
- 화질/지연 레버가 **전부 인앱**에서? (env 잔존은 dev-only — 충족: sharpen/NV12/exclusive/10bit/cursor)
- **모드 하나**로 게임/영상 바뀜? (fast/medium/quality — **부분**: 해상도·비트레이트·
  audio_exclusive 기본은 모드 전환 시 실제로 바뀜(웹·네이티브 락스텝, 0-센티널 override).
  코덱 flip은 H264로 collapse(호스트+라이브 후속))
- 호스트 운영자가 **sunshine.conf 미접촉**으로 모니터/프라이버시/인코더? (**부분**: 스키마/생성/
  마이그레이션 배관은 §4로 완성 — `generate_child_config`가 포트/비트레이트를 스토어에서
  생성, `identity.rs`가 기존 설치를 백업 우선으로 관리 레이아웃에 이관; encoder preset/AQ/
  capture_cursor/멀티모니터 세부 노브를 host 스키마에 추가하고 실제로 소비하는 건 호스트 롤
  UI 배선과 함께 후속(G006))
- 새 기기 **재로그인만으로** 이전 프리셋 복원? (계정 동기화 후속)
