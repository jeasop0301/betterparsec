# BetterParsec performance, efficiency, and security audit

Date: 2026-07-13  
Audited source checkout: `C:\Users\kje12\Desktop\Projects\betterparsec`  
Base commit: `1bcf68e4c864a3df471b56ef4368c79cb5289d87`  
Source state: dirty working tree; source checkout was treated as read-only during this audit.

## Executive verdict

The current direction is technically sound and worth continuing, but the project has not yet
proved that it beats Parsec Native or Parsec Web. The implementation has crossed the prototype
threshold: it has a functioning browser stream, honest sender telemetry, selected ICE-path
telemetry, a bounded REMB/loss controller, a runtime Sunshine bitrate-control path, a reproducible
patched dependency bootstrap, Foundation host apply evidence, and a repeatable benchmark runner.

The next objective must not be “add AV1” or “raise quality.” It must be to establish a trusted
measurement and security foundation, then optimize the largest observed latency/quality cost one
stage at a time. Higher quality, lower latency, and fewer bytes cannot all improve for every scene
and network condition. Product claims must be framed as Pareto improvements in explicitly defined
content and network cells.

## Audit scope

The audit covered:

- WebRTC sender queue and write path
- browser receive/decode/jitter-buffer telemetry
- REMB and Receiver Report ABR logic
- runtime Sunshine bitrate request/apply boundary
- codec capability and browser WebCodecs paths
- authentication/password and forwarded-header trust boundaries
- paired-host TLS behavior
- benchmark reproducibility and artifact identity
- Rust/npm dependency and CI posture
- current roadmap ordering

## What is already strong

### Measurement semantics

The code distinguishes encoded input, queue acceptance, dequeue, RTP track-write success,
track-write failure, paused skips, queue wait, write await, and in-flight depth. It does not label
RTP payload bytes as full wire bytes. Browser export schema v2 records monotonic sequence,
source freshness, codec/pipeline/HDR, sanitized URL, selected candidate pair, direct/relay route,
candidate protocol, RTT, and browser counters.

### Adaptive bitrate safety

The ABR controller is pure and deterministic, starts at the configured ceiling, reacts quickly to
congestion/loss, recovers slowly, clamps to floor/ceiling, and uses an explicit apply gate. Runtime
state remains honest: `sent_unacknowledged` is not presented as encoder application. Foundation
host logs independently demonstrated H.264 NVENC reconfiguration for the tested build and GPU.

### Reproducibility

The external mutable `../vendor-mlc-rust` dependency was replaced by pinned upstream revisions,
repository-owned patches, bootstrap scripts, and CI bootstrap. The Windows release path can build
vendored OpenSSL with Strawberry Perl and NASM when no MSVC OpenSSL SDK is installed.

### Basic application security

Passwords use PBKDF2-HMAC-SHA256 with a high iteration count, random salts, and constant-time
comparison. Forwarded-header identity is accepted only from loopback peers. npm production audit
reported no production vulnerabilities during this audit.

## P0 findings

### P0-1: paired-host TLS verification is currently unsafe for arbitrary host addresses

The current patched Moonlight Hyper/Rustls client accepts the server certificate and the TLS
`CertificateVerify` signature without cryptographic verification. The patch comment assumes the
bridge-to-Sunshine connection is always loopback, but BetterParsec stores and passes arbitrary host
addresses. Therefore the assumption is not enforced by product architecture.

Required fix:

- pin the exact DER bytes of the paired Sunshine certificate;
- parse the historical zero-serial Sunshine certificate with OpenSSL rather than webpki;
- verify TLS 1.2/1.3 `CertificateVerify` with the public key from that exact pinned certificate;
- reject certificate replacement and signature tampering;
- retain hostname/SAN bypass only because pairing pins identity independently.

Audit worktree B contains a tested prototype and an incremental integration patch. The prototype
passes exact-DER, RSA-PKCS1, RSA-PSS, wrong-message, tampered-signature, and unsupported-scheme
tests. The patch compiles when applied after the existing BetterParsec Moonlight patch in a clean
pinned clone. It still requires a real stock/ Foundation paired-host smoke before landing.

### P0-2: invalid benchmark artifacts could previously enter the comparison set

A browser export from another time and a short 45-second smoke could be attached to a nominal
120-second run manifest and look plausible. A new truth-gate analyzer now rejects:

- non-completed manifests;
- missing or non-v2 browser exports;
- sample gaps and non-monotonic time;
- codec/resolution/FPS/HDR mismatch;
- manifest/browser time-envelope mismatch;
- undersized measurement windows;
- ICE route/protocol mismatch and candidate-pair transitions;
- stale telemetry;
- sender queue/rejection/write failures;
- browser decode/processing/FPS/jitter-buffer gate failures;
- freeze/loss violations;
- required shaping/packet-capture omissions.

The old dry-run manifest combined with the historical browser smoke is now correctly rejected.

### P0-3: two-machine ground truth is still missing

Loopback and same-PC data prove functionality and instrumentation, not superiority. The first
trusted dataset must use a separate host, client, and impairment router with an exact content hash,
packet capture, browser export, bridge logs, host apply logs, and randomized order.

## P1 findings

### P1-1: browser playout/jitter-buffer delay is the largest observed latency candidate

The historical direct-UDP loopback smoke showed approximately:

- sender queue-wait p95: 0.042 ms
- RTP write-await p95: 0.268 ms
- browser decode p95: 0.427 ms
- browser jitter-buffer target p50/p95: roughly 41/44 ms

This does not prove 44 ms of actual additional glass-to-glass latency, but it is much larger than
other measured internal stages and must be isolated before encoder micro-optimization. The current
stats code correctly converts cumulative target delay to an interval average. Next experiments
should compare the native WebRTC track renderer with the data-channel/WebCodecs pipeline and test
browser receiver playout-delay controls where available. Any change must be measured for freezes,
late drops, power use, and browser compatibility.

### P1-2: current ABR is a safety controller, not a complete low-latency congestion controller

REMB and Receiver Report loss are useful, but the controller does not yet incorporate:

- TWCC/delay-gradient feedback;
- sender queue pressure and write latency;
- receiver decoder/backlog pressure;
- feedback freshness and no-feedback fallback;
- explicit host apply acknowledgement;
- per-content complexity or encoder saturation.

Do not replace the current bounded controller wholesale. Add signals behind logged, replayable
controller inputs and validate each change against deterministic traces.

### P1-3: stream replacement lifecycle can overlap

The current stream-start path can dispatch old-stream shutdown without awaiting full termination
before a new stream starts. This risks overlapping host streams, encoder ownership, or stale
runtime-bitrate tasks. The lifecycle should become a serialized state machine with bounded stop,
forced cleanup, and generation IDs.

### P1-4: browser decoder recovery threshold is too large for interactive use

The WebCodecs path resets only after an estimated decode queue delay above roughly 200 ms. A gaming
stream should prevent backlog much earlier through ABR and frame-age policies. Decoder reset remains
a last-resort recovery mechanism, not congestion control.

## P2 findings

### Quality and efficiency optimization order

1. Win on H.264 first because it has the broadest reliable hardware decode coverage.
2. Build an encoder configuration matrix for low-latency CBR/ultra-low-latency CBR, B-frame and
   lookahead disablement, VBV size, keyframe policy, and dynamic reconfigure behavior.
3. Add deterministic scene classes: static text, scrolling, window motion, high-motion game,
   particle/noise, and scene cuts.
4. Compare equal-wire-byte quality and equal-quality wire bytes, not configured bitrate.
5. Add HEVC 4:4:4 only where host encode, bridge packetization, browser decode, and display path are
   all proven.
6. Add AV1 4:2:0 capability-gated; do not advertise unsupported AV1 4:4:4 on the tested RTX 4070.
7. Add ROI/QP maps or content-aware allocation only after the baseline controller and quality
   scorer are trustworthy.

Potential high-value efficiency features after the truth gates:

- static-screen bitrate collapse with immediate motion recovery;
- text/UI region protection;
- cursor and active-window ROI without eye tracking;
- scene-cut-aware keyframe and burst budget;
- content-complexity feed-forward bounded by queue/loss feedback;
- FEC/retransmission policy by loss pattern and path type.

## P3 findings

- TURN/TLS/TCP 443 must be measured as a separate constrained-network product cell.
- reconnect, resume, credential rotation, relay cost, and session recovery need soak testing.
- add Rust advisory scanning and review pinned git dependencies in CI.
- panic exit code must be non-zero and crash reason must survive into the run artifact.
- add connect/disconnect stress and resource-leak tests.

## Locked execution order

### Gate A: land security and truth foundations

1. Integrate exact certificate pin + TLS handshake signature verification.
2. Run stock Sunshine and Foundation paired-host regression tests.
3. Integrate benchmark analyzer with the runner and save `analysis.json` automatically.
4. Make clean comparison profiles require content hash, shaping, packet capture, and full duration.

### Gate B: first trusted baseline

1. Separate host/client/router.
2. H.264 1080p60 direct UDP.
3. Deterministic content trace.
4. clean LAN plus 20→8→15 Mbps trace.
5. BetterParsec and Parsec Native randomized ABBA, five repetitions per cell.
6. Reject invalid runs before aggregation.

### Gate C: latency isolation

1. Correlate sender, PCAP, browser receive, decode, and presentation markers.
2. Compare WebRTC track renderer versus WebCodecs/data path.
3. Investigate jitter-buffer target/minimum control and browser implementation differences.
4. Add external input-to-photon measurement.

### Gate D: controller and encoder optimization

1. Add queue/write/freshness feedback to ABR.
2. Add TWCC delay signal only after replayable trace tests.
3. Test H.264 NVENC low-latency matrix.
4. Choose settings by Pareto frontier, not one weighted score.

### Gate E: codec/content efficiency

1. HEVC 4:4:4 truth table.
2. AV1 4:2:0 truth table.
3. static/text/ROI content-aware allocation.
4. quality metrics and human text/UI review.

## Worktree orchestration and saved state

No source-checkout edits, stash, reset, commit, merge, or push were performed by this audit.

### Worktree A — benchmark truth gate

- Path: `C:\Users\kje12\.devspace\worktrees\betterparsec-3325fd20`
- Purpose: benchmark analyzer only
- Files:
  - `tools/benchmark/analyze-benchmark.mjs`
  - `tools/benchmark/analyze-benchmark.test.mjs`
  - `tools/benchmark/ANALYZER.md`
  - two strict H.264 comparison profiles
  - `patches/benchmark-runner-truth-gate.patch`
  - this audit document
- Additional strict profiles:
  - `tools/benchmark/profiles/1080p60-h264-clean-lan-comparison.json`
  - `tools/benchmark/profiles/1080p60-h264-20-8-15-comparison.json`
- Validation: 10/10 Node tests, profile JSON parse, `git diff --check`, historical mismatched smoke rejected, runner integration patch apply-check/PowerShell parse/dry-run passed

### Worktree B — paired TLS hardening

- Path: `C:\Users\kje12\.devspace\worktrees\betterparsec-daa34397`
- Purpose: security prototype and integration patch only
- Files:
  - `security/tls-pinning-prototype/`
  - `patches/moonlight-common-rust-tls-pinning.patch`
- Validation:
  - prototype 4/4 tests;
  - incremental patch apply-check on current patched dependency;
  - clean pinned clone + existing BetterParsec patch + TLS patch `cargo check` passed.

## Next exact action

Before any codec or ABR expansion, review and integrate Worktree B's TLS patch into the dependency
bootstrap patch set, then integrate Worktree A's analyzer into `Run-Benchmark.ps1`. After both are
retested in the dirty source checkout without overwriting user changes, perform a real paired-host
TLS smoke and prepare the first two-machine benchmark cell.
