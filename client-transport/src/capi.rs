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

use std::sync::Mutex;
use std::time::Duration;

use transport_core::video_rx::{DecodeUnit, RxEvent, VideoReceiver};

use crate::frame_queue::{DEFAULT_FRAME_CAP, FrameQueue};

// ── Objects ───────────────────────────────────────────────────────────────

/// Opaque receiver handle (`CtReceiver*` in C).
pub struct CtReceiver {
    rx: Mutex<VideoReceiver>,
    queue: FrameQueue,
    /// Latest un-polled ACK value. ACKs are cumulative
    /// (highest_fully_decoded), so newest-wins collapsing is lossless for
    /// the host window — see fec-framing.md §2.
    pending_ack: Mutex<Option<u32>>,
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
        rx: Mutex::new(VideoReceiver::new(now_ms)),
        queue: FrameQueue::new(DEFAULT_FRAME_CAP),
        pending_ack: Mutex::new(None),
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
    r.queue.close();
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

    let events = lock_ignore_poison(&r.rx).on_message(bytes, now_ms);
    for ev in events {
        match ev {
            RxEvent::Frame(unit) => {
                r.queue.push(unit);
            }
            RxEvent::Ack(a) => {
                *lock_ignore_poison(&r.pending_ack) = Some(a);
            }
        }
    }
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
    if let Some(a) = lock_ignore_poison(&r.rx).tick(now_ms) {
        *lock_ignore_poison(&r.pending_ack) = Some(a);
    }
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
    match lock_ignore_poison(&r.pending_ack).take() {
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
    let rx_flag = lock_ignore_poison(&r.rx).poll_needs_idr();
    let overflow = r.queue.take_overflowed();
    i32::from(rx_flag || overflow)
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
    match r.queue.wait_pop(Duration::from_millis(timeout_ms)) {
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
            assert!(ct_receiver_wait_frame(std::ptr::null_mut(), 0).is_null());
            assert_eq!(ct_frame_view(std::ptr::null(), std::ptr::null_mut()), 0);
            ct_frame_free(std::ptr::null_mut());
        }

        let rx = Rx::new(0);
        unsafe {
            // NULL buf with non-zero len must be rejected, not dereferenced.
            ct_receiver_on_message(rx.p(), std::ptr::null(), 5, 0);
            // NULL out pointer.
            assert_eq!(ct_receiver_poll_ack(rx.p(), std::ptr::null_mut()), 0);
        }
        feed(&rx, &[0xFF, 1, 2, 3], 0); // unknown symbol kind
        assert!(pop_frame(&rx, 5).is_none());
    }
}
