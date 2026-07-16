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

use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::frame_queue::{
    DEFAULT_AUDIO_CAP, DEFAULT_FRAME_CAP, FrameQueue, SampleQueue, VideoEvent,
};
use transport_core::fec_wire::{
    AckMsg, Epoch, V2ControlMsg, encode_ack_msg, encode_control_msg_v2,
};
use transport_core::video_rx::{DecodeUnit, DiscontinuityReason, RxEvent, VideoReceiver};

// ── Objects ───────────────────────────────────────────────────────────────

/// Shared receive core: FEC receive pipeline + frame queue + ack latch.
/// `Arc`-shared between the C ABI handle and the Rust session
/// (`crate::session`) so both can drive the same pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FecCodec {
    /// Standalone C-ABI receivers retain legacy v1 behavior until a session
    /// explicitly starts negotiation.
    V1,
    /// A signaling session is live but has not selected its FEC wire yet.
    /// DataChannel bytes are not admitted during this interval.
    Unconfigured,
    V2(Epoch),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FecControl {
    Subscribe,
    Ack(u32),
    NeedsIdr { reason: u8 },
}

pub struct RxCore {
    rx: Mutex<VideoReceiver>,
    /// Selected wire contract. This gate is deliberately outside
    /// `VideoReceiver`: the generic receiver can follow newer epochs, while a
    /// negotiated session must reject every epoch except its selected one.
    fec_codec: Mutex<FecCodec>,
    /// Serializes standalone C-ABI ingress against session acquisition. A
    /// receiver is deliberately single-session because its blocking queues
    /// cannot safely be reopened after `ct_stop` closes them.
    lease: Mutex<LeaseState>,
    /// A selected v2 generation must begin with an IDR even when its first
    /// packet uses the selected epoch (the generic receiver only gates after a
    /// discontinuity).
    v2_key_gate: AtomicBool,
    /// Configuration is write-once for each receiver generation.
    fec_configured: AtomicBool,
    queue: FrameQueue,
    /// Latest un-polled ACK value. ACKs are cumulative
    /// (highest_fully_decoded), so newest-wins collapsing is lossless for
    /// the host window — see fec-framing.md §2.
    pending_ack: Mutex<Option<u32>>,
    /// Latest transport discontinuity, standalone-poll latch consumed by
    /// [`RxCore::take_discontinuity`] (`ct_receiver_poll_discontinuity`'s
    /// Rust-side source). The ordered queue path
    /// ([`RxCore::wait_event`]/[`RxCore::try_event`] and the C ABI
    /// `ct_event_discontinuity`) is the decoder-recovery source of truth —
    /// it carries the recovery generation independently via
    /// [`FrameQueue`]'s own state, this field does not.
    discontinuity: Mutex<Option<RxEvent>>,
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
    /// SDP-negotiated audio channel count (already clamped by
    /// `crate::flow::negotiated_channels`), latched by the session's
    /// `ConnectionComplete` handler. `0` = unknown/not yet negotiated —
    /// `app-native`'s audio thread treats that as "decode stereo".
    audio_channels: AtomicU16,
    /// Monotonic count of opus packets pushed from the RTP track (G004
    /// audio heartbeat age source; never reset). See [`RxCore::push_audio`].
    audio_pushed: AtomicU64,
    /// Monotonic count of native decoder output units (G004 decode-stage
    /// heartbeat; fed ONLY by [`RxCore::note_decoded_output`] — never by
    /// receive-side symbol/frame arrival).
    decoded_output: AtomicU64,
    /// Monotonic count of native present-path successes (G004 present-
    /// stage heartbeat; fed ONLY by [`RxCore::note_presented`]).
    presented: AtomicU64,
    /// Diagnostic (non-consuming) copy of the latest ACK value latched
    /// alongside `pending_ack` — G004 incident snapshots read this without
    /// disturbing the real (consuming) `poll_ack` latch.
    last_ack_seen: AtomicU32,
    last_ack_present: AtomicBool,
    /// Diagnostic frame id of the most recently queued frame (G004
    /// incident snapshots).
    last_frame_id: AtomicU32,
    last_frame_id_present: AtomicBool,
}

#[derive(Debug, Default)]
struct LeaseState {
    active: Option<SessionGeneration>,
    ever_started: bool,
    closed: bool,
    next_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionGeneration(u64);

impl SessionGeneration {
    /// The raw lease value — used to stamp the real session generation
    /// into `WatchdogSupervisor`/`WatchdogStatus` (G004 incident
    /// telemetry) instead of a decorative self-generated counter.
    pub(crate) fn value(self) -> u64 {
        self.0
    }
}

impl RxCore {
    pub fn new(now_ms: u64) -> Self {
        Self {
            rx: Mutex::new(VideoReceiver::new(now_ms)),
            fec_codec: Mutex::new(FecCodec::V1),
            lease: Mutex::new(LeaseState::default()),
            v2_key_gate: AtomicBool::new(false),
            fec_configured: AtomicBool::new(false),
            queue: FrameQueue::new(DEFAULT_FRAME_CAP),
            pending_ack: Mutex::new(None),
            discontinuity: Mutex::new(None),
            decode_needs_idr: AtomicBool::new(false),
            frames_delivered: AtomicU64::new(0),
            audio: SampleQueue::new(DEFAULT_AUDIO_CAP),
            audio_channels: AtomicU16::new(0),
            audio_pushed: AtomicU64::new(0),
            decoded_output: AtomicU64::new(0),
            presented: AtomicU64::new(0),
            last_ack_seen: AtomicU32::new(0),
            last_ack_present: AtomicBool::new(false),
            last_frame_id: AtomicU32::new(0),
            last_frame_id_present: AtomicBool::new(false),
        }
    }

    /// Start a fresh negotiated generation. This is intentionally only called
    /// after the C ABI session lease has been acquired.
    pub fn begin_fec_negotiation(&self) {
        *lock_ignore_poison(&self.rx) = VideoReceiver::new(0);
        *lock_ignore_poison(&self.fec_codec) = FecCodec::Unconfigured;
        *lock_ignore_poison(&self.pending_ack) = None;
        *lock_ignore_poison(&self.discontinuity) = None;
        self.queue.clear_recovery();
        self.decode_needs_idr.store(false, Ordering::Release);
        self.v2_key_gate.store(false, Ordering::Release);
        self.fec_configured.store(false, Ordering::Release);
        // A queue cannot be reopened after close. A generation is therefore
        // one-shot; pre-session C ingress is rejected once leased, and stale
        // pre-negotiation video and overflow state are drained before the new
        // receiver is exposed.
        while self.queue.try_pop().is_some() {}
        let _ = self.queue.take_overflowed();
    }

    /// Atomically reserve this receiver for its one and only C ABI session.
    pub(crate) fn acquire_session_lease(&self) -> Result<SessionGeneration, &'static str> {
        let mut lease = lock_ignore_poison(&self.lease);
        if lease.closed {
            return Err("receiver queues are closed");
        }
        if lease.active.is_some() {
            return Err("receiver already has an active session");
        }
        if lease.ever_started {
            return Err("receiver queues are single-session");
        }
        lease.next_generation = lease.next_generation.wrapping_add(1).max(1);
        let generation = SessionGeneration(lease.next_generation);
        lease.active = Some(generation);
        lease.ever_started = true;
        Ok(generation)
    }

    pub(crate) fn release_session_lease(
        &self,
        generation: SessionGeneration,
    ) -> Result<(), &'static str> {
        let mut lease = lock_ignore_poison(&self.lease);
        if lease.active != Some(generation) {
            return Err("stale session generation");
        }
        lease.active = None;
        Ok(())
    }

    fn on_standalone_message(&self, bytes: &[u8], now_ms: u64) {
        let lease = lock_ignore_poison(&self.lease);
        if lease.active.is_none() {
            self.on_message(bytes, now_ms);
        }
    }

    fn on_standalone_tick(&self, now_ms: u64) {
        let lease = lock_ignore_poison(&self.lease);
        if lease.active.is_none() {
            self.tick(now_ms);
        }
    }

    fn poll_standalone_ack(&self) -> Option<u32> {
        let lease = lock_ignore_poison(&self.lease);
        lease.active.is_none().then(|| self.poll_ack()).flatten()
    }

    fn poll_standalone_needs_idr(&self) -> bool {
        let lease = lock_ignore_poison(&self.lease);
        lease.active.is_none() && self.poll_needs_idr()
    }

    fn poll_standalone_discontinuity(&self) -> Option<RxEvent> {
        let lease = lock_ignore_poison(&self.lease);
        lease
            .active
            .is_none()
            .then(|| self.take_discontinuity())
            .flatten()
    }

    /// Latch the negotiated FEC contract before accepting media bytes. A
    /// generation may select exactly one codec.
    pub fn configure_fec(&self, version: u8, epoch: Option<NonZeroU32>) -> bool {
        let codec = match (version, epoch) {
            (1, None) => FecCodec::V1,
            (2, Some(epoch)) => FecCodec::V2(
                Epoch::new(epoch.get()).expect("NonZeroU32 always yields a valid FEC epoch"),
            ),
            _ => return false,
        };
        if self.fec_configured.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut selected = lock_ignore_poison(&self.fec_codec);
        *selected = codec;
        self.v2_key_gate
            .store(matches!(codec, FecCodec::V2(_)), Ordering::Release);
        true
    }

    /// Currently selected outbound wire epoch, if any. `None` before
    /// negotiation completes and for FEC v1 (no epoch concept) — the
    /// session-side outbound ACK slot uses this to key newest-wins
    /// collapsing per epoch so a stale value from a superseded epoch can
    /// never survive a renegotiation.
    pub fn selected_epoch(&self) -> Option<u32> {
        match *lock_ignore_poison(&self.fec_codec) {
            FecCodec::V2(epoch) => Some(epoch.get()),
            FecCodec::V1 | FecCodec::Unconfigured => None,
        }
    }

    pub fn control_message(&self, control: FecControl) -> Option<Vec<u8>> {
        match *lock_ignore_poison(&self.fec_codec) {
            FecCodec::Unconfigured => None,
            FecCodec::V1 => Some(encode_ack_msg(&match control {
                FecControl::Subscribe => AckMsg::Subscribe,
                FecControl::Ack(seq) => AckMsg::Ack(seq),
                FecControl::NeedsIdr { .. } => AckMsg::NeedsIdr,
            })),
            FecCodec::V2(epoch) => Some(encode_control_msg_v2(&match control {
                FecControl::Subscribe => V2ControlMsg::Subscribe { epoch },
                FecControl::Ack(highest_seq) => V2ControlMsg::Ack { epoch, highest_seq },
                FecControl::NeedsIdr { reason } => V2ControlMsg::NeedsIdr { epoch, reason },
            })),
        }
    }

    pub fn on_message(&self, bytes: &[u8], now_ms: u64) {
        if !self.accepts_message(bytes) {
            return;
        }
        let events = lock_ignore_poison(&self.rx).on_message(bytes, now_ms);
        self.consume_events(events);
    }

    pub fn tick(&self, now_ms: u64) {
        let events = lock_ignore_poison(&self.rx).tick_events(now_ms);
        self.consume_events(events);
    }

    fn accepts_message(&self, bytes: &[u8]) -> bool {
        match *lock_ignore_poison(&self.fec_codec) {
            FecCodec::Unconfigured => false,
            FecCodec::V1 => matches!(bytes.first(), Some(0x00 | 0x01)),
            FecCodec::V2(epoch) => {
                matches!(bytes.first(), Some(0x02 | 0x03))
                    && bytes.len() >= 5
                    && u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) == epoch.get()
            }
        }
    }

    fn consume_events(&self, events: Vec<RxEvent>) {
        for ev in events {
            match ev {
                RxEvent::Frame(unit) => {
                    if self.v2_key_gate.load(Ordering::Acquire) {
                        if !unit.is_key {
                            continue;
                        }
                        self.v2_key_gate.store(false, Ordering::Release);
                    }
                    let frame_id = unit.frame_id;
                    if self.queue.push_frame_reporting_overflow(unit).is_some() {
                        self.frames_delivered.fetch_add(1, Ordering::Relaxed);
                        self.last_frame_id.store(frame_id, Ordering::Relaxed);
                        self.last_frame_id_present.store(true, Ordering::Release);
                        // Recovery (on overflow) already opened atomically
                        // with the reset event under the queue's own lock
                        // (see `FrameQueue::push_frame_reporting_overflow`)
                        // — a consumer that pops that reset can never
                        // observe recovery as closed/never-opened.
                    }
                }
                RxEvent::Ack(a) => {
                    *lock_ignore_poison(&self.pending_ack) = Some(a);
                    self.last_ack_seen.store(a, Ordering::Relaxed);
                    self.last_ack_present.store(true, Ordering::Release);
                }
                // `VideoReceiver` already latches NeedsIdr for every
                // discontinuity. Every discontinuity also enters the frame
                // queue in order (ahead of any later frame) and opens
                // decoder-recovery for a fresh generation — G002's queue →
                // flush → acknowledge-decoded-key contract.
                RxEvent::Discontinuity { epoch, reason } => {
                    *lock_ignore_poison(&self.discontinuity) =
                        Some(RxEvent::Discontinuity { epoch, reason });
                    // Recovery opens atomically with this event becoming
                    // poppable (both under the queue's own lock in
                    // `push_discontinuity`) — a consumer that pops this
                    // reset can never observe recovery as
                    // closed/never-opened.
                    self.queue.push_discontinuity(epoch, reason);
                }
            }
        }
    }

    pub fn poll_ack(&self) -> Option<u32> {
        lock_ignore_poison(&self.pending_ack).take()
    }

    /// Monotonic delivered-frame count (M4 stall watchdog frame signal).
    pub fn frames_delivered(&self) -> u64 {
        self.frames_delivered.load(Ordering::Relaxed)
    }
    /// Snapshot FEC/reassembly recovery state for stall diagnostics.
    pub fn video_stats(&self) -> transport_core::video_rx::VideoReceiverStats {
        lock_ignore_poison(&self.rx).stats()
    }

    /// Frame-queue depth (G004 incident "queue" bound counter).
    pub fn queue_len(&self) -> usize {
        self.queue.len()
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
    /// Take the latest transport discontinuity for a future decoder-flush
    /// integration. This is Rust-only; the existing C ABI remains unchanged.
    pub fn take_discontinuity(&self) -> Option<RxEvent> {
        lock_ignore_poison(&self.discontinuity).take()
    }

    pub fn wait_frame(&self, timeout: Duration) -> Option<DecodeUnit> {
        self.queue.wait_pop(timeout)
    }

    /// Non-blocking pop of the next reassembled frame, if one is already
    /// queued (drains stale backlog; the newest is what gets presented).
    pub fn try_frame(&self) -> Option<DecodeUnit> {
        self.queue.try_pop()
    }

    /// Block up to `timeout` for the next queued event (frame or
    /// discontinuity), in wire order. Decoder-recovery aware consumers
    /// should prefer this over [`wait_frame`](Self::wait_frame): a
    /// discontinuity is guaranteed to surface here before any frame it
    /// gates.
    pub fn wait_event(&self, timeout: Duration) -> Option<VideoEvent> {
        self.queue.wait_event(timeout)
    }

    /// Non-blocking pop of the next queued event, if one is already queued.
    pub fn try_event(&self) -> Option<VideoEvent> {
        self.queue.try_event()
    }

    /// Currently open decoder-recovery, as `(generation, epoch)`, or `None`
    /// when the decoder has no outstanding flush/decoded-key handshake.
    /// `generation` is the ack key: every discontinuity/overflow event opens
    /// a fresh generation (see [`FrameQueue`]), so a stale ack from a
    /// superseded discontinuity — including one that shares `epoch` with
    /// the current recovery, which most discontinuity reasons do — is
    /// correctly treated as stale.
    pub fn recovery(&self) -> Option<(u64, u32)> {
        self.queue.recovery()
    }

    /// Close decoder-recovery for `(generation, epoch)` after the decoder
    /// flushed and successfully decoded a key frame. Only closes when both
    /// match the currently open recovery — a stale ack (superseded by a
    /// later discontinuity, even a same-epoch one) or a wrong-epoch ack is
    /// a no-op. Returns whether the ack closed recovery.
    ///
    /// `frame_id` is diagnostic-only and unvalidated: recovery identity is
    /// carried entirely by `(generation, epoch)`, which already prevents an
    /// earlier/aliased ack from closing the wrong recovery. Validating frame
    /// identity too would require latching the first post-reset admitted
    /// key's id for no additional safety over what the generation key
    /// already provides.
    pub fn acknowledge_decoded_key(&self, generation: u64, epoch: u32, frame_id: u32) -> bool {
        let _ = frame_id;
        self.queue.acknowledge_recovery(generation, epoch)
    }

    /// Push one opus packet from the audio RTP track (transport thread).
    pub fn push_audio(&self, pkt: Vec<u8>) {
        self.audio.push(pkt);
        self.audio_pushed.fetch_add(1, Ordering::Relaxed);
    }

    /// Monotonic count of audio packets pushed (G004 audio heartbeat age
    /// source; never reset).
    pub fn audio_pushed_count(&self) -> u64 {
        self.audio_pushed.load(Ordering::Relaxed)
    }

    /// Record one unit of native decoder output (G004 decode-stage
    /// heartbeat). Call ONLY from the native decoder's output callback —
    /// receive-side symbol/frame arrival must never satisfy this signal.
    pub fn note_decoded_output(&self) {
        self.decoded_output.fetch_add(1, Ordering::Relaxed);
    }

    /// Monotonic count of decoded-output units (G004 decode-stage
    /// heartbeat source; never reset).
    pub fn decoded_output_count(&self) -> u64 {
        self.decoded_output.load(Ordering::Relaxed)
    }

    /// Record one successful native present (G004 present-stage
    /// heartbeat). Call ONLY from the native present path's success case.
    pub fn note_presented(&self) {
        self.presented.fetch_add(1, Ordering::Relaxed);
    }

    /// Monotonic count of successful presents (G004 present-stage
    /// heartbeat source; never reset).
    pub fn presented_count(&self) -> u64 {
        self.presented.load(Ordering::Relaxed)
    }

    /// Diagnostic (non-consuming) snapshot of the latest observed ACK
    /// value, for G004 incident reporting — does not disturb the real
    /// (consuming) [`RxCore::poll_ack`] latch.
    pub fn last_ack_seen(&self) -> Option<u32> {
        self.last_ack_present
            .load(Ordering::Acquire)
            .then(|| self.last_ack_seen.load(Ordering::Relaxed))
    }

    /// Diagnostic frame id of the most recently queued frame, for G004
    /// incident reporting.
    pub fn last_frame_id(&self) -> Option<u32> {
        self.last_frame_id_present
            .load(Ordering::Acquire)
            .then(|| self.last_frame_id.load(Ordering::Relaxed))
    }

    /// Block up to `timeout` for the next opus packet (audio thread).
    /// `None` on timeout or after [`RxCore::close`].
    pub fn wait_audio(&self, timeout: Duration) -> Option<Vec<u8>> {
        self.audio.wait_pop(timeout)
    }

    /// Latch the SDP-negotiated audio channel count (session's
    /// `ConnectionComplete` handler, already run through
    /// `crate::flow::negotiated_channels`).
    pub fn set_audio_channels(&self, channels: u16) {
        self.audio_channels.store(channels, Ordering::Release);
    }

    /// Current negotiated audio channel count; `0` means unknown/not yet
    /// negotiated (audio thread decodes stereo until this becomes nonzero).
    pub fn audio_channels(&self) -> u16 {
        self.audio_channels.load(Ordering::Acquire)
    }

    pub fn close(&self) {
        lock_ignore_poison(&self.lease).closed = true;
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
    r.core.on_standalone_message(bytes, now_ms);
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
    r.core.on_standalone_tick(now_ms);
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
    match r.core.poll_standalone_ack() {
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
    i32::from(r.core.poll_standalone_needs_idr())
}
/// Take one typed transport discontinuity. Returns 1 and writes the selected
/// FEC epoch and reason, or 0 when none is pending. Reason values are stable:
/// 1=epoch transition, 2=reorder gap, 3=reassembly eviction,
/// 4=metadata mismatch, 5=frame CRC, 6=memory cap, 7=FEC eviction.
///
/// # Safety
/// `p` must be null or a live receiver pointer returned by `ct_receiver_new`.
/// Non-null output pointers must be valid for one `u32` and one `u8` write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_poll_discontinuity(
    p: *mut CtReceiver,
    out_epoch: *mut u32,
    out_reason: *mut u8,
) -> i32 {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return 0;
    };
    if out_epoch.is_null() || out_reason.is_null() {
        return 0;
    }
    match r.core.poll_standalone_discontinuity() {
        Some(RxEvent::Discontinuity { epoch, reason }) => {
            unsafe {
                *out_epoch = epoch;
                *out_reason = discontinuity_reason_code(reason);
            }
            1
        }
        Some(_) | None => 0,
    }
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

// ── G004 native decode/present heartbeats ──────────────────────────────────
// Coordinated over IRC with the app-native lane: called from the FFmpeg
// decoder output callback and the D3D11 present success path respectively.
// Browser/receive-side symbol arrival must NEVER call these — that is the
// receive-stage heartbeat's job (`ct_receiver_tick`/`ct_receiver_on_message`
// already drive it via `frames_delivered`).

/// Record one native decoder output unit. Call ONLY from the decoder's
/// output callback (decoder thread).
///
/// # Safety
/// `p` as in [`ct_receiver_free`]. NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_note_decoded_output(p: *const CtReceiver) {
    if let Some(r) = unsafe { p.as_ref() } {
        r.core.note_decoded_output();
    }
}

/// Record one successful native present. Call ONLY from the present
/// path's success case (present thread).
///
/// # Safety
/// `p` as in [`ct_receiver_free`]. NULL is ignored.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_note_presented(p: *const CtReceiver) {
    if let Some(r) = unsafe { p.as_ref() } {
        r.core.note_presented();
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
pub struct CtSession {
    session: Option<crate::session::Session>,
}

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
/// invalid config, a closed receiver, or if another caller acquired the
/// receiver's one-shot session lease. The receiver must stay alive until after
/// [`ct_stop`].
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
    let Ok(session) = crate::session::Session::try_start(config, rx_ref.core.clone()) else {
        return std::ptr::null_mut();
    };
    Box::into_raw(Box::new(CtSession {
        session: Some(session),
    }))
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
    match &s.session {
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
    if let Some(session) = boxed.session.take() {
        session.stop();
    }
}

// ── G004 watchdog / incident ABI (session thread's 50ms tick publishes;
// any thread may poll) ──────────────────────────────────────────────────
//
// Action kinds mirror `WatchdogAction`'s declaration order in watchdog.rs:
// 0=Stall 1=Recovered 2=RequestIdr 3=RestartIce 4=Reconnect 5=DecodeStall
// 6=PresentStall. Only 5/6 are surfaced here — the receive-stage rungs
// 0-4 are already fully handled server-side (signaling sends) and
// reflected via `ct_session_stalled`/`ct_session_reconnect_requested`.

/// POD mirror of [`crate::session::IncidentSnapshot`] for C. `_present`/
/// `_open` fields are 1/0, not C `bool`, for FFI-width stability (see
/// [`CtDecodeUnit::is_key`]).
#[repr(C)]
pub struct CtIncidentSnapshot {
    pub generation: u64,
    pub last_frame_id: u32,
    pub last_frame_id_present: u8,
    pub active_epoch: u32,
    pub active_epoch_present: u8,
    pub source_symbols_received: u64,
    pub repair_symbols_received: u64,
    pub symbols_recovered: u64,
    pub frames_recovered: u64,
    pub frames_dropped_awaiting_idr: u64,
    pub loss_spans: u64,
    pub loss_spans_recovered: u64,
    pub queue_len: u32,
    pub last_ack: u32,
    pub last_ack_present: u8,
    pub last_idr_attempt: u32,
    pub receive_age_ms: u64,
    pub decode_age_ms: u64,
    pub present_age_ms: u64,
    pub audio_age_ms: u64,
    pub recovery_open: u8,
    pub recovery_generation: u64,
    pub recovery_epoch: u32,
}

impl From<crate::session::IncidentSnapshot> for CtIncidentSnapshot {
    fn from(s: crate::session::IncidentSnapshot) -> Self {
        Self {
            generation: s.generation,
            last_frame_id: s.last_frame_id,
            last_frame_id_present: u8::from(s.last_frame_id_present),
            active_epoch: s.active_epoch,
            active_epoch_present: u8::from(s.active_epoch_present),
            source_symbols_received: s.source_symbols_received,
            repair_symbols_received: s.repair_symbols_received,
            symbols_recovered: s.symbols_recovered,
            frames_recovered: s.frames_recovered,
            frames_dropped_awaiting_idr: s.frames_dropped_awaiting_idr,
            loss_spans: s.loss_spans,
            loss_spans_recovered: s.loss_spans_recovered,
            queue_len: s.queue_len,
            last_ack: s.last_ack,
            last_ack_present: u8::from(s.last_ack_present),
            last_idr_attempt: s.last_idr_attempt,
            receive_age_ms: s.receive_age_ms,
            decode_age_ms: s.decode_age_ms,
            present_age_ms: s.present_age_ms,
            audio_age_ms: s.audio_age_ms,
            recovery_open: u8::from(s.recovery_open),
            recovery_generation: s.recovery_generation,
            recovery_epoch: s.recovery_epoch,
        }
    }
}

/// Poll-and-clear a pending typed decode/present-stall action. Returns 1
/// and writes `*out_kind` (5=DecodeStall, 6=PresentStall) and
/// `*out_attempt` (1-based) when one is pending, else 0 (including NULL
/// input). Native maps kind 5 to decoder flush + IDR, kind 6 to its own
/// bounded device-recreate/R8-fallback ladder — a final `Reconnect` past
/// that ladder surfaces as a plain [`ct_session_reconnect_requested`], not
/// through this poll.
///
/// # Safety
/// `s` must be NULL or a live pointer from [`ct_start`]; non-null output
/// pointers must be valid for one `i32` and one `u32` write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_session_poll_watchdog_action(
    s: *const CtSession,
    out_kind: *mut i32,
    out_attempt: *mut u32,
) -> i32 {
    let Some(s) = (unsafe { s.as_ref() }) else {
        return 0;
    };
    let Some(session) = &s.session else {
        return 0;
    };
    if out_kind.is_null() || out_attempt.is_null() {
        return 0;
    }
    let watchdog = session.watchdog();
    let (kind, attempt) = if let Some(attempt) = watchdog.poll_decode_stall() {
        (5, attempt)
    } else if let Some(attempt) = watchdog.poll_present_stall() {
        (6, attempt)
    } else {
        return 0;
    };
    unsafe {
        *out_kind = kind;
        *out_attempt = attempt;
    }
    1
}

/// Receive-stage stall indicator (UI readback). 1/0, or 0 on NULL/no
/// session.
///
/// # Safety
/// `s` must be NULL or a live pointer from [`ct_start`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_session_stalled(s: *const CtSession) -> i32 {
    let Some(s) = (unsafe { s.as_ref() }) else {
        return 0;
    };
    match &s.session {
        Some(session) => i32::from(session.watchdog().stalled()),
        None => 0,
    }
}

/// Any stage's ladder exhausted into the terminal reconnect rung; the
/// session thread has ended (state `Failed`) and the shell should
/// rebuild. 1/0, or 0 on NULL/no session.
///
/// # Safety
/// `s` must be NULL or a live pointer from [`ct_start`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_session_reconnect_requested(s: *const CtSession) -> i32 {
    let Some(s) = (unsafe { s.as_ref() }) else {
        return 0;
    };
    match &s.session {
        Some(session) => i32::from(session.watchdog().reconnect_requested()),
        None => 0,
    }
}

/// Hold/resume watchdog escalation across all three G004 stages (window
/// minimized/hidden). Idempotent; picked up on the next 50 ms session
/// tick. NULL/no session is a no-op.
///
/// # Safety
/// `s` must be NULL or a live pointer from [`ct_start`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_session_set_watchdog_paused(s: *const CtSession, paused: i32) {
    let Some(s) = (unsafe { s.as_ref() }) else {
        return;
    };
    if let Some(session) = &s.session {
        session.set_watchdog_paused(paused != 0);
    }
}

/// Pollable G004 incident snapshot. Returns 1 and fills `*out` when at
/// least one watchdog stage has acted since `ct_start`, else 0 (including
/// NULL input). Non-consuming — safe to poll repeatedly (e.g. from a
/// telemetry timer) without racing the event that produced it.
///
/// # Safety
/// `s` must be NULL or a live pointer from [`ct_start`]; `out` must be a
/// valid writable [`CtIncidentSnapshot`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_session_poll_incident(
    s: *const CtSession,
    out: *mut CtIncidentSnapshot,
) -> i32 {
    let Some(s) = (unsafe { s.as_ref() }) else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let Some(session) = &s.session else {
        return 0;
    };
    match session.watchdog().latest_incident() {
        Some(snapshot) => {
            unsafe {
                *out = snapshot.into();
            }
            1
        }
        None => 0,
    }
}

// ── G002 decoder-recovery ABI (decoder thread) ────────────────────────────

/// Query the decoder-recovery handshake currently open, if any: the
/// generation/epoch pair from the discontinuity or overflow event that
/// opened it. Returns 1 and writes `*out_generation`/`*out_epoch` when a
/// handshake is open, 0 when none is open (including NULL input) — the
/// return code, not the written values, is what "open" means: a
/// legitimately open recovery can carry generation/epoch `0`, so callers
/// must branch on the return value, never on the out-params alone.
///
/// `generation` is a monotonic identity assigned fresh to every opened
/// recovery (see [`ct_event_discontinuity`]) — the C consumer must retain
/// the `(generation, epoch)` pair from the most recently popped
/// [`CT_EVENT_DISCONTINUITY`] event and pass that same pair to
/// [`ct_receiver_acknowledge_decoded_key`] once it decodes a fresh key;
/// `epoch` alone is not a safe ack key because most discontinuity reasons
/// do not change it (two resets can share one epoch, and only `generation`
/// tells them apart).
///
/// # Safety
/// `p` must be NULL or a live receiver pointer from [`ct_receiver_new`];
/// non-null output pointers must be valid for one `u64` and one `u32`
/// write respectively.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_recovery_epoch(
    p: *const CtReceiver,
    out_generation: *mut u64,
    out_epoch: *mut u32,
) -> i32 {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return 0;
    };
    if out_generation.is_null() || out_epoch.is_null() {
        return 0;
    }
    match r.core.recovery() {
        Some((generation, epoch)) => {
            unsafe {
                *out_generation = generation;
                *out_epoch = epoch;
            }
            1
        }
        None => 0,
    }
}

/// Close decoder-recovery for `(generation, epoch)` after the decoder
/// flushed and successfully decoded a key frame at that epoch — pass the
/// pair retained from the gating [`ct_event_discontinuity`] (see
/// [`ct_receiver_recovery_epoch`]'s doc), not just the epoch. Returns 1 when
/// the pair matched the currently open recovery and closed it, else 0
/// (stale generation, wrong epoch, or no recovery open — all no-ops besides
/// the return value).
///
/// `frame_id` is diagnostic-only and unvalidated by the implementation: the
/// `(generation, epoch)` pair alone determines whether this ack closes
/// recovery.
///
/// # Safety
/// `p` must be NULL or a live receiver pointer from [`ct_receiver_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_acknowledge_decoded_key(
    p: *const CtReceiver,
    generation: u64,
    epoch: u32,
    frame_id: u32,
) -> i32 {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return 0;
    };
    i32::from(r.core.acknowledge_decoded_key(generation, epoch, frame_id))
}

// ── Ordered event pull loop (decoder thread) ──────────────────────────────

/// Opaque queued event (`CtVideoEvent*` in C): a frame or a discontinuity,
/// in the same wire order they were produced. Superset of [`CtFrame`] for
/// decoder-recovery aware consumers; see [`ct_event_kind`].
pub struct CtVideoEvent(VideoEvent);

/// Event kind returned by [`ct_event_kind`]: a reassembled frame.
pub const CT_EVENT_FRAME: i32 = 0;
/// Event kind returned by [`ct_event_kind`]: a discontinuity/reset.
pub const CT_EVENT_DISCONTINUITY: i32 = 1;

/// Block up to `timeout_ms` for the next queued event (frame or
/// discontinuity). Returns an owned `CtVideoEvent*` (release with
/// [`ct_event_free`]) or NULL on timeout / after [`ct_receiver_close`].
///
/// # Safety
/// `p` as in [`ct_receiver_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_wait_event(
    p: *mut CtReceiver,
    timeout_ms: u64,
) -> *mut CtVideoEvent {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return std::ptr::null_mut();
    };
    match r.core.wait_event(Duration::from_millis(timeout_ms)) {
        Some(ev) => Box::into_raw(Box::new(CtVideoEvent(ev))),
        None => std::ptr::null_mut(),
    }
}

/// Non-blocking pop of the next queued event, if one is already queued.
/// Returns an owned `CtVideoEvent*` (release with [`ct_event_free`]) or NULL.
///
/// # Safety
/// `p` as in [`ct_receiver_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_receiver_try_event(p: *mut CtReceiver) -> *mut CtVideoEvent {
    let Some(r) = (unsafe { p.as_ref() }) else {
        return std::ptr::null_mut();
    };
    match r.core.try_event() {
        Some(ev) => Box::into_raw(Box::new(CtVideoEvent(ev))),
        None => std::ptr::null_mut(),
    }
}

/// Kind of a queued event: [`CT_EVENT_FRAME`] or [`CT_EVENT_DISCONTINUITY`],
/// or -1 on NULL input.
///
/// # Safety
/// `e` must be NULL or a live pointer from [`ct_receiver_wait_event`] /
/// [`ct_receiver_try_event`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_event_kind(e: *const CtVideoEvent) -> i32 {
    let Some(e) = (unsafe { e.as_ref() }) else {
        return -1;
    };
    match e.0 {
        VideoEvent::Frame(_) => CT_EVENT_FRAME,
        VideoEvent::Discontinuity { .. } => CT_EVENT_DISCONTINUITY,
    }
}

/// Fill `*out` with a view of the frame carried by a [`CT_EVENT_FRAME`]
/// event. The `data` pointer stays valid until [`ct_event_free`]. Returns 1
/// on success, 0 when `e`/`out` are NULL or the event is not a frame.
///
/// # Safety
/// `e` must be NULL or a live pointer from [`ct_receiver_wait_event`] /
/// [`ct_receiver_try_event`]; `out` must be a valid writable
/// [`CtDecodeUnit`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_event_view(e: *const CtVideoEvent, out: *mut CtDecodeUnit) -> i32 {
    let Some(e) = (unsafe { e.as_ref() }) else {
        return 0;
    };
    if out.is_null() {
        return 0;
    }
    let VideoEvent::Frame(u) = &e.0 else {
        return 0;
    };
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

/// Fill the recovery generation, FEC epoch, and typed reason of a
/// [`CT_EVENT_DISCONTINUITY`] event. `*out_generation` is the identity to
/// retain and later pass to [`ct_receiver_acknowledge_decoded_key`] — see
/// [`ct_receiver_recovery_epoch`]'s doc for why `epoch` alone is not a safe
/// ack key. Reason codes match [`ct_receiver_poll_discontinuity`]; 8 is
/// [`DiscontinuityReason::QueueOverflow`] (decoder-side frame-queue reset —
/// never produced by the receiver itself). Returns 1 on success, 0 when
/// `e`/output pointers are NULL or the event is not a discontinuity.
///
/// # Safety
/// `e` must be NULL or a live pointer from [`ct_receiver_wait_event`] /
/// [`ct_receiver_try_event`]; non-null output pointers must be valid for one
/// `u64`, one `u32`, and one `u8` write respectively.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_event_discontinuity(
    e: *const CtVideoEvent,
    out_generation: *mut u64,
    out_epoch: *mut u32,
    out_reason: *mut u8,
) -> i32 {
    let Some(e) = (unsafe { e.as_ref() }) else {
        return 0;
    };
    if out_generation.is_null() || out_epoch.is_null() || out_reason.is_null() {
        return 0;
    }
    let VideoEvent::Discontinuity {
        generation,
        epoch,
        reason,
    } = e.0
    else {
        return 0;
    };
    unsafe {
        *out_generation = generation;
        *out_epoch = epoch;
        *out_reason = discontinuity_reason_code(reason);
    }
    1
}

/// Release an event from [`ct_receiver_wait_event`] / [`ct_receiver_try_event`].
/// NULL is ignored.
///
/// # Safety
/// `e` must be NULL or an owned pointer from those functions; any view of it
/// is dangling afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ct_event_free(e: *mut CtVideoEvent) {
    if !e.is_null() {
        drop(unsafe { Box::from_raw(e) });
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn discontinuity_reason_code(reason: DiscontinuityReason) -> u8 {
    match reason {
        DiscontinuityReason::EpochTransition => 1,
        DiscontinuityReason::ReorderGap => 2,
        DiscontinuityReason::ReassemblyEviction => 3,
        DiscontinuityReason::MetadataMismatch => 4,
        DiscontinuityReason::FrameCrc => 5,
        DiscontinuityReason::MemoryCap => 6,
        DiscontinuityReason::FecEviction => 7,
        DiscontinuityReason::QueueOverflow => 8,
    }
}
fn lock_ignore_poison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ── Tests (exercise the ABI exactly as the C++ shim will) ─────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::sync::{Arc, Barrier};
    use transport_core::fec::Symbol;
    use transport_core::fec_wire::{
        V2Symbol, chunk_frame, chunk_frame_v2, encode_symbol_msg, encode_symbol_msg_v2,
    };

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
    fn start_with_test_config(rx: *mut CtReceiver) -> *mut CtSession {
        let base_url = CString::new("http://127.0.0.1:1").expect("static URL has no NUL");
        let username = CString::new("user").expect("static username has no NUL");
        let password = CString::new("password").expect("static password has no NUL");
        let cfg = CtSessionConfig {
            base_url: base_url.as_ptr(),
            username: username.as_ptr(),
            password: password.as_ptr(),
            host_id: 1,
            app_id: 1,
            bitrate_kbps: 1_000,
            width: 640,
            height: 480,
            fps: 60,
            insecure_tls: 1,
            cert_sha256: std::ptr::null(),
        };
        unsafe { ct_start(&cfg, rx) }
    }

    #[test]
    fn selected_v2_uses_epoch_controls_and_rejects_other_wire_epochs() {
        let core = RxCore::new(0);
        let epoch = NonZeroU32::new(7).expect("test epoch must be non-zero");
        assert!(core.configure_fec(2, Some(epoch)));
        assert_eq!(
            core.control_message(FecControl::Subscribe),
            Some(vec![0x82, 7, 0, 0, 0])
        );
        assert_eq!(
            core.control_message(FecControl::Ack(99)),
            Some(vec![0x81, 7, 0, 0, 0, 99, 0, 0, 0])
        );
        assert_eq!(
            core.control_message(FecControl::NeedsIdr { reason: 1 }),
            Some(vec![0x80, 7, 0, 0, 0, 1])
        );

        let chunk = chunk_frame_v2(1, true, 10, &[1, 2, 3])
            .expect("test frame must fit a v2 chunk")
            .remove(0);
        let old_epoch = encode_symbol_msg_v2(&V2Symbol::Source {
            epoch: Epoch::new(6).expect("test old epoch must be valid"),
            seq: 0,
            payload: chunk.clone(),
        })
        .expect("test old-epoch source symbol must encode");
        let selected_epoch = encode_symbol_msg_v2(&V2Symbol::Source {
            epoch: Epoch::new(7).expect("test selected epoch must be valid"),
            seq: 0,
            payload: chunk,
        })
        .expect("test selected-epoch source symbol must encode");
        core.on_message(&old_epoch, 0);
        assert!(core.try_frame().is_none(), "old epoch must be dropped");
        core.on_message(&selected_epoch, 0);
        assert!(core.try_frame().is_some(), "selected epoch is admitted");

        // A selected v2 codec must not reinterpret malformed v2 as legacy v1.
        core.on_message(&[0x02, 7, 0, 0, 0], 0);
        assert!(core.try_frame().is_none());
    }
    #[test]
    fn fresh_negotiation_clears_stale_receiver_controls_and_frames() {
        let core = RxCore::new(0);
        let mut seq = 0;
        let message = source_msgs_for_frame(0, true, 0, &[1], &mut seq).remove(0);
        core.on_message(&message, 0);
        assert!(core.try_frame().is_some());
        *lock_ignore_poison(&core.pending_ack) = Some(42);
        core.request_idr();

        core.begin_fec_negotiation();
        assert!(core.try_frame().is_none());
        assert_eq!(core.poll_ack(), None);
        assert!(!core.poll_needs_idr());
    }
    #[test]
    fn session_lease_is_exclusive_and_never_reusable() {
        let core = RxCore::new(0);
        let generation = core.acquire_session_lease().expect("first lease");
        core.release_session_lease(generation)
            .expect("current lease releases");
        assert!(
            core.acquire_session_lease().is_err(),
            "closed queues make a receiver a one-session allocation"
        );
    }
    #[test]
    fn fresh_negotiation_consumes_pre_session_queue_overflow() {
        let core = RxCore::new(0);
        let mut seq = 0;
        for frame_id in 0..17 {
            let message =
                source_msgs_for_frame(frame_id, false, frame_id, &[1], &mut seq).remove(0);
            core.on_message(&message, 0);
        }

        core.begin_fec_negotiation();

        assert!(
            !core.poll_needs_idr(),
            "the new generation must not inherit an unpolled queue overflow"
        );
    }

    #[test]
    fn concurrent_ct_start_returns_null_to_the_losing_caller() {
        let rx = Rx::new(0);
        let receiver = rx.p() as usize;
        let barrier = Arc::new(Barrier::new(3));
        let mut starters = Vec::new();
        for _ in 0..2 {
            let barrier = barrier.clone();
            starters.push(std::thread::spawn(move || {
                barrier.wait();
                start_with_test_config(receiver as *mut CtReceiver) as usize
            }));
        }
        barrier.wait();

        let sessions: Vec<_> = starters
            .into_iter()
            .map(|starter| starter.join().expect("starter must not panic"))
            .collect();
        assert_eq!(
            sessions.iter().filter(|&&session| session != 0).count(),
            1,
            "exactly one concurrent caller owns the C session"
        );
        let winner = *sessions
            .iter()
            .find(|&&session| session != 0)
            .expect("one caller won the lease");
        unsafe { ct_stop(winner as *mut CtSession) };
    }

    #[test]
    fn create_close_then_start_returns_null() {
        let rx = Rx::new(0);
        unsafe { ct_receiver_close(rx.p()) };
        assert!(
            start_with_test_config(rx.p()).is_null(),
            "closed queues cannot create a session"
        );
    }

    #[test]
    fn leased_receiver_rejects_standalone_ingress_and_control_polls() {
        let rx = Rx::new(0);
        let core = unsafe { &(*rx.p()).core };
        let generation = core.acquire_session_lease().expect("lease");

        let mut seq = 0;
        let message = source_msgs_for_frame(0, true, 0, &[1], &mut seq).remove(0);
        feed(&rx, &message, 0);
        assert!(
            core.try_frame().is_none(),
            "external feed cannot steal a lease"
        );

        *lock_ignore_poison(&core.pending_ack) = Some(99);
        core.decode_needs_idr.store(true, Ordering::Release);
        let mut ack = 0;
        assert_eq!(unsafe { ct_receiver_poll_ack(rx.p(), &raw mut ack) }, 0);
        assert_eq!(unsafe { ct_receiver_poll_needs_idr(rx.p()) }, 0);
        core.release_session_lease(generation)
            .expect("current lease releases");
    }

    #[test]
    fn selected_v2_generation_requires_a_keyframe_before_delivery() {
        let core = RxCore::new(0);
        let epoch = Epoch::new(7).expect("test epoch must be valid");
        core.begin_fec_negotiation();
        assert!(core.configure_fec(2, NonZeroU32::new(epoch.get())));

        let delta = chunk_frame_v2(1, false, 1, &[1])
            .expect("test frame must fit")
            .remove(0);
        let delta = encode_symbol_msg_v2(&V2Symbol::Source {
            epoch,
            seq: 0,
            payload: delta,
        })
        .expect("test symbol must encode");
        core.on_message(&delta, 0);
        assert!(
            core.try_frame().is_none(),
            "first selected-epoch delta drops"
        );

        let key = chunk_frame_v2(2, true, 2, &[2])
            .expect("test frame must fit")
            .remove(0);
        let key = encode_symbol_msg_v2(&V2Symbol::Source {
            epoch,
            seq: 1,
            payload: key,
        })
        .expect("test symbol must encode");
        core.on_message(&key, 0);
        assert!(core.try_frame().is_some(), "selected-epoch key admits");
    }

    #[test]
    fn begin_negotiation_blocks_bytes_until_a_codec_is_selected() {
        let core = RxCore::new(0);
        core.begin_fec_negotiation();
        let mut seq = 0;
        let message = source_msgs_for_frame(1, true, 1, &[1], &mut seq).remove(0);
        core.on_message(&message, 0);
        assert!(core.try_frame().is_none());
        assert!(core.configure_fec(1, None));
        core.on_message(&message, 0);
        assert!(core.try_frame().is_some());
    }
    #[test]
    fn tick_drains_reorder_discontinuity_into_needs_idr() {
        let core = RxCore::new(0);
        let mut seq = 0;
        for frame_id in [0, 2] {
            let message =
                source_msgs_for_frame(frame_id, frame_id == 0, frame_id, &[1], &mut seq).remove(0);
            core.on_message(&message, 0);
        }
        assert!(!core.poll_needs_idr(), "gap has not expired yet");
        core.tick(100);
        assert!(
            core.poll_needs_idr(),
            "tick_events must drain expired reorder gaps into the IDR latch"
        );
        assert!(matches!(
            core.take_discontinuity(),
            Some(RxEvent::Discontinuity { .. })
        ));
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
    #[test]
    fn discontinuity_enters_queue_before_later_frames_and_opens_recovery() {
        let core = RxCore::new(0);
        let mut seq = 0;

        let f0 = source_msgs_for_frame(0, true, 0, &[1], &mut seq).remove(0);
        core.on_message(&f0, 0);

        // frame_id 2 arrives while 1 is missing: reorder-gap expiry (tick)
        // fires a Discontinuity for this epoch.
        let f2 = source_msgs_for_frame(2, false, 2, &[1], &mut seq).remove(0);
        core.on_message(&f2, 0);
        assert!(core.recovery().is_none(), "no reset queued yet");
        core.tick(100);
        assert!(
            core.recovery().is_some(),
            "reorder-gap discontinuity must open decoder-recovery"
        );

        // Only a keyframe is admitted post-reset; deliver one so a later
        // frame also lands in the queue.
        let f3 = source_msgs_for_frame(3, true, 3, &[9], &mut seq).remove(0);
        core.on_message(&f3, 0);

        assert_eq!(
            core.try_event()
                .and_then(|e| e.as_frame().map(|u| u.frame_id).or(Some(u32::MAX))),
            Some(0),
            "frame 0 admitted before the reset"
        );
        let generation = match core.try_event() {
            Some(VideoEvent::Discontinuity {
                generation,
                epoch,
                reason,
            }) => {
                assert_eq!(epoch, 0);
                assert_eq!(
                    reason,
                    transport_core::video_rx::DiscontinuityReason::ReorderGap
                );
                generation
            }
            other => panic!("expected the reorder-gap reset, got {other:?}"),
        };
        assert_eq!(
            core.try_event()
                .and_then(|e| e.as_frame().map(|u| u.frame_id)),
            Some(3),
            "frame 3 must be visible only after its reset event"
        );
        assert!(core.try_event().is_none());

        // Stale/wrong acks are no-ops; only the matching generation+epoch
        // closes recovery.
        assert_eq!(core.recovery(), Some((generation, 0)));
        assert!(
            !core.acknowledge_decoded_key(generation, 1, 3),
            "wrong epoch rejected"
        );
        assert!(
            !core.acknowledge_decoded_key(generation.wrapping_add(1), 0, 3),
            "wrong generation rejected"
        );
        assert_eq!(
            core.recovery(),
            Some((generation, 0)),
            "wrong-epoch/wrong-generation acks must not close it"
        );
        assert!(
            core.acknowledge_decoded_key(generation, 0, 3),
            "matching generation+epoch closes recovery"
        );
        assert_eq!(core.recovery(), None);
        assert!(
            !core.acknowledge_decoded_key(generation, 0, 3),
            "stale ack (already closed) is a no-op"
        );
    }

    #[test]
    fn queue_overflow_discontinuity_precedes_retained_frame_and_opens_recovery() {
        let core = RxCore::new(0);
        let mut seq = 0;
        // Overflow the 16-cap frame queue without popping: 17 frames.
        for i in 0..17u32 {
            for msg in source_msgs_for_frame(i, false, i * 1000, &[i as u8], &mut seq) {
                feed_message(&core, &msg, 0);
            }
        }
        let (generation, epoch) = core
            .recovery()
            .expect("queue overflow must open decoder-recovery for the triggering frame's epoch");
        assert_eq!(epoch, 0);
        match core.try_event() {
            Some(VideoEvent::Discontinuity {
                generation: ev_generation,
                epoch,
                reason,
            }) => {
                assert_eq!(ev_generation, generation);
                assert_eq!(epoch, 0);
                assert_eq!(
                    reason,
                    transport_core::video_rx::DiscontinuityReason::QueueOverflow
                );
                assert_eq!(discontinuity_reason_code(reason), 8, "stable C ABI code");
            }
            other => panic!("expected the overflow reset first, got {other:?}"),
        }
        assert_eq!(
            core.try_event()
                .and_then(|e| e.as_frame().map(|u| u.frame_id)),
            Some(16),
            "only the newest frame is retained, after its reset event"
        );
        assert!(core.try_event().is_none());
        assert!(core.acknowledge_decoded_key(generation, 0, 16));
        assert_eq!(core.recovery(), None);
    }

    fn feed_message(core: &RxCore, msg: &[u8], now_ms: u64) {
        core.on_message(msg, now_ms);
    }

    #[test]
    fn same_epoch_double_discontinuity_generation_prevents_stale_ack_aliasing() {
        // Two separate queue-overflow bursts on the same (never-changing,
        // for v1) epoch must open two distinct recovery generations — an
        // ack keyed by the first must never close the second's recovery,
        // exactly the aliasing agent://87-CleanG002Queue flagged.
        let core = RxCore::new(0);
        let mut seq = 0;

        for i in 0..17u32 {
            for msg in source_msgs_for_frame(i, false, i * 1000, &[i as u8], &mut seq) {
                feed_message(&core, &msg, 0);
            }
        }
        let (gen1, epoch1) = core.recovery().expect("first overflow opens recovery");
        assert_eq!(epoch1, 0);

        // Drain fully so the second burst overflows cleanly.
        while core.try_event().is_some() {}

        for i in 17..34u32 {
            for msg in source_msgs_for_frame(i, false, i * 1000, &[i as u8], &mut seq) {
                feed_message(&core, &msg, 0);
            }
        }
        let (gen2, epoch2) = core.recovery().expect("second overflow opens recovery");
        assert_eq!(
            epoch2, 0,
            "same epoch as the first burst — v1 never bumps it"
        );
        assert_ne!(gen1, gen2, "each opened recovery gets a fresh generation");

        assert!(
            !core.acknowledge_decoded_key(gen1, 0, 999),
            "stale generation must not close a later same-epoch recovery"
        );
        assert_eq!(
            core.recovery(),
            Some((gen2, 0)),
            "recovery must still be open under the current generation"
        );
        assert!(
            core.acknowledge_decoded_key(gen2, 0, 999),
            "matching generation closes recovery"
        );
        assert_eq!(core.recovery(), None);
    }

    #[test]
    fn fresh_negotiation_clears_open_recovery() {
        let core = RxCore::new(0);
        let mut seq = 0;
        for i in 0..17u32 {
            for msg in source_msgs_for_frame(i, false, i * 1000, &[i as u8], &mut seq) {
                feed_message(&core, &msg, 0);
            }
        }
        assert!(core.recovery().is_some());
        core.begin_fec_negotiation();
        assert_eq!(
            core.recovery(),
            None,
            "a fresh generation must not inherit stale recovery state"
        );
    }

    #[test]
    fn c_abi_event_pull_and_decoded_key_ack() {
        let rx = Rx::new(0);
        let mut seq = 0;
        for i in 0..17u32 {
            for msg in source_msgs_for_frame(i, false, i * 1000, &[i as u8], &mut seq) {
                feed(&rx, &msg, 0);
            }
        }

        let ev = unsafe { ct_receiver_wait_event(rx.p(), 10) };
        assert!(!ev.is_null());
        assert_eq!(unsafe { ct_event_kind(ev) }, CT_EVENT_DISCONTINUITY);
        let mut generation = 0u64;
        let mut epoch = 0u32;
        let mut reason = 0u8;
        assert_eq!(
            unsafe {
                ct_event_discontinuity(ev, &raw mut generation, &raw mut epoch, &raw mut reason)
            },
            1
        );
        assert_eq!(epoch, 0);
        assert_eq!(reason, 8, "QueueOverflow stable code");
        // Not a frame — ct_event_view must reject it.
        let mut view = CtDecodeUnit {
            frame_id: 0,
            is_key: 0,
            timestamp_us: 0,
            duration_us: 0,
            data: std::ptr::null(),
            data_len: 0,
        };
        assert_eq!(unsafe { ct_event_view(ev, &raw mut view) }, 0);
        unsafe { ct_event_free(ev) };

        let ev = unsafe { ct_receiver_try_event(rx.p()) };
        assert!(!ev.is_null());
        assert_eq!(unsafe { ct_event_kind(ev) }, CT_EVENT_FRAME);
        assert_eq!(unsafe { ct_event_view(ev, &raw mut view) }, 1);
        assert_eq!(view.frame_id, 16);
        // Not a discontinuity — ct_event_discontinuity must reject it.
        assert_eq!(
            unsafe {
                ct_event_discontinuity(ev, &raw mut generation, &raw mut epoch, &raw mut reason)
            },
            0
        );
        unsafe { ct_event_free(ev) };

        let mut recovery_generation = 999u64;
        let mut recovery_epoch = 999u32;
        assert_eq!(
            unsafe {
                ct_receiver_recovery_epoch(
                    rx.p(),
                    &raw mut recovery_generation,
                    &raw mut recovery_epoch,
                )
            },
            1
        );
        assert_eq!(
            recovery_generation, generation,
            "matches the popped event's generation"
        );
        assert_eq!(recovery_epoch, 0);
        assert_eq!(
            unsafe { ct_receiver_acknowledge_decoded_key(rx.p(), generation, 1, 16) },
            0,
            "wrong epoch rejected"
        );
        assert_eq!(
            unsafe {
                ct_receiver_acknowledge_decoded_key(rx.p(), generation.wrapping_add(1), 0, 16)
            },
            0,
            "wrong generation rejected"
        );
        assert_eq!(
            unsafe { ct_receiver_acknowledge_decoded_key(rx.p(), generation, 0, 16) },
            1,
            "matching generation+epoch closes recovery"
        );
        assert_eq!(
            unsafe {
                ct_receiver_recovery_epoch(
                    rx.p(),
                    &raw mut recovery_generation,
                    &raw mut recovery_epoch,
                )
            },
            0,
            "recovery closed"
        );
        assert_eq!(
            unsafe { ct_receiver_acknowledge_decoded_key(rx.p(), generation, 0, 16) },
            0,
            "stale ack rejected"
        );
    }

    #[test]
    fn c_abi_event_pull_null_inputs_are_safe() {
        unsafe {
            assert!(ct_receiver_wait_event(std::ptr::null_mut(), 0).is_null());
            assert!(ct_receiver_try_event(std::ptr::null_mut()).is_null());
            assert_eq!(ct_event_kind(std::ptr::null()), -1);
            assert_eq!(ct_event_view(std::ptr::null(), std::ptr::null_mut()), 0);
            assert_eq!(
                ct_event_discontinuity(
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                0
            );
            ct_event_free(std::ptr::null_mut());
            assert_eq!(
                ct_receiver_recovery_epoch(
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut()
                ),
                0
            );
            assert_eq!(
                ct_receiver_acknowledge_decoded_key(std::ptr::null(), 0, 0, 0),
                0
            );
        }
    }

    #[test]
    fn session_generation_value_reflects_the_lease_and_is_stable_across_uses() {
        let core = RxCore::new(0);
        let generation = core.acquire_session_lease().expect("first lease");
        assert_eq!(
            generation.value(),
            1,
            "first lease on a fresh receiver is generation 1"
        );
        core.release_session_lease(generation)
            .expect("current lease releases");
        // A receiver is a one-shot lease (see
        // `session_lease_is_exclusive_and_never_reusable` above): acquiring
        // again after release is rejected, so the value contract is only
        // pinned for the single lease a receiver ever grants.
        assert!(core.acquire_session_lease().is_err());
    }

    #[test]
    fn ct_incident_snapshot_round_trips_every_field_from_incident_snapshot() {
        // Pin the POD-mirror `From` conversion field-for-field so a future
        // field addition/reorder on either side (session.rs's
        // `IncidentSnapshot` or this file's `CtIncidentSnapshot`) is caught
        // here instead of silently dropping/misaligning a value across the
        // C ABI boundary.
        let snapshot = crate::session::IncidentSnapshot {
            generation: 7,
            last_frame_id: 11,
            last_frame_id_present: true,
            active_epoch: 3,
            active_epoch_present: true,
            source_symbols_received: 100,
            repair_symbols_received: 20,
            symbols_recovered: 5,
            frames_recovered: 2,
            frames_dropped_awaiting_idr: 1,
            loss_spans: 4,
            loss_spans_recovered: 3,
            queue_len: 9,
            last_ack: 42,
            last_ack_present: true,
            last_idr_attempt: 6,
            receive_age_ms: 1_000,
            decode_age_ms: 2_000,
            present_age_ms: 3_000,
            audio_age_ms: 4_000,
            recovery_open: true,
            recovery_generation: 8,
            recovery_epoch: 12,
        };
        let ct: CtIncidentSnapshot = snapshot.into();
        assert_eq!(ct.generation, 7);
        assert_eq!(ct.last_frame_id, 11);
        assert_eq!(ct.last_frame_id_present, 1);
        assert_eq!(ct.active_epoch, 3);
        assert_eq!(ct.active_epoch_present, 1);
        assert_eq!(ct.source_symbols_received, 100);
        assert_eq!(ct.repair_symbols_received, 20);
        assert_eq!(ct.symbols_recovered, 5);
        assert_eq!(ct.frames_recovered, 2);
        assert_eq!(ct.frames_dropped_awaiting_idr, 1);
        assert_eq!(ct.loss_spans, 4);
        assert_eq!(ct.loss_spans_recovered, 3);
        assert_eq!(ct.queue_len, 9);
        assert_eq!(ct.last_ack, 42);
        assert_eq!(ct.last_ack_present, 1);
        assert_eq!(ct.last_idr_attempt, 6);
        assert_eq!(ct.receive_age_ms, 1_000);
        assert_eq!(ct.decode_age_ms, 2_000);
        assert_eq!(ct.present_age_ms, 3_000);
        assert_eq!(ct.audio_age_ms, 4_000);
        assert_eq!(ct.recovery_open, 1);
        assert_eq!(ct.recovery_generation, 8);
        assert_eq!(ct.recovery_epoch, 12);
    }
}
