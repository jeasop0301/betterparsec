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

/* Receive path (transport thread). */
void ct_receiver_on_message(CtReceiver *p, const uint8_t *buf, size_t len,
                            uint64_t now_ms);
void ct_receiver_tick(CtReceiver *p, uint64_t now_ms); /* call every ~50 ms */

/* Returns 1 and writes *out when an ACK is pending (send as 4-byte LE u32
 * on video_fec_ack), else 0. Cumulative: newest value supersedes. */
int32_t ct_receiver_poll_ack(CtReceiver *p, uint32_t *out);

/* Returns 1 at most once per latch (send 1-byte 0x00 NeedsIdr on
 * video_fec_ack), else 0. */
int32_t ct_receiver_poll_needs_idr(CtReceiver *p);

/* Decoder pull loop (decoder thread) — LiWaitForNextVideoFrame replacement.
 * NULL on timeout or after ct_receiver_close. */
CtFrame *ct_receiver_wait_frame(CtReceiver *p, uint64_t timeout_ms);
int32_t  ct_frame_view(const CtFrame *f, CtDecodeUnit *out);
void     ct_frame_free(CtFrame *f);

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

/* NULL on invalid config. The receiver must outlive the session. */
CtSession *ct_start(const CtSessionConfig *cfg, CtReceiver *rx);
/* 0=connecting 1=peer-connected 2=streaming 3=failed 4=stopped, -1 NULL. */
int32_t ct_session_state(const CtSession *s);
/* Stops (joins) and frees. Closes the receiver queue as a side effect. */
void ct_stop(CtSession *s);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* BETTERPARSEC_CLIENT_TRANSPORT_H */
