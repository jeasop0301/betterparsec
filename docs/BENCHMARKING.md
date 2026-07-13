# betterparsec benchmark protocol

This document defines what the built-in statistics can prove and how to run a
repeatable comparison. A metric name must describe the layer that produced it;
`payload bytes` must not be presented as `wire bytes`, and media timestamps must
not be presented as input-to-photon latency.

## Measurement layers

| Layer | Measurements available now | Important limitation |
| --- | --- | --- |
| Sunshine / Moonlight host | host processing latency reported by Moonlight | Does not isolate capture from encode |
| Streamer | processing time, sender queue/in-flight depth, accepted/rejected/replaced frames, encoded/RTP payload bytes, actual track-write success/failure/skips, queue wait, write latency, IDR counters | Successful RTP payload writes still exclude RTP/SRTP/UDP/IP overhead and retransmissions |
| Runtime bitrate control | target/request kbps, attempt count, `unsupported`/`send_failed`/`sent_unacknowledged` state | No encoder ACK; queued or failed send does not prove host application |
| Browser WebRTC receiver | payload and RTP-header receive bitrate, frame/counter deltas, jitter-buffer/decode/processing time, loss and recovery counters when exposed by the browser | Browser implementations may omit optional fields |
| Display and input | not measured by the current protocol | Requires a shared frame/input marker or external high-speed-camera rig |

The selected ICE candidate pair now records whether the active path is direct or
relayed, candidate protocols, current RTT and browser-exposed available bitrate.
Candidate-pair byte counters may provide a whole-peer transport total when the
browser exposes them. That value includes audio, RTCP and data-channel traffic,
and still does not include every link-layer byte. Use an OS/interface counter or
controlled packet capture for a public "network usage" claim.

Streamer delivery counters have strict meanings:

- `rtpPacketsDequeued` / `rtpPayloadBytesDequeued` mean work removed from the
  internal queue. They do not prove delivery.
- `rtpPacketsWriteSucceeded` / `rtpPayloadBytesWriteSucceeded` mean the WebRTC
  track write future returned success.
- `rtpPacketsWriteFailed` and `rtpPacketsWriteSkipped` expose failures and
  paused-track skips instead of hiding them in dequeue throughput.
- Queue wait measures enqueue-to-dequeue residence. RTP write latency measures
  time awaiting the track write. In-flight current/max exposes async backlog.
- Even successful writes are not evidence that a remote display presented the
  frame. Correlate sender counters with browser receive/decode/presentation and
  external markers.

## Benchmark export schema v2

Each export records run start/end/export time, retained sample window, browser
and sanitized page identity, requested stream settings, selected codec and
pipelines, HDR state, sample sequence range/count/limit, and per-source freshness.
The sanitized page URL excludes credentials, query parameters and fragments, so
host/application identifiers and tokens are not exported through the URL.

Every sample has a monotonic sequence number. A sequence gap at the beginning is
expected after the 3,600-sample retention window rolls over; gaps inside a saved
run require investigation. Version, build, Git revision, host software and
driver fields remain `unknown` unless they are obtained from an authoritative
runtime source. They must never be inferred from the UI bundle or user agent.

## Reproducible run

1. Pin host GPU, driver, Sunshine version, browser version, resolution, refresh
   rate, codec/profile, target bitrate and network path.
2. Use the same deterministic content trace for every candidate. Keep separate
   traces for static desktop text, scrolling, window movement and full-screen
   game motion.
3. Warm up for 30 seconds, then collect at least 120 seconds. Run each cell five
   times in randomized order.
   Enable **Stats** at the start of the warm-up; this resets the in-browser
   recorder. Use **Export Benchmark** after the run to save the raw one-second
   JSON samples. The recorder retains the latest 3,600 samples.
4. Treat the first sample after startup or a counter reset as a baseline, not a
   measurement.
5. Preserve raw interval samples. Report the median run plus p50/p95/p99 rather
   than averaging all samples across runs.
6. Record direct UDP, TURN/UDP and TURN/TCP/TLS as different experiment cells.
7. Reject a run if its selected ICE path is absent or ambiguous. Record any path
   transition as a separate phase rather than averaging direct and relay samples.

## Repository benchmark harness

`tools/benchmark/Run-Benchmark.ps1` implements the run envelope above. It writes
one immutable result directory under ignored `benchmark-results/<run-id>/` with:

- profile and network-trace copies;
- Git SHA/dirty state and authoritative local OS/GPU/driver/toolchain inventory;
- Windows adapter counters before/after/delta;
- `pktmon` ETL plus converted PCAPNG when run elevated;
- exact SSH `tc netem` phase commands and timestamps;
- optional application stdout/stderr and binary SHA-256;
- optional Foundation Sunshine start, host and cleanup logs with stock restoration
  verification;
- browser schema-v2 export when supplied;
- a top-level manifest whose state is `completed` or `failed`.

The supplied `20-8-15mbps.json` trace has three 40-second phases and matches the
120-second measurement window in `1080p60-h264.json`. Apply it to both router
egress interfaces for symmetric shaping. Applying it to one interface is a
one-direction experiment and must be labeled as such.

The runner stops shaping and packet capture before waiting for browser JSON
export, so export UI/download traffic is excluded from the measured interval.
It intentionally does not automate candidate UI clicks or deterministic content
startup; the operator confirmation prevents measuring the wrong app, codec, ICE
path or trace.

`tools/benchmark/New-AbbaPlan.ps1` generates recorded-seed ABBA/BAAB blocks for
each network cell. A plan defines order only; every run still needs its own
manifest and validity decision. See `tools/benchmark/README.md` for commands and
artifact layout.

Historical same-PC artifacts under `target/live-test/` remain evidence for the
2026-07-13 smoke runs, but executable helpers now live under `tools/benchmark/`
so `cargo clean` cannot remove the benchmark protocol.

## Superseded same-PC smoke (2026-07-13, pre-fix)

This smoke run used an RTX 4070 host, stock Sunshine `2026.516.143833`
(`14ffa6fd`), Chromium WebRTC, H.264 NVENC, 1920x1080 and 60 fps. The client
and host ran on the same Windows PC over a loopback ICE candidate. The stream
remained connected for 605 seconds and Sunshine completed encoder teardown on
exit.

| Sample | FPS | Video payload | Host processing avg | Streamer processing avg | Decode avg | Loss / dropped / NACK / freeze | Sender queue max |
| --- | ---: | ---: | ---: | ---: | ---: | --- | ---: |
| Large screen change | 61 | 4.55 Mbps | 1.87 ms | 1.30 ms | 0.38 ms | 0 / 0 / 0 / 0 | 1/3 frames |
| Static desktop 1 | 60 | 0.55 Mbps | 1.66 ms | 0.22 ms | 0.37 ms | 0 / 0 / 0 / 0 | 1/3 frames |
| Static desktop 2 | 59 | 0.55 Mbps | 1.93 ms | 0.25 ms | 0.44 ms | 0 / 0 / 0 / 0 | 1/3 frames |
| Static desktop 3 | 60 | 0.57 Mbps | 1.80 ms | 0.23 ms | 0.39 ms | 0 / 0 / 0 / 0 | 1/3 frames |

The browser reported roughly 168 ms instantaneous RTP jitter throughout, while
the interval jitter-buffer delay was 0.19-1.78 ms and its target delay was
17.37-40.08 ms. Investigation found that the bridge treated Moonlight's
microsecond `presentationTimeUs` as though it were a 90 kHz timestamp. At 60 fps
this produced an RTP step near 16,667 instead of about 1,500. This unit defect,
not demonstrated network jitter, explains the contradictory receiver values.

The same conversion path also truncated Moonlight's 0.1 ms
`frameHostProcessingLatency` by using integer milliseconds instead of preserving
100 us units. Consequently, all host-processing latency values in the table
above are invalid as a performance baseline. The payload, connection-duration,
loss/drop and teardown observations remain useful only as a functional smoke.
Do not use any row above in a Parsec comparison or before/after latency claim.

Two launch defects were exposed before the run: the web server had no rustls
crypto provider and panicked when it constructed the Moonlight HTTPS client,
and the development config's `./streamer` path resolved to the source directory
on Windows. The crypto-provider defect now has a regression test. The web server
now falls back to a sibling `streamer.exe` for the default Windows path; the
corrected smoke launched that sibling without an explicit path override.

This is only a superseded functional smoke. It does not include a second
machine, a deterministic motion trace, repeated randomized runs, full wire-byte
capture, quality scoring, WAN impairment, or input-to-photon instrumentation.
It therefore cannot support a Parsec superiority claim.

## Corrected same-PC H.264 smoke (2026-07-13)

After fixing both unit conversions and adding regression tests, the same-PC
H.264 path was run again. Browser RTP jitter fell from the erroneous ~168 ms to
2-4 ms, jitter-buffer target was approximately 11 ms, and interval
jitter-buffer delay was 0.03-0.04 ms. Reported loss, dropped frames and freezes
were all zero.

These results confirm the direction of the timestamp fix and a healthy loopback
smoke. They do **not** establish input-to-photon latency, WAN behavior, quality
at a fixed wire bitrate, or superiority over Parsec. Packet capture must still
verify the 90 kHz RTP cadence, and host-processing latency must be checked
against external ground truth before either metric becomes a release gate.

The installed host remains stock LizardByte Sunshine `2026.516.143833`
(`14ffa6fd`). The Foundation Sunshine build carrying the dynamic-bitrate
capability patch completed and was staged successfully: binary SHA
`b29f747b510f416db0d8075ed23b280e50ce3cbc531b766c828dedb9c2ee7dec`, version
`2026.0713.165229.杂鱼`. It started alongside the stock
service on alternate base port `49000`, without replacing the installed
service.

RTX 4070 encoder probes succeeded for H.264, HEVC and AV1. HEVC 10-bit YUV444
succeeded. AV1 YUV444 was reported unsupported, after which AV1 10-bit 4:2:0
succeeded; AV1 YUV444 must therefore remain unadvertised on this host.

## Foundation paired H.264 run (2026-07-13)

An elevated helper copied the stock pairing state, certificate, key, app list,
and configuration into a disposable ACL-restricted directory. All Foundation
state paths were passed as absolute overrides. This distinction is required:
passing only an alternate `sunshine.conf` still resolves relative certificate,
state, and app paths against the directory beside `sunshine.exe` and silently
creates a new host identity.

The stock service was stopped without changing its files or service definition.
Foundation then owned the normal Sunshine ports and the existing BetterParsec
host connected without re-pairing. The observed session was H.264 NVENC,
1920x1080 at 60 fps, with a 10,000 Kbps initial ceiling.

The exported 75-second browser sample contained 75 one-second samples:

| Metric | p50 | p95 | Maximum / total |
| --- | ---: | ---: | ---: |
| Host processing latency (sample averages) | 2.203 ms | 2.272 ms | 2.302 ms |
| Streamer processing latency (sample averages) | 1.263 ms | 1.552 ms | 1.651 ms |
| Browser decode time | 0.460 ms | 0.509 ms | 0.574 ms |
| Browser processing delay | 2.042 ms | 2.661 ms | 2.849 ms |
| WebRTC RTP jitter | 1.000 ms | 1.000 ms | 4.000 ms |
| WebRTC payload receive rate | 3,629 Kbps | 4,728 Kbps | 5,091 Kbps |
| Sender encoded-input rate | 3,631 Kbps | 4,755 Kbps | 5,212 Kbps |
| Sender queue maximum | — | — | 1 frame |
| Loss / browser drop / sender drop / freeze / NACK | — | — | 0 / 0 / 0 / 0 / 0 |

The integrated BetterParsec control path produced host requests of 3431, 3787,
4229, and 4814 Kbps. Every request was followed by the Foundation capture-thread
apply event and an NVENC AVC success line. With 20% FEC, the corresponding
encoder targets were 2744, 3029, 3383, and 3851 Kbps. The benchmark recorder
also captured subsequent integrated controller targets of 5418 and 6050 Kbps
as `SentUnacknowledged`; the host log independently confirmed actual NVENC
application.

The Foundation-only runtime API was separately used to request exactly 8000 and
15000 Kbps. NVENC applied 6400 and 12000 Kbps respectively after FEC. The
BetterParsec controller then replaced each value in about 0.6 seconds. This
proves the host reconfiguration mechanism, but it is not an integrated
20→8→15 Mbps network-shaping result and must not be presented as one.

Artifacts are stored under `target/live-test/` as
`foundation-h264-2026-07-13.json`, `foundation-h264-host-2026-07-13.log`, and
`stock-restoration-verification.json`. After the run, Foundation, streamer, and
the watchdog were stopped. `SunshineService` returned to `Running`, stock owned
port 47989, and SHA-256 values for apps, config, state, certificate, and private
key all matched their pre-run values.

This closes the Foundation capability-handshake and encoder-application proof.
It still does not measure total wire bytes, input-to-photon latency, objective
image quality, deterministic congestion recovery, or performance against
Parsec.

## Browser-native telemetry smoke (2026-07-13)

The rebuilt web server and streamer were exercised through the in-app Chromium
browser against the restored stock Sunshine service. The stream negotiated H.264
at 1920x1080/60 with a requested 10,000 Kbps ceiling. This run validates the new
telemetry plumbing; it is not a network or Parsec comparison baseline.

The selected ICE pair was an unambiguous direct host/UDP-to-host/UDP path with a
browser-reported candidate-pair RTT of 0.00 ms on the same-PC path. In the final
one-second interval, the sender reported 87 successful WebRTC track writes, zero
failures, average enqueue-to-dequeue wait of 0.084 ms, and average successful/error
track-write await of 0.261 ms. These values prove that the counters are live and
internally separated. They do not prove UDP wire delivery or remote presentation.

The exported schema-v2 artifact contains 46 consecutive samples (sequence 0-45),
selected codec and audio/video pipelines, requested settings, selected ICE path,
and per-source freshness. Its page URL is
`http://127.0.0.1:8080/stream.html`; host/application query identifiers were
removed and `queryParametersIncluded` is false. The artifact is stored at
`target/live-test/browser-wedge-schema-v2-2026-07-13.json`.

## Parsec superiority protocol (hard-gate model)

An unconditional "better in every environment" or "100% better" claim is not
testable. The defensible target is a 100% pass rate across every declared hard
gate and network cell below. A failed gate cannot be compensated by a weighted
score elsewhere.

| Dimension | Required result against Parsec on the identical trace |
| --- | --- |
| Quality / bitrate | VMAF-NEG +2 at equal actual wire bitrate, or equal quality with total bidirectional wire bytes reduced by at least 20% |
| "Much more efficient" wording | Equal-quality total bidirectional wire bytes reduced by at least 30% |
| Input-to-photon latency | LAN median at least 2 ms faster and p95 at least 5 ms faster; p99 non-inferior, measured by 1,000 fps camera or GPIO+photodiode |
| Network use | Total bidirectional wire bytes at least 20% lower; static trace at least 40% lower |
| Loss recovery | freeze/min at least 30% lower, recovery p95 at least 25% faster, and zero disconnects in each 15-minute run |
| Session reliability | 100 connect/disconnect cycles, zero crashes, at least 99% success, and faster start p95 |
| Metric validity | Product telemetry within ±2 ms or ±10% of external ground truth |

Run all gates on clean LAN; typical WAN (30 ms/3 ms jitter, 0.1% loss,
20 Mbps); bad Wi-Fi (30 ms/12 ms, 1% burst loss, 10 Mbps); congested WAN
(50 ms/8 ms, 2% loss, 5 Mbps); and recovery transitions of 5%→0% loss and
5→20 Mbps. Use five randomized ABBA runs per cell and require the bootstrap
confidence interval—not only the point estimate—to clear the gate.

## Initial engineering gates

- Sender queue maximum: at most two frames during the steady-state run.
- Healthy-link frame rejection: below 0.5%.
- Browser decode p95: below 5 ms on the declared supported client class.
- Delivered payload bitrate p95: no more than 15% above the configured target.
- Healthy-link freezes: zero; loss-recovery tests report freeze duration and IDR
  count instead of hiding them in an average.
- A codec/preset change is accepted only when latency and freeze gates remain
  non-inferior and the same-content quality measurement improves.

These are engineering gates, not a Parsec superiority claim. A public claim also
requires the same host/client/network trace, total-session or external wire-byte
measurement, objective desktop and motion quality scores, and an external or
marker-based input-to-photon test.

## Metrics that must not be inferred

- RTP timestamps are a media timeline, not a wall clock. They cannot establish
  absolute frame age across machines.
- `hardwareAcceleration: "prefer-hardware"` is a preference, not proof of a
  hardware decoder. Confirm with decoder implementation, power/CPU/GPU evidence
  and decode-time behavior on every supported client class.
- REMB is a receiver estimate, not the encoder's applied bitrate and not actual
  throughput.
- Runtime bitrate `sent_unacknowledged` is a queued control request, not proof
  of encoder reconfiguration. Correlate it with host logs and measured payload.
- Encoded payload bytes do not include retransmissions or transport overhead.
