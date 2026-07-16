/* client_transport.h — C ABI of the BetterParsec client-transport cdylib.
 *
 * Hand-maintained mirror of client-transport/src/capi.rs; keep in sync.
 * Consumer: moonlight-qt fork app/streaming/transport/our_transport.cpp
 * (m6-native-spike.md Option-3).
 *
 * Threading contract:
 *   ct_receiver_on_message / ct_receiver_tick / ct_receiver_poll_*  — transport thread
 *   ct_receiver_wait_frame / ct_frame_*                             — decoder thread
 *   ct_receiver_close unblocks the decoder thread; ct_receiver_free only
 *   after both threads stopped using the pointer.
 */
#ifndef BETTERPARSEC_CLIENT_TRANSPORT_H
#define BETTERPARSEC_CLIENT_TRANSPORT_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct CtReceiver CtReceiver;
typedef struct CtFrame CtFrame;

/* POD frame view; `data` stays valid until ct_frame_free. */
typedef struct CtDecodeUnit {
    uint32_t frame_id;
    uint8_t  is_key;        /* 1 = key/IDR, 0 = delta */
    uint32_t timestamp_us;  /* sender capture timestamp (us, wraps at u32) */
    int64_t  duration_us;   /* timestamp delta to the previous frame */
    const uint8_t *data;    /* concatenated Annex-B frame bytes */
    size_t   data_len;
} CtDecodeUnit;

/* Lifecycle. now_ms: caller monotonic clock in milliseconds. */
CtReceiver *ct_receiver_new(uint64_t now_ms);
void        ct_receiver_close(CtReceiver *p); /* wake decoder thread */
void        ct_receiver_free(CtReceiver *p);

/* Receive path (transport thread). These standalone feed/tick/poll entrypoints
 * return/do nothing while a ct_start session holds the receiver lease. */
void ct_receiver_on_message(CtReceiver *p, const uint8_t *buf, size_t len,
                            uint64_t now_ms);
void ct_receiver_tick(CtReceiver *p, uint64_t now_ms); /* call every ~50 ms */

/* Returns 1 and writes *out when an ACK is pending. Standalone receivers use
 * the legacy 4-byte LE u32 wire; ct_start serializes v1 or selected-v2
 * controls internally. Cumulative: newest value supersedes. */
int32_t ct_receiver_poll_ack(CtReceiver *p, uint32_t *out);

/* Returns 1 at most once per latch. Standalone receivers use the legacy
 * 1-byte 0x00 NeedsIdr wire; ct_start serializes selected-v2 controls
 * internally. */
int32_t ct_receiver_poll_needs_idr(CtReceiver *p);
/* Returns 1 and writes the FEC epoch and typed reason, else 0. Reasons:
 * 1=epoch transition, 2=reorder gap, 3=reassembly eviction,
 * 4=metadata mismatch, 5=frame CRC, 6=memory cap, 7=FEC eviction,
 * 8=decoder-side frame-queue overflow (see ct_event_discontinuity — this
 * standalone poll only ever reports transport-sourced reasons 1-7; 8 is
 * listed here because the code space is shared). */
int32_t ct_receiver_poll_discontinuity(CtReceiver *p, uint32_t *out_epoch,
                                       uint8_t *out_reason);

/* Decoder pull loop (decoder thread) — LiWaitForNextVideoFrame replacement.
 * NULL on timeout or after ct_receiver_close. */
CtFrame *ct_receiver_wait_frame(CtReceiver *p, uint64_t timeout_ms);
int32_t  ct_frame_view(const CtFrame *f, CtDecodeUnit *out);
void     ct_frame_free(CtFrame *f);

/* ── G002 ordered event pull + decoder-recovery (decoder thread) ────────
 * Every discontinuity (transport-sourced or queue-overflow) is queued in
 * wire order strictly before any frame it gates, so draining ct_receiver_*
 * events in order can never observe a frame ahead of the reset that must
 * flush the decoder first. ct_receiver_wait_frame/ct_frame_* above remain a
 * frame-only compatibility view: they silently skip discontinuities.
 *
 * Decoder-recovery (the flush -> decode-a-fresh-key handshake) is keyed by
 * a monotonic `generation`, not by epoch: most discontinuity reasons do
 * not change the epoch, so two discontinuities can share one epoch and a
 * stale ack for the first must never close the second's recovery. Consumer
 * pattern: retain `(generation, epoch)` from the most recently popped
 * ct_event_discontinuity, and pass that exact pair to
 * ct_receiver_acknowledge_decoded_key once a fresh key decodes — never
 * track epoch alone.
 */
typedef struct CtVideoEvent CtVideoEvent;

#define CT_EVENT_FRAME         0
#define CT_EVENT_DISCONTINUITY 1

/* NULL on timeout / after ct_receiver_close. Release with ct_event_free. */
CtVideoEvent *ct_receiver_wait_event(CtReceiver *p, uint64_t timeout_ms);
/* Non-blocking; NULL when nothing is queued. Release with ct_event_free. */
CtVideoEvent *ct_receiver_try_event(CtReceiver *p);
/* CT_EVENT_FRAME / CT_EVENT_DISCONTINUITY, or -1 on NULL input. */
int32_t ct_event_kind(const CtVideoEvent *e);
/* Valid only when ct_event_kind == CT_EVENT_FRAME. 1 on success, 0 else
 * (NULL input or wrong kind). data stays valid until ct_event_free. */
int32_t ct_event_view(const CtVideoEvent *e, CtDecodeUnit *out);
/* Valid only when ct_event_kind == CT_EVENT_DISCONTINUITY. 1 on success, 0
 * else. *out_generation is the recovery identity to retain (see the G002
 * section note above) and later pass to
 * ct_receiver_acknowledge_decoded_key; epoch alone is not a safe ack key.
 * Reason codes match ct_receiver_poll_discontinuity (8 = queue overflow). */
int32_t ct_event_discontinuity(const CtVideoEvent *e, uint64_t *out_generation,
                                uint32_t *out_epoch, uint8_t *out_reason);
void    ct_event_free(CtVideoEvent *e);

/* Query the decoder-recovery handshake currently open, if any. Returns 1
 * and writes *out_generation/*out_epoch when one is open, else 0 (no
 * recovery open, including NULL input) — the return code is what "open"
 * means; a legitimately open recovery can carry generation/epoch 0, so
 * never branch on the written values alone. Every discontinuity/overflow
 * event opens a fresh generation (see the G002 section note above); only a
 * matching ct_receiver_acknowledge_decoded_key closes it — a stale
 * generation (including one sharing the current epoch) or wrong-epoch ack
 * is a no-op. Independent of the receiver's own ordering gate (which
 * withholds delta frames until the next keyframe). */
int32_t ct_receiver_recovery_epoch(const CtReceiver *p, uint64_t *out_generation,
                                   uint32_t *out_epoch);
/* Closes recovery for `(generation, epoch)` — the pair retained from the
 * gating ct_event_discontinuity — once a key frame decoded successfully at
 * that epoch. Returns 1 iff the pair matched the open recovery and closed
 * it, else 0. `frame_id` is diagnostic-only and unvalidated by the
 * implementation: the `(generation, epoch)` pair alone decides. */
int32_t ct_receiver_acknowledge_decoded_key(const CtReceiver *p,
                                            uint64_t generation, uint32_t epoch,
                                            uint32_t frame_id);

/* Audio pull loop (audio thread). Blocks up to timeout_ms for the next
 * opus packet and copies it into buf (at most cap bytes). Returns the
 * full packet length (truncated when > cap; 4096 always suffices for
 * RFC 7587 payloads), 0 on timeout / after ct_receiver_close, -1 on
 * NULL input. The packet is consumed either way. */
intptr_t ct_receiver_wait_audio(CtReceiver *p, uint64_t timeout_ms,
                                uint8_t *buf, size_t cap);

/* ── G004 native decode/present heartbeats ────────────────────────────────
 * Call ct_receiver_note_decoded_output ONLY from the FFmpeg decoder's
 * output callback (decoder thread), and ct_receiver_note_presented ONLY
 * from the D3D11 present path's success case (present thread).
 * Browser/receive-side symbol arrival must NEVER call these — that is the
 * receive-stage heartbeat's job (ct_receiver_tick/ct_receiver_on_message
 * already drive it via frames_delivered). NULL is ignored. */
void ct_receiver_note_decoded_output(const CtReceiver *p);
void ct_receiver_note_presented(const CtReceiver *p);

/* ── Session (connection) ───────────────────────────────────────────────
 * ct_start connects like the browser does: POST /api/login (cookie) →
 * ws /api/host/stream signaling → WebRTC answer → video_fec subscribe.
 * Frames land in the receiver's queue; ACK/needs-IDR replies are sent
 * automatically by the session's internal 50 ms ticker.
 */
typedef struct CtSession CtSession;

typedef struct CtSessionConfig {
    const char *base_url;   /* e.g. "https://192.168.0.10:8080" */
    const char *username;
    const char *password;
    uint32_t host_id;
    uint32_t app_id;
    uint32_t bitrate_kbps;
    uint32_t width;
    uint32_t height;
    uint32_t fps;
    /* 1 = accept any TLS certificate (dev only). When 0, cert_sha256 must
     * point to the 32-byte SHA-256 of the server certificate (DER). */
    uint8_t insecure_tls;
    const uint8_t *cert_sha256;
} CtSessionConfig;

/* NULL on invalid configuration, a closed receiver, or when another caller
 * already acquired the receiver's session lease. A receiver supports exactly
 * one ct_start generation and must outlive that session. */
CtSession *ct_start(const CtSessionConfig *cfg, CtReceiver *rx);
/* 0=connecting 1=peer-connected 2=streaming 3=failed 4=stopped, -1 NULL. */
int32_t ct_session_state(const CtSession *s);
/* Stops (joins) and frees. Closes the receiver queue as a side effect. */
void ct_stop(CtSession *s);

/* ── G004 three-stage watchdog / incident telemetry ──────────────────────
 * Receive-stage rungs (Stall/RequestIdr/RestartIce) are fully handled
 * server-side over signaling; ct_session_stalled/reconnect_requested are
 * the readback. Decode/present stalls are typed actions the native pump
 * must consume: DecodeStall -> decoder flush + IDR; PresentStall -> the
 * native pump's own bounded ladder (device recreate, then R8 fallback);
 * a final Reconnect past that bound surfaces via
 * ct_session_reconnect_requested, not through the action poll. */

/* Poll-and-clear a pending typed decode/present-stall action. Returns 1
 * and writes *out_kind (5=DecodeStall, 6=PresentStall) and *out_attempt
 * (1-based) when one is pending, else 0 (including NULL input). */
int32_t ct_session_poll_watchdog_action(const CtSession *s, int32_t *out_kind,
                                        uint32_t *out_attempt);
/* Receive-stage stall indicator (UI readback). 1/0, or 0 on NULL/no
 * session. */
int32_t ct_session_stalled(const CtSession *s);
/* Any stage's ladder exhausted into the terminal reconnect rung; the
 * session thread has ended (state Failed) and the shell should rebuild.
 * 1/0, or 0 on NULL/no session. */
int32_t ct_session_reconnect_requested(const CtSession *s);
/* Hold/resume watchdog escalation across all three stages (window
 * minimized/hidden). Idempotent; picked up on the next 50ms session tick.
 * NULL/no session is a no-op. */
void ct_session_set_watchdog_paused(const CtSession *s, int32_t paused);

/* POD incident snapshot (G004): the session's real generation (the
 * ct_start lease value — see the G002 section note above; distinct from,
 * and stable across, the unrelated per-recovery `recovery_generation`
 * below), active epoch, last frame id, symbol/loss/queue counters
 * (VideoReceiverStats passthrough, itself a FecDecoderStats superset),
 * last ACK/IDR results, all three stage heartbeat ages plus audio, and the
 * open decoder-recovery generation/epoch, if any.
 * `_present`/`_open` fields are 1/0, not C bool, for FFI-width stability. */
typedef struct CtIncidentSnapshot {
    uint64_t generation;
    uint32_t last_frame_id;
    uint8_t  last_frame_id_present;
    uint32_t active_epoch;
    uint8_t  active_epoch_present;
    uint64_t source_symbols_received;
    uint64_t repair_symbols_received;
    uint64_t symbols_recovered;
    uint64_t frames_recovered;
    uint64_t frames_dropped_awaiting_idr;
    uint64_t loss_spans;
    uint64_t loss_spans_recovered;
    uint32_t queue_len;
    uint32_t last_ack;
    uint8_t  last_ack_present;
    uint32_t last_idr_attempt;
    uint64_t receive_age_ms;
    uint64_t decode_age_ms;
    uint64_t present_age_ms;
    uint64_t audio_age_ms;
    uint8_t  recovery_open;
    uint64_t recovery_generation;
    uint32_t recovery_epoch;
} CtIncidentSnapshot;

/* Pollable G004 incident snapshot. Returns 1 and fills *out when at least
 * one watchdog stage has acted since ct_start, else 0 (including NULL
 * input). Non-consuming — safe to poll repeatedly (e.g. from a telemetry
 * timer) without racing the event that produced it. */
int32_t ct_session_poll_incident(const CtSession *s, CtIncidentSnapshot *out);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* BETTERPARSEC_CLIENT_TRANSPORT_H */
