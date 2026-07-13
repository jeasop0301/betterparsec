# Runtime bitrate apply path (`0x5506`)

## Verified compatibility boundary

The bridge computes a bounded ABR target from WebRTC REMB and receiver-report
loss. Runtime host application is a separate, non-standard capability:

- Stock LizardByte Sunshine at `1d9ab7b8` has no `0x5506` handler and no
  runtime encoder bitrate path. It must be treated as unsupported.
- AlkaidLab Foundation Sunshine at `e110872d` handles control packet `0x5506`.
  For bitrate, its payload is two little-endian 32-bit integers:
  `[parameter_type = 2, bitrate_kbps]`.
- The Foundation handler validates `1..=800000` kbps and dispatches an
  asynchronous encoder event. It sends no acknowledgement. NVENC failure is
  logged host-side but cannot be observed by the client.
- The qiin2333 Moonlight client fork at `d22aa771` includes `0x5506` in its
  packet table, but does not expose a client-to-host `LiChangeBitrate` API.

Therefore the only truthful client states are `unsupported`, `send_failed`,
and `sent_unacknowledged`. `sent_unacknowledged` is not evidence that the
encoder changed. A late ENet service failure can also happen after queueing, so
`send_failed` means host application is unknown rather than definitely unchanged.

## Implemented bridge path

The active local `moonlight-common-rust` patch now contains:

- `LiChangeBitrate(int)` in moonlight-common-c;
- a safe `MoonlightStream::change_bitrate(u32)` wrapper;
- reliable ENet delivery on the generic encrypted control channel;
- fixed 8-byte little-endian payload and `1..=800000` client-side validation;
- a required provisional `LI_FF_DYNAMIC_BITRATE` capability bit (`0x40`).

The streamer now:

1. shares the WebRTC ABR target through `Arc<AtomicU32>`;
2. seeds its gate from the session's initial bitrate, avoiding an initial
   no-op control message;
3. polls every 500 ms;
4. applies a 900 ms minimum send interval and 10% hysteresis;
5. commits the gate baseline only after the C library queued the message;
6. cancels the apply task when the stream stops or restarts.

The capability bit is intentionally required. Its precise meaning is "the host
accepts a dynamic bitrate request", not "the selected encoder backend applied
it". Encrypted ENet or a Sunshine version number does not prove even request
support, and stock Sunshine silently ignoring an unknown packet would otherwise
create a false-success state.

## Host opt-in

Current Foundation Sunshine implements the packet but does not advertise a
dedicated capability. Apply
`docs/host-patches/foundation-sunshine-dynamic-bitrate-capability.patch` to the
pinned Foundation source before building the host. Do not advertise the bit on
stock Sunshine until its encoder path and packet handler are implemented.

This capability value is provisional and private to the paired BetterParsec
client/host builds. Upstreaming must allocate or agree on a durable feature bit.

## Live validation status

The 2026-07-13 paired Foundation H.264 run collected matching client
`sent_unacknowledged` requests and host capture-thread/NVENC apply success for
3431→3787→4229→4814 Kbps. This proves the capability boundary and host
reconfiguration mechanism for that pinned build, GPU and codec. It does not turn
the client state into an acknowledgement.

The repository-owned benchmark runner now collects the client/application logs,
Foundation host logs, cleanup verification, OS counters and PCAPNG into one run
manifest. The remaining live validation is:

- run the 20→8→15 Mbps trace on a separate client/router topology;
- show measured wire bitrate convergence without sender queue growth;
- report frame-size p95/p99 and IDR count per phase;
- keep encode/decode and external input-to-photon latency inside the gates;
- add a request-id ACK if the product must expose `applied` without host-log
  correlation.

Foundation's current NVENC implementation forces reset + IDR only for an HEVC
bitrate increase. H.264/AV1 and HEVC decreases do not take that branch. This
must be measured per codec and driver; the bridge must not assume every change
forces an IDR.

## Source revisions

- `LizardByte/Sunshine@1d9ab7b8d8bb623a9f4608b4ddc5c252e3c9e1c9`
- `AlkaidLab/foundation-sunshine@e110872d8f0fc9fbe54ecd787a7a384b44f778e8`
- `qiin2333/moonlight-common-c@d22aa7715d77594ba512e48c698460d321f87356`
