# G005/G006 QA + review report (ultragoal run 019f68d6, commits d175400, 20a1536, fa25162)

Surface: algorithm/package (Rust settings.rs unit tests + web mode.ts node --test + doc-code consistency cross-check).

## Suite pass counts (QA agent 15-G5G6QA + leader after P1 fix)
- app-native --features video: 71 (settings:: 9 after the P1 guard added, present, integration) passed
- web node --test tests/mode.test.mjs: 10/10 passed (TS canonicalVectors == Rust canonical_vectors() exactly)
- clippy clean, fmt applied

## Architect review (14-G5G6ArchReview -> P1 -> 16-P1Confirm)
- Initial: productStatus BLOCK (P1) — ClientSettings::default() seeded concrete Medium numbers, masking the mode selector on default install.
- Fix fa25162: seed 0-sentinels; first run resolves to Medium, mode switch now changes resolved fields; guard test added.
- Confirm 16-P1Confirm: architecture/product/code all CLEAR, APPROVE, P1 resolved.

## Adversarial coverage matrix (settings.rs)
- missing file -> defaults: COVERED (load_missing_returns_defaults)
- corrupt present -> backup .bak-<ts> created + defaults returned (no silent overwrite): COVERED (corrupt_backs_up_and_regenerates)
- unknown top-level keys survive save+load: COVERED (unknown_fields_preserved_roundtrip)
- to_flowconfig_fields Quality mode + bitrate override: COVERED
- DEFAULT-path mode switch changes resolved fields (P1 guard): COVERED (default_store_mode_switch_changes_flowconfig)
- mode serde unknown string -> Medium: COVERED
- client_cursor default off + persist: COVERED
- no panic paths (default_settings_path None on unset env, backup_corrupt unwrap_or(0), serde exhaustive match): inspection-verified
- gaps (non-blocking): genuine I/O-error Err-surfacing untested (code correct by inspection); width/height/fps override only tested one layer down (transport-core/web)

## Doc-code consistency (G006, 15-G5G6QA)
PASS — config-model.md mode table (Fast H264/1280x720/8000, Medium HEVC/1920x1080/20000, Quality AV1/3840x2160/50000, audio_exclusive flip on Quality) matches transport-core/src/mode.rs::mode_defaults field-for-field; precedence chain + corrupt-handling + codec-collapse caveat all match settings.rs. §5 softened to "partial" after the P1 fix.

## Verdict
Architect CLEAR/CLEAR/CLEAR + APPROVE (16-P1Confirm). QA PASSED, no blockers. Residuals P2/P3 non-blocking (nested client.* unknown-field preservation, env OR-fold can't force-off, backup unix-second collision).
