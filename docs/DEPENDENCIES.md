# BetterParsec patched dependency policy

BetterParsec의 동작에 필요한 Moonlight 변경은 더 이상 저장소 밖의
`../vendor-mlc-rust` 작업 트리에 의존하지 않는다. 재현 가능한 source of truth는 다음
세 요소다.

1. 고정 upstream revision
2. 이 저장소에 commit되는 patch
3. 동일 과정을 수행하는 bootstrap script와 CI

## 고정 revision

| Dependency | Upstream | Revision |
| --- | --- | --- |
| moonlight-common-rust | `MrCreativ3001/moonlight-common-rust` | `df9f1e3003fb4834dbb17a4bd4d3cf25d2fea3d9` |
| moonlight-common-c | `moonlight-stream/moonlight-common-c` | `62687809b1f7410c3db4be2527503a54ae408d70` |

`moonlight-common-c` revision은 위 Rust repository의 submodule pin과도 일치한다.

## repository-owned patches

- `patches/moonlight-common-rust.patch`
  - pairing certificate serial fix
  - localhost Sunshine pairing TLS verifier compatibility
  - typed `MoonlightStream::change_bitrate()` boundary
  - presentation timestamp microsecond conversion fix
  - host processing latency 0.1 ms conversion fix와 tests
- `patches/moonlight-common-c.patch`
  - capability-gated `LiChangeBitrate()`
  - reliable encrypted `0x5506` bitrate request
  - provisional `LI_FF_DYNAMIC_BITRATE (0x40)`

Foundation Sunshine host-side capability patch는 별도로
`docs/host-patches/foundation-sunshine-dynamic-bitrate-capability.patch`에 있다.

## bootstrap

Windows PowerShell:

```powershell
pwsh ./tools/bootstrap-dependencies.ps1
```

Linux, macOS, WSL, Git Bash:

```sh
bash tools/bootstrap-dependencies.sh
```

스크립트는 `vendor/moonlight-common-rust`를 생성한다. 이 디렉터리는 build artifact와
같이 `.gitignore` 대상이다. bootstrap은 다음을 검증한다.

- 두 repository의 정확한 HEAD
- 두 patch가 실제로 적용됐는지 reverse `git apply --check`
- 중단된 첫 실행으로 정확한 clean base만 남았으면 삭제 없이 patch를 이어서 적용
- 이미 준비된 상태면 아무것도 바꾸지 않는 idempotent 동작

`build-windows.ps1`과 GitHub Actions CI는 bootstrap을 먼저 실행한다.

## 왜 generated vendor를 commit하지 않나

전체 third-party source snapshot을 이 repository에 복제하면 upstream diff review와
라이선스 경계가 흐려지고 불필요한 수천 파일이 생긴다. 반대로 로컬 path만 참조하면
새 PC와 CI가 빌드할 수 없다. pinned revision + small patches 방식은 다음을 동시에
보장한다.

- clean clone 재현성
- 실제 BetterParsec 변경만 code review 가능
- upstream revision 변경이 명시적
- Rust와 C patch를 독립적으로 검증 가능

## dependency 갱신 절차

1. 새 upstream revision을 임시 clean clone에서 checkout한다.
2. 기존 patch를 `git apply --check`한다.
3. conflict를 수정하고 patch를 다시 생성한다.
4. 두 bootstrap script의 revision을 함께 바꾼다.
5. `Cargo.toml`의 upstream dependency revision과 일치시킨다.
6. clean directory에서 bootstrap을 실행한다.
7. `cargo test --workspace --locked`, `npm run test:stats`, Windows build를 실행한다.
8. Foundation paired live smoke에서 timestamp, host latency, capability gate,
   runtime bitrate apply를 다시 확인한다.

patch가 더 이상 작고 명확하지 않거나 upstream과 장기간 분기한다면, 그 시점에만
별도 public fork + immutable commit SHA 방식으로 전환한다.
