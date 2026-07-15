//! C ABI surface consumed by the moonlight-qt fork's `our_transport.cpp`
//! shim (m6-native-spike.md §B-1 Option-3).
//!
//! Threading contract (mirrors the shim design):
//! - `ct_receiver_on_message` / `ct_receiver_tick` / `ct_receiver_poll_*`:
//!   transport receive thread.
//! - `ct_receiver_wait_frame` / `ct_frame_*`: FFmpeg decoder thread
//!   (the `LiWaitForNextVideoFrame` replacement pull loop).
//! - `ct_receiver_close` unblocks the decoder thread; `ct_receiver_free`
//!   must only run after both threads stopped using the pointer.
//!
//! The mirror C header lives at `client-transport/include/client_transport.h`
//! and must stay in sync with this file.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use transport_core::video_rx::{DecodeUnit, RxEvent, VideoReceiver};

use crate::frame_queue::{DEFAULT_AUDIO_CAP, DEFAULT_FRAME_CAP, FrameQueue, SampleQueue};

// ── Objects ───────────────────────────────────────────────────────────────

/// Shared receive core: FEC receive pipeline + frame queue + ack latch.
/// `Arc`-shared between the C ABI handle and the Rust session
/// (`crate::session`) so both can drive the same pipeline.
pub struct RxCore {
    rx: Mutex<VideoReceiver>,
    queue: FrameQueue,
    /// Latest un-polled ACK value. ACKs are cumulative
    /// (highest_fully_decoded), so newest-wins collapsing is lossless for
    /// the host window — see fec-framing.md §2.
    pending_ack: Mutex<Option<u32>>,
    /// Latched by the decoder when it hits an unrecoverable bitstream
    /// error (e.g. missing reference after frame-queue eviction); drained
    /// through [`RxCore::poll_needs_idr`] like the receiver-side flags.
    decode_needs_idr: AtomicBool,
    /// Monotonic count of frames delivered to the frame queue (M4 stall
    /// watchdog frame signal; never reset).
    frames_delivered: AtomicU64,
    /// Opus packets from the audio RTP track (session `on_track` pushes;
    /// the audio render thread pops). Independent of the video pipeline.
    audio: SampleQueue,
}

impl RxCore {
    pub fn new(now_ms: u64) -> Self {
        Self {
            rx: Mutex::new(VideoReceiver::new(now_ms)),
            queue: FrameQueue::new(DEFAULT_FRAME_CAP),
            pending_ack: Mutex::new(None),
            decode_needs_idr: AtomicBool::new(false),
            frames_delivered: AtomicU64::new(0),
            audio: SampleQueue::new(DEFAULT_AUDIO_CAP),
        }
    }

    pub fn on_message(&self, bytes: &[u8], now_ms: u64) {
        let events = lock_ignore_poison(&self.rx).on_message(bytes, now_ms);
        for ev in events {
            match ev {
                RxEvent::Frame(unit) => {
                    self.queue.push(unit);
                    self.frames_delivered.fetch_add(1, Ordering::Relaxed);
                }
                RxEvent::Ack(a) => {
                    *lock_ignore_poison(&self.pending_ack) = Some(a);
                }
            }
        }
    }

    pub fn tick(&self, now_ms: u64) {
        if let Some(a) = lock_ignore_poison(&self.rx).tick(now_ms) {
            *lock_ignore_poison(&self.pending_ack) = Some(a);
        }
    }

    pub fn poll_ack(&self) -> Option<u32> {
        lock_ignore_poison(&self.pending_ack).take()
    }

    /// Monotonic delivered-frame count (M4 stall watchdog frame signal).
    pub fn frames_delivered(&self) -> u64 {
        self.frames_delivered.load(Ordering::Relaxed)
    }

    /// Latch a needs-IDR request from the decode side. Collapses with the
    /// receiver/overflow flags into the next [`RxCore::poll_needs_idr`].
    pub fn request_idr(&self) {
        self.decode_needs_idr.store(true, Ordering::Release);
    }

    pub fn poll_needs_idr(&self) -> bool {
        let rx_flag = lock_ignore_poison(&self.rx).poll_needs_idr();
        let overflow = self.queue.take_overflowed();
        let decode = self.decode_needs_idr.swap(false, Ordering::AcqRel);
        rx_flag || overflow || decode
    }

    pub fn wait_frame(&self, timeout: Duration) -> Option<DecodeUnit> {
        self.queue.wait_pop(timeout)
    }

    /// Push one opus packet from the audio RTP track (transport thread).
    pub fn push_audio(&self, pkt: Vec<u8>) {
        self.audio.push(pkt);
    }

    /// Block up to `timeout` for the next opus packet (audio thread).
    /// `None` on timeout or after [`RxCore::close`].
    pub fn wait_audio(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.audio.wait_pop(timeout)
    }

    pub fn close(&self) {
        self.queue.close();
        self.audio.close();
    }
}

/// Opaque receiver handle (`CtReceiver*` in C).
pub struct CtReceiver {
    pub(crate) core: Arc<RxCore>,
}

/// Opaque frame handle (`CtFrame*` in C). Owns the frame bytes; the view
/// returned by [`ct_frame_view`] borrows from it until [`ct_frame_free`].
pub struct CtFrame(DecodeUnit);

/// POD view of a frame for C (`data` borrows from the `CtFrame`).
#[repr(C)]
pub struct CtDecodeUnit {
    pub frame_id: u32,
    /// 1 = key/IDR frame, 0 = delta.
    pub is_key: u8,
    pub timestamp_us: u32,
    pub duration_us: i64,
    pub data: *const u8,
    pub data_len: usize,
}

// ── Lifecycle ─────────────────────────────────────────────────────────────

/// Create a receiver. `now_ms`: caller monotonic clock in milliseconds.
/// Returns an owned pointer; release with [`ct_receiver_free`].
#[unsafe(no_mangle)]
pub extern "C" fn ct_receiver_new(now_ms: u64) -> *mut CtReceiver {
    Box::into_raw(Box::new(CtReceiver {
        core: Arc::new(RxCore::new(now_ms)),
    }))
}

/// Close the frame queue, waking any decoder thread blocked in
/// [`ct_receiver_wait_frame`]. Idempotent. NULL is ignored.
///
/// # Safety
/// `p` must be NULL or a live pointer from [`ct_receiver_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_close(p: *mut CtReceiver) {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return;
    };
    r.core.close();
}

/// Destroy the receiver. NULL is ignored.
///
/// # Safety
/// `p` must be NULL or an owned pointer from [`ct_receiver_new`] that no
/// other thread touches after this call. Call [`ct_receiver_close`] first
/// and join the decoder thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_free(p: *mut CtReceiver) {
    if !p.is_null() {
        drop(unsafe { Box::from_raw(p) });
    }
}

// ── Receive path (transport thread) ───────────────────────────────────────

/// Feed one raw `video_fec` DataChannel message. Completed frames land in
/// the frame queue; fired ACKs latch for [`ct_receiver_poll_ack`].
/// Malformed messages are silently dropped.
///
/// # Safety
/// `p` as in [`ct_receiver_free`]; `buf` must point to `len` readable bytes
/// (NULL only when `len == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_on_message(
    p: *mut CtReceiver,
    buf: *const u8,
    len: usize,
    now_ms: u64,
) {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return;
    };
    if buf.is_null() && len != 0 {
        return;
    }
    let bytes: &[u8] = if len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(buf, len) }
    };
    r.core.on_message(bytes, now_ms);
}

/// ~50 ms timer tick (keeps the host encoder window advancing on idle
/// streams). Any fired ACK latches for [`ct_receiver_poll_ack`].
///
/// # Safety
/// `p` as in [`ct_receiver_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_tick(p: *mut CtReceiver, now_ms: u64) {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return;
    };
    r.core.tick(now_ms);
}

/// Take the latest un-sent ACK. Returns 1 and writes `*out` when one is
/// pending, else 0. Caller sends `AckMsg::Ack(*out)` on `video_fec_ack`.
///
/// # Safety
/// `p` as in [`ct_receiver_free`]; `out` must be a valid writable u32.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_poll_ack(p: *mut CtReceiver, out: *mut u32) -> i32 {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    match r.core.poll_ack() {
        Some(a) => {
            unsafe { *out = a };
            1
        }
        None => 0,
    }
}

/// Latched needs-IDR poll (receiver loss/eviction OR frame-queue overflow).
/// Returns 1 at most once per latch; caller sends `AckMsg::NeedsIdr`.
///
/// # Safety
/// `p` as in [`ct_receiver_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_poll_needs_idr(p: *mut CtReceiver) -> i32 {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return 0;
    };
    i32::from(r.core.poll_needs_idr())
}

// ── Decoder pull loop (decoder thread) ────────────────────────────────────

/// Block up to `timeout_ms` for the next frame — the
/// `LiWaitForNextVideoFrame` replacement. Returns an owned `CtFrame*`
/// (release with [`ct_frame_free`]) or NULL on timeout / after
/// [`ct_receiver_close`].
///
/// # Safety
/// `p` as in [`ct_receiver_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_wait_frame(
    p: *mut CtReceiver,
    timeout_ms: u64,
) -> *mut CtFrame {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return std::ptr::null_mut();
    };
    match r.core.wait_frame(Duration::from_millis(timeout_ms)) {
        Some(unit) => Box::into_raw(Box::new(CtFrame(unit))),
        None => std::ptr::null_mut(),
    }
}

/// Fill `*out` with a view of the frame. The `data` pointer stays valid
/// until [`ct_frame_free`]. Returns 1 on success, 0 on NULL input.
///
/// # Safety
/// `f` must be NULL or a live pointer from [`ct_receiver_wait_frame`];
/// `out` must be a valid writable [`CtDecodeUnit`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_frame_view(f: *const CtFrame, out: *mut CtDecodeUnit) -> i32 {
    let Some(frame) = (unsafe { f.as_ref() }) else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let u = &frame.0;
    unsafe {
        *out = CtDecodeUnit {
            frame_id: u.frame_id,
            is_key: u8::from(u.is_key),
            timestamp_us: u.timestamp_us,
            duration_us: u.duration_us,
            data: u.data.as_ptr(),
            data_len: u.data.len(),
        };
    }
    1
}

/// Release a frame from [`ct_receiver_wait_frame`]. NULL is ignored.
///
/// # Safety
/// `f` must be NULL or an owned pointer from [`ct_receiver_wait_frame`];
/// any [`CtDecodeUnit`] view of it is dangling afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_frame_free(f: *mut CtFrame) {
    if !f.is_null() {
        drop(unsafe { Box::from_raw(f) });
    }
}

// ── Audio pull loop (audio thread) ────────────────────────────────────────

/// Block up to `timeout_ms` for the next opus packet and copy it into
/// `buf` (at most `cap` bytes). Returns the full packet length (caller
/// detects truncation when it exceeds `cap`; 4096 always suffices for
/// RFC 7587 payloads), 0 on timeout / after [`ct_receiver_close`], -1 on
/// NULL input. The packet is consumed either way.
///
/// # Safety
/// `p` as in [`ct_receiver_free`]; `buf` must point to `cap` writable
/// bytes (NULL only when `cap == 0`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_wait_audio(
    p: *mut CtReceiver,
    timeout_ms: u64,
    buf: *mut u8,
    cap: usize,
) -> isize {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return -1;
    };
    if buf.is_null() && cap != 0 {
        return -1;
    }
    match r.core.wait_audio(Duration::from_millis(timeout_ms)) {
        Some(pkt) => {
            let n = pkt.len().min(cap);
            if n > 0 {
                unsafe { std::ptr::copy_nonoverlapping(pkt.as_ptr(), buf, n) };
            }
            pkt.len() as isize
        }
        None => 0,
    }
}

// ── Session (connection) ABI ──────────────────────────────────────────────

/// C-side session configuration for [`ct_start`]. All strings are
/// NUL-terminated UTF-8; lifetimes only need to cover the `ct_start` call.
#[repr(C)]
pub struct CtSessionConfig {
    /// e.g. `https://192.168.0.10:8080` (no trailing slash).
    pub base_url: *const std::os::raw::c_char,
    pub username: *const std::os::raw::c_char,
    pub password: *const std::os::raw::c_char,
    pub host_id: u32,
    pub app_id: u32,
    pub bitrate_kbps: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// 1 = accept any TLS certificate (dev only). When 0, `cert_sha256`
    /// must point to the 32-byte SHA-256 of the server certificate (DER).
    pub insecure_tls: u8,
    pub cert_sha256: *const u8,
}

/// Opaque session handle (`CtSession*` in C).
pub struct CtSession(Option<crate::session::Session>);

unsafe fn cstr_arg(p: *const std::os::raw::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe { std::ffi::CStr::from_ptr(p) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

/// Start a client session feeding the given receiver. Returns NULL on
/// invalid config. The receiver must stay alive until after [`ct_stop`].
///
/// # Safety
/// `cfg` must point to a valid [`CtSessionConfig`]; `rx` must be a live
/// pointer from [`ct_receiver_new`]. String/pin pointers per field docs.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_start(
    cfg: *const CtSessionConfig,
    rx: *mut CtReceiver,
) -> *mut CtSession {
    let (Some(cfg), Some(rx_ref)) = (unsafe { cfg.as_ref() }, unsafe { rx.as_ref() }) else {
        return std::ptr::null_mut();
    };
    let (Some(base_url), Some(username), Some(password)) = (
        unsafe { cstr_arg(cfg.base_url) },
        unsafe { cstr_arg(cfg.username) },
        unsafe { cstr_arg(cfg.password) },
    ) else {
        return std::ptr::null_mut();
    };
    let trust = if cfg.insecure_tls != 0 {
        crate::tls::ServerTrust::InsecureAcceptAny
    } else if !cfg.cert_sha256.is_null() {
        let mut pin = [0u8; 32];
        pin.copy_from_slice(unsafe { std::slice::from_raw_parts(cfg.cert_sha256, 32) });
        crate::tls::ServerTrust::PinnedSha256(pin)
    } else {
        return std::ptr::null_mut(); // neither pin nor explicit insecure opt-in
    };

    let config = crate::session::SessionConfig {
        base_url,
        username,
        password,
        trust,
        flow: crate::flow::FlowConfig {
            host_id: cfg.host_id,
            app_id: cfg.app_id,
            video_frame_queue_size: 3,
            audio_sample_queue_size: 20,
            bitrate_kbps: cfg.bitrate_kbps,
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            supported_codecs: crate::session::H264_BIT,
        },
    };
    let session = crate::session::Session::start(config, rx_ref.core.clone());
    Box::into_raw(Box::new(CtSession(Some(session))))
}

/// Session state: 0=connecting 1=peer-connected 2=streaming 3=failed
/// 4=stopped, -1 on NULL.
///
/// # Safety
/// `s` must be NULL or a live pointer from [`ct_start`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_session_state(s: *const CtSession) -> i32 {
    let Some(s) = (unsafe { s.as_ref() }) else {
        return -1;
    };
    match &s.0 {
        Some(session) => session.state() as i32,
        None => crate::session::SessionState::Stopped as i32,
    }
}

/// Stop the session (joins its thread) and free the handle. NULL ignored.
/// The associated receiver is closed as a side effect (decoder unblocks).
///
/// # Safety
/// `s` must be NULL or an owned pointer from [`ct_start`]; not used after.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_stop(s: *mut CtSession) {
    if s.is_null() {
        return;
    }
    let mut boxed = unsafe { Box::from_raw(s) };
    if let Some(session) = boxed.0.take() {
        session.stop();
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn lock_ignore_poison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ── Tests (exercise the ABI exactly as the C++ shim will) ─────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use transport_core::fec::Symbol;
    use transport_core::fec_wire::{chunk_frame, encode_symbol_msg};

    /// RAII wrapper so a failing assert can't leak the receiver.
    struct Rx(*mut CtReceiver);
    impl Rx {
        fn new(now_ms: u64) -> Self {
            let p = ct_receiver_new(now_ms);
            assert!(!p.is_null());
            Self(p)
        }
        fn p(&self) -> *mut CtReceiver {
            self.0
        }
    }
    impl Drop for Rx {
        fn drop(&mut self) {
            unsafe {
                ct_receiver_close(self.0);
                ct_receiver_free(self.0);
            }
        }
    }

    fn feed(rx: &Rx, msg: &[u8], now_ms: u64) {
        unsafe { ct_receiver_on_message(rx.p(), msg.as_ptr(), msg.len(), now_ms) };
    }

    fn source_msgs_for_frame(
        frame_id: u32,
        key: bool,
        ts_us: u32,
        data: &[u8],
        next_seq: &mut u32,
    ) -> Vec<Vec<u8>> {
        chunk_frame(frame_id, key, ts_us, data)
            .into_iter()
            .map(|chunk| {
                let msg = encode_symbol_msg(&Symbol::Source {
                    seq: *next_seq,
                    payload: chunk,
                });
                *next_seq += 1;
                msg
            })
            .collect()
    }

    #[test]
    fn decode_side_idr_request_latches_once() {
        let core = RxCore::new(0);
        assert!(!core.poll_needs_idr());
        core.request_idr();
        core.request_idr(); // collapses — cumulative like the other flags
        assert!(core.poll_needs_idr());
        assert!(!core.poll_needs_idr());
    }

    /// The watchdog frame signal counts queue pushes, monotonic, never
    /// reset — even when the frame is popped (or dropped) afterwards.
    #[test]
    fn frames_delivered_counts_queue_pushes() {
        let core = RxCore::new(0);
        assert_eq!(core.frames_delivered(), 0);
        let mut seq = 0;
        for frame_id in 0..3u32 {
            for msg in
                source_msgs_for_frame(frame_id, frame_id == 0, frame_id, &[1, 2, 3], &mut seq)
            {
                core.on_message(&msg, 0);
            }
        }
        assert_eq!(core.frames_delivered(), 3);
        // Popping does not reset the signal.
        let _ = core.wait_frame(Duration::from_millis(5));
        assert_eq!(core.frames_delivered(), 3);
    }

    fn pop_frame(rx: &Rx, timeout_ms: u64) -> Option<(CtDecodeUnit, Vec<u8>, *mut CtFrame)> {
        let f = unsafe { ct_receiver_wait_frame(rx.p(), timeout_ms) };
        if f.is_null() {
            return None;
        }
        let mut view = CtDecodeUnit {
            frame_id: 0,
            is_key: 0,
            timestamp_us: 0,
            duration_us: 0,
            data: std::ptr::null(),
            data_len: 0,
        };
        assert_eq!(unsafe { ct_frame_view(f, &raw mut view) }, 1);
        let bytes = if view.data_len == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(view.data, view.data_len) }.to_vec()
        };
        Some((view, bytes, f))
    }

    #[test]
    fn frames_flow_from_wire_to_wait_frame() {
        let rx = Rx::new(0);
        let mut seq = 0;

        let payload: Vec<u8> = (0..2000).map(|i| (i % 251) as u8).collect();
        for msg in source_msgs_for_frame(0, true, 16_667, &payload, &mut seq) {
            feed(&rx, &msg, 0);
        }

        let (view, bytes, f) = pop_frame(&rx, 100).expect("frame delivered");
        assert_eq!(view.frame_id, 0);
        assert_eq!(view.is_key, 1);
        assert_eq!(view.timestamp_us, 16_667);
        assert_eq!(view.duration_us, 16_667);
        assert_eq!(bytes, payload, "byte-identical across the ABI");
        unsafe { ct_frame_free(f) };

        // No second frame.
        assert!(pop_frame(&rx, 10).is_none());
    }

    #[test]
    fn ack_latches_and_newest_wins() {
        let rx = Rx::new(0);
        let mut seq = 0;

        // 40 single-chunk frames → the 32-symbol cadence fires at least once;
        // by symbol 40 a second advance has replaced the latched value.
        for i in 0..40u32 {
            for msg in source_msgs_for_frame(i, false, i * 1000, &[i as u8], &mut seq) {
                feed(&rx, &msg, 0);
            }
        }

        let mut out = 0u32;
        assert_eq!(unsafe { ct_receiver_poll_ack(rx.p(), &raw mut out) }, 1);
        assert!(out >= 31, "cumulative ack covers at least the 32nd symbol");
        assert_eq!(
            unsafe { ct_receiver_poll_ack(rx.p(), &raw mut out) },
            0,
            "latch consumed"
        );

        // Timer path: advance the clock and tick.
        for msg in source_msgs_for_frame(40, false, 40_000, &[40], &mut seq) {
            feed(&rx, &msg, 10);
        }
        unsafe { ct_receiver_tick(rx.p(), 100) };
        assert_eq!(unsafe { ct_receiver_poll_ack(rx.p(), &raw mut out) }, 1);
        assert_eq!(out, 40, "tick-driven ack reports newest hfd");

        // Drain frames so the queue-cap latch is not confused with rx state.
        while let Some((.., f)) = pop_frame(&rx, 5) {
            unsafe { ct_frame_free(f) };
        }
    }

    #[test]
    fn needs_idr_merges_receiver_and_queue_overflow() {
        let rx = Rx::new(0);
        let mut seq = 0;

        // Overflow the 16-cap frame queue without popping: 17 frames.
        for i in 0..17u32 {
            for msg in source_msgs_for_frame(i, false, i * 1000, &[i as u8], &mut seq) {
                feed(&rx, &msg, 0);
            }
        }
        assert_eq!(
            unsafe { ct_receiver_poll_needs_idr(rx.p()) },
            1,
            "queue overflow surfaces as needs-IDR"
        );
        assert_eq!(
            unsafe { ct_receiver_poll_needs_idr(rx.p()) },
            0,
            "latch consumed"
        );

        // Receiver-side latch: evict pending frames via 9 partials.
        for i in 100..109u32 {
            let chunks = chunk_frame(i, false, i, &[1, 2, 3]);
            // Send only the first chunk of a fake 2-chunk frame by crafting a
            // partial: reuse chunk 0 but declare chunk_count 2.
            let mut partial = chunks[0].clone();
            partial[6..8].copy_from_slice(&2u16.to_le_bytes());
            let msg = encode_symbol_msg(&Symbol::Source {
                seq,
                payload: partial,
            });
            seq += 1;
            feed(&rx, &msg, 0);
        }
        assert_eq!(
            unsafe { ct_receiver_poll_needs_idr(rx.p()) },
            1,
            "receiver eviction surfaces as needs-IDR"
        );
    }

    #[test]
    fn close_unblocks_decoder_thread() {
        let rx = Rx::new(0);
        let p = rx.p() as usize; // Send across the thread as an address.

        let waiter = std::thread::spawn(move || {
            let f = unsafe { ct_receiver_wait_frame(p as *mut CtReceiver, 30_000) };
            f.is_null()
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        unsafe { ct_receiver_close(rx.p()) };
        assert!(
            waiter.join().expect("waiter must not panic"),
            "close returns NULL to the blocked decoder thread"
        );
    }

    #[test]
    fn null_and_malformed_inputs_are_safe() {
        unsafe {
            ct_receiver_close(std::ptr::null_mut());
            ct_receiver_free(std::ptr::null_mut());
            ct_receiver_on_message(std::ptr::null_mut(), std::ptr::null(), 0, 0);
            ct_receiver_tick(std::ptr::null_mut(), 0);
            assert_eq!(
                ct_receiver_poll_ack(std::ptr::null_mut(), std::ptr::null_mut()),
                0
            );
            assert_eq!(ct_receiver_poll_needs_idr(std::ptr::null_mut()), 0);
            assert_eq!(
                ct_receiver_wait_audio(std::ptr::null_mut(), 0, std::ptr::null_mut(), 0),
                -1
            );
            assert!(ct_receiver_wait_frame(std::ptr::null_mut(), 0).is_null());
            assert_eq!(ct_frame_view(std::ptr::null(), std::ptr::null_mut()), 0);
            ct_frame_free(std::ptr::null_mut());
        }

        let rx = Rx::new(0);
        unsafe {
            // NULL buf with non-zero len must be rejected, not dereferenced.
            ct_receiver_on_message(rx.p(), std::ptr::null(), 5, 0);
            assert_eq!(
                ct_receiver_wait_audio(rx.p(), 0, std::ptr::null_mut(), 8),
                -1
            );
            // NULL out pointer.
            assert_eq!(ct_receiver_poll_ack(rx.p(), std::ptr::null_mut()), 0);
        }
        feed(&rx, &[0xFF, 1, 2, 3], 0); // unknown symbol kind
        assert!(pop_frame(&rx, 5).is_none());
    }

    #[test]
    fn audio_pull_copies_truncates_and_times_out() {
        let rx = Rx::new(0);
        let core = unsafe { &(*rx.p()).core };
        core.push_audio(vec![0xAA, 0xBB, 0xCC]);
        core.push_audio(vec![0x11; 10]);

        let mut buf = [0u8; 8];
        // Full copy.
        let n = unsafe { ct_receiver_wait_audio(rx.p(), 0, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(n, 3);
        assert_eq!(&buf[..3], &[0xAA, 0xBB, 0xCC]);
        // Truncation: returns the full length, copies only `cap`.
        let n = unsafe { ct_receiver_wait_audio(rx.p(), 0, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(n, 10);
        assert_eq!(&buf, &[0x11; 8]);
        // Timeout.
        let n = unsafe { ct_receiver_wait_audio(rx.p(), 10, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(n, 0);
        // Closed → 0 immediately.
        unsafe { ct_receiver_close(rx.p()) };
        let n = unsafe { ct_receiver_wait_audio(rx.p(), 30_000, buf.as_mut_ptr(), buf.len()) };
        assert_eq!(n, 0);
    }
}
