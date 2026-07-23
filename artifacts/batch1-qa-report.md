# Batch-1 QA / red-team report (ultragoal run 019f68d6, commit f980144 + 6825d4a)

Surface: algorithm/package (Rust crate unit tests). Device-adjacent paths (audio decoder reopen, present 10-bit) have HW device tests passing on this machine.

## Targeted suite pass counts (leader + QA agent 11-Batch1QA)
- transport-core: 129 lib + 8 rig passed (mode:: 6/6)
- client-transport --lib: 55 passed (flow::negotiated_channels incl.)
- streamer: 206 passed (codec_select/idr_decision/qu_relay incl.)
- app-native --features video: 57 (+ quality_probe 6) passed (present 10-bit + surround-integrate + fixed test-isolation regression)
- clippy: clean; cargo fmt: applied

## Adversarial coverage matrix (pure fns)
- mode::knobs_for / canonical_vectors — mode defaults + per-field override; free_lunch consts proven immutable; no arithmetic → no overflow. Plain field assignment, panic-free.
- flow::negotiated_channels(u32)->u16 — 0->2, 1/2/6/8 passthrough (both boundaries), 9->2, 100->2, u32::MAX->2 by inspection (range-gated cast, provably panic-free across full u32 domain).
- web_socket::should_request_idr — rtt spike->true, healthy->false, zero interval; saturating arithmetic; usize->u32 hardened to try_from().unwrap_or(u32::MAX) (was truncating cast).
- webrtc::negotiate_codecs — host-priority intersection, case-insensitive, empty intersection->empty; pure filter+collect, no risk.
- qu_relay::resolve_relay_port — None/Some(0)->ephemeral(0), Some(n) verbatim; single unwrap_or, uniform.
- present::preferred_present_format — default-off->R8, want+supported->R10, want+unsupported->R8; pure selector.

## Verdict
Architect (10-Batch1ArchReview): architecture/product/code all CLEAR, recommendation APPROVE, blockers none.
QA (11-Batch1QA): PASSED, blockers none. Advisory-only items (usize->u32 truncation) resolved in 6825d4a; remaining gaps are provably-safe untested boundary values, not functional defects.
