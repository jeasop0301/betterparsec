//! Audio — A0 slice 4 (design D7 Phase A,
//! docs/design/unified-app-architecture.md §4-3).
//!
//! Opus decode through libavcodec's built-in decoder (same FFmpeg pin as
//! video — no new native dependency) and WASAPI **shared**-mode render
//! with a plain polling fill loop. An opt-in exclusive-mode, event-driven
//! sink (`BP_AUDIO_EXCLUSIVE=1`, spike §C-3) is available alongside it —
//! [`run`] falls back to the shared-mode sink on any exclusive-mode init
//! failure, so the default remains unchanged. The decode half is shared
//! by both sinks.
//!
//! The host sends opus 48 kHz stereo over an RTP track (RFC 7587, one
//! packet per payload); `client-transport` queues raw packets in
//! [`RxCore`]'s sample queue and [`run`] drains it on the `a0-audio`
//! thread: wait → decode → convert to the device mix format → fill.

use std::collections::VecDeque;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use client_transport::capi::RxCore;
use ffmpeg_sys_next as ff;

/// Audio failure. Fatal to audio only — the session and video continue.
#[derive(Debug)]
pub struct AudioError(pub String);

impl std::fmt::Display for AudioError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn err_str(rc: i32) -> String {
    let mut buf = [0u8; 128];
    let ok = unsafe { ff::av_strerror(rc, buf.as_mut_ptr().cast(), buf.len()) } >= 0;
    if !ok {
        return format!("ffmpeg error {rc}");
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    format!("{} ({rc})", String::from_utf8_lossy(&buf[..end]))
}

// ── Shared state (audio thread → UI) ──────────────────────────────────────

/// `AudioShared::state` values; 0 (default) = starting.
pub const AUDIO_RUNNING: u8 = 1;
pub const AUDIO_FAILED: u8 = 2;

#[derive(Default)]
pub struct AudioShared {
    pub packets: AtomicU64,
    pub decode_errors: AtomicU64,
    /// 0 = starting, [`AUDIO_RUNNING`], [`AUDIO_FAILED`].
    pub state: AtomicU8,
    /// Opt-in WASAPI exclusive render (low latency). Seeded before the
    /// audio thread starts (env `BP_AUDIO_EXCLUSIVE` or the in-app toggle,
    /// which takes effect on the next connect — the device is owned for
    /// the session's lifetime so it cannot flip live). Read once at the
    /// top of [`run`].
    pub exclusive: AtomicBool,
}

// ── Opus decode (libavcodec built-in) ─────────────────────────────────────

/// Decodes RFC 7587 opus packets to interleaved f32 stereo @ 48 kHz.
pub struct OpusDecoder {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
}

// SAFETY: pointers are exclusively owned; the audio thread is the only
// user (same argument as video::Decoder).
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn new() -> Result<Self, AudioError> {
        unsafe {
            let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_OPUS);
            if codec.is_null() {
                return Err(AudioError("no opus decoder in libavcodec".into()));
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(AudioError("avcodec_alloc_context3 failed".into()));
            }
            let mut d = Self {
                ctx,
                frame: ptr::null_mut(),
                pkt: ptr::null_mut(),
            };
            // RTP opus is always decoded at 48 kHz; TOC switches frame
            // sizes internally. The track is stereo (RFC 7587 §4.2).
            (*ctx).sample_rate = 48_000;
            ff::av_channel_layout_default(&mut (*ctx).ch_layout, 2);
            // TODO(surround): decoder is opened stereo; N-channel decode
            // needs the stream channel count (SDP-negotiated).
            let rc = ff::avcodec_open2(ctx, codec, ptr::null_mut());
            if rc < 0 {
                return Err(AudioError(format!("avcodec_open2: {}", err_str(rc))));
            }
            d.frame = ff::av_frame_alloc();
            d.pkt = ff::av_packet_alloc();
            if d.frame.is_null() || d.pkt.is_null() {
                return Err(AudioError("frame/packet alloc failed".into()));
            }
            Ok(d)
        }
    }

    /// Decode one opus packet, appending interleaved stereo f32 @ 48 kHz
    /// to `out`. Errors are per-packet: skip and continue.
    pub fn decode(&mut self, data: &[u8], out: &mut Vec<f32>) -> Result<(), AudioError> {
        if data.is_empty() {
            return Ok(());
        }
        unsafe {
            let rc = ff::av_new_packet(self.pkt, data.len() as i32);
            if rc < 0 {
                return Err(AudioError(format!("av_new_packet: {}", err_str(rc))));
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            let rc = ff::avcodec_send_packet(self.ctx, self.pkt);
            ff::av_packet_unref(self.pkt);
            if rc < 0 && rc != ff::AVERROR(libc::EAGAIN) {
                return Err(AudioError(format!("send_packet: {}", err_str(rc))));
            }
            loop {
                let rc = ff::avcodec_receive_frame(self.ctx, self.frame);
                if rc == ff::AVERROR(libc::EAGAIN) || rc == ff::AVERROR_EOF {
                    break;
                }
                if rc < 0 {
                    return Err(AudioError(format!("receive_frame: {}", err_str(rc))));
                }
                self.frame_to_f32(out)?;
                ff::av_frame_unref(self.frame);
            }
        }
        Ok(())
    }

    /// Interleave the current `self.frame` as f32, using the frame's own
    /// `ch_layout.nb_channels` (not a fixed count) so this stays correct
    /// if the decoder is ever opened for more than 2 channels.
    unsafe fn frame_to_f32(&mut self, out: &mut Vec<f32>) -> Result<(), AudioError> {
        unsafe {
            let f = self.frame;
            let n = (*f).nb_samples as usize;
            let ch = (*f).ch_layout.nb_channels as usize;
            if n == 0 || ch == 0 {
                return Ok(());
            }
            use ff::AVSampleFormat as S;
            let fmt = (*f).format;
            if fmt == S::AV_SAMPLE_FMT_FLTP as i32 {
                // Planar float — libavcodec's native opus output; one
                // plane per channel, `ch` of them.
                out.reserve(n * ch);
                for i in 0..n {
                    for c in 0..ch {
                        out.push(*(*f).data[c].cast::<f32>().add(i));
                    }
                }
            } else if fmt == S::AV_SAMPLE_FMT_FLT as i32 {
                // Already interleaved float.
                let p = (*f).data[0].cast::<f32>();
                out.reserve(n * ch);
                for i in 0..n {
                    let base = i * ch;
                    for c in 0..ch {
                        out.push(*p.add(base + c));
                    }
                }
            } else {
                return Err(AudioError(format!("unsupported sample format {fmt}")));
            }
            Ok(())
        }
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_packet_free(&mut self.pkt);
            ff::av_frame_free(&mut self.frame);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

// ── WASAPI shared-mode sink (Windows) ─────────────────────────────────────

#[cfg(windows)]
mod sink {
    use super::AudioError;
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::{
        AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator,
        MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eConsole,
        eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize,
    };
    use windows::Win32::System::Threading::WaitForSingleObject;
    use windows::core::{BOOL, GUID, PCWSTR};

    // `windows::Win32::System::Threading::CreateEventW`'s signature takes
    // an `Option<*const Win32::Security::SECURITY_ATTRIBUTES>`, which
    // needs the `Win32_Security` cargo feature — not currently enabled in
    // app-native's Cargo.toml, and this module always passes NULL there.
    // Bind kernel32's CreateEventW directly rather than adding a feature
    // for a parameter type we never touch (see the exclusive-mode sink
    // below for the only caller).
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateEventW(
            lpeventattributes: *const core::ffi::c_void,
            bmanualreset: BOOL,
            binitialstate: BOOL,
            lpname: PCWSTR,
        ) -> HANDLE;
    }

    const TAG_IEEE_FLOAT: u16 = 0x0003;
    const TAG_EXTENSIBLE: u16 = 0xFFFE;
    /// KSDATAFORMAT_SUBTYPE_IEEE_FLOAT.
    const SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

    impl From<windows::core::Error> for AudioError {
        fn from(e: windows::core::Error) -> Self {
            Self(e.to_string())
        }
    }

    /// Per-thread COM guard; create before any sink, drop after.
    pub struct ComGuard(());

    impl ComGuard {
        pub fn init() -> Self {
            // S_FALSE/RPC_E_CHANGED_MODE both leave COM usable here.
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            Self(())
        }
    }

    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    /// Shared-mode render endpoint at the engine mix format (f32 only —
    /// the shared engine format is float on every supported Windows).
    pub struct WasapiOut {
        client: IAudioClient,
        render: IAudioRenderClient,
        buffer_frames: u32,
        pub rate: u32,
        pub channels: u16,
    }

    impl WasapiOut {
        pub fn new() -> Result<Self, AudioError> {
            unsafe {
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
                let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;

                let fmt = client.GetMixFormat()?;
                let (rate, channels, is_f32) = {
                    // WAVEFORMATEX(TENSIBLE) is packed — read fields
                    // unaligned, never through references.
                    let f = std::ptr::read_unaligned(fmt);
                    let f32_fmt = match f.wFormatTag {
                        TAG_IEEE_FLOAT => f.wBitsPerSample == 32,
                        TAG_EXTENSIBLE => {
                            let sub =
                                std::ptr::addr_of!((*fmt.cast::<WAVEFORMATEXTENSIBLE>()).SubFormat)
                                    .read_unaligned();
                            sub == SUBTYPE_IEEE_FLOAT && f.wBitsPerSample == 32
                        }
                        _ => false,
                    };
                    (f.nSamplesPerSec, f.nChannels, f32_fmt)
                };
                if !is_f32 {
                    CoTaskMemFree(Some(fmt.cast()));
                    return Err(AudioError("mix format is not float32".into()));
                }

                // 200 ms buffer (100 ns units), timer-driven polling fill.
                let rc = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 2_000_000, 0, fmt, None);
                CoTaskMemFree(Some(fmt.cast()));
                rc?;
                let buffer_frames = client.GetBufferSize()?;
                let render: IAudioRenderClient = client.GetService()?;
                client.Start()?;
                Ok(Self {
                    client,
                    render,
                    buffer_frames,
                    rate,
                    channels,
                })
            }
        }

        /// Move as many device-format frames (interleaved f32,
        /// `self.channels` wide) from `fifo` into the render buffer as
        /// currently fit. The engine plays silence when nothing is queued.
        pub fn fill(&self, fifo: &mut std::collections::VecDeque<f32>) -> Result<(), AudioError> {
            let ch = self.channels as usize;
            unsafe {
                let padding = self.client.GetCurrentPadding()?;
                let writable = (self.buffer_frames - padding) as usize;
                let frames = (fifo.len() / ch).min(writable);
                if frames == 0 {
                    return Ok(());
                }
                let buf = self.render.GetBuffer(frames as u32)?.cast::<f32>();
                for i in 0..frames * ch {
                    *buf.add(i) = fifo.pop_front().unwrap_or(0.0);
                }
                self.render.ReleaseBuffer(frames as u32, 0)?;
            }
            Ok(())
        }
    }

    impl Drop for WasapiOut {
        fn drop(&mut self) {
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }
    /// Build an IEEE-float `WAVEFORMATEXTENSIBLE` for the exclusive-mode
    /// format probe/`Initialize` call — `rate`/`channels` fixed to the
    /// decode output so no resampling is needed on this path (module
    /// doc: "if it gets complex, prefer to bail").
    fn ieee_float_ext(rate: u32, channels: u16) -> WAVEFORMATEXTENSIBLE {
        let block_align = channels * 4; // f32 = 4 bytes/sample
        WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: TAG_EXTENSIBLE,
                nChannels: channels,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * block_align as u32,
                nBlockAlign: block_align,
                wBitsPerSample: 32,
                cbSize: 22, // trailing Samples + dwChannelMask + SubFormat
            },
            Samples: WAVEFORMATEXTENSIBLE_0 {
                wValidBitsPerSample: 32,
            },
            dwChannelMask: match channels {
                1 => 0x4, // SPEAKER_FRONT_CENTER
                2 => 0x3, // SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT
                _ => 0,   // unspecified; IsFormatSupported below will reject if unhappy
            },
            SubFormat: SUBTYPE_IEEE_FLOAT,
        }
    }

    /// Exclusive-mode, event-driven render endpoint (opt-in via
    /// `BP_AUDIO_EXCLUSIVE=1`). Bypasses the shared audio engine's mixer
    /// entirely, so it owns the device outright and only accepts the one
    /// format it was built for — [`super::run`] is expected to fall back
    /// to [`WasapiOut`] on any [`Self::new`] failure.
    pub struct WasapiExclusiveOut {
        client: IAudioClient,
        render: IAudioRenderClient,
        event: HANDLE,
        buffer_frames: u32,
        pub rate: u32,
        pub channels: u16,
    }

    // SAFETY: the interface pointers are exclusively owned by whichever
    // single thread holds this `WasapiExclusiveOut` at a time (moved
    // wholesale into the dedicated render thread in `run_exclusive`,
    // never touched from elsewhere afterward) — same discipline as
    // `OpusDecoder`'s `Send` impl above.
    unsafe impl Send for WasapiExclusiveOut {}

    impl WasapiExclusiveOut {
        /// `channels` must match the channel count already produced by
        /// the decode + convert stage.
        pub fn new(channels: u16) -> Result<Self, AudioError> {
            unsafe {
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
                let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;

                let rate = 48_000u32;
                let wfx = ieee_float_ext(rate, channels);
                let fmt = (&raw const wfx).cast::<WAVEFORMATEX>();

                let mut client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
                // Exclusive mode requires ppClosestMatch == NULL; anything
                // but S_OK means the device cannot do this exact format —
                // bail to the shared-mode fallback rather than negotiate
                // a second format.
                client
                    .IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, fmt, None)
                    .ok()
                    .map_err(|e| {
                        AudioError(format!(
                            "exclusive format ({rate} Hz / {channels} ch f32) unsupported: {e}"
                        ))
                    })?;

                let mut default_period = 0i64;
                let mut min_period = 0i64;
                client.GetDevicePeriod(Some(&mut default_period), Some(&mut min_period))?;
                let mut period = min_period.max(1);

                let mut rc = client.Initialize(
                    AUDCLNT_SHAREMODE_EXCLUSIVE,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    period,
                    period,
                    fmt,
                    None,
                );
                // WASAPI exclusive-mode contract: a misaligned period must
                // be corrected from the device's own aligned buffer size
                // and the client re-activated fresh — it cannot be
                // re-initialized in place after this particular failure
                // (MSDN "Initializing a Rendering or Capture Client").
                if let Err(e) = &rc
                    && e.code() == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED
                {
                    let frames = client.GetBufferSize()?;
                    period = (10_000_000i64 * frames as i64 + rate as i64 - 1) / rate as i64;
                    client = device.Activate(CLSCTX_ALL, None)?;
                    rc = client.Initialize(
                        AUDCLNT_SHAREMODE_EXCLUSIVE,
                        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                        period,
                        period,
                        fmt,
                        None,
                    );
                }
                rc.map_err(|e| AudioError(format!("exclusive Initialize failed: {e}")))?;

                let event = CreateEventW(std::ptr::null(), BOOL(0), BOOL(0), PCWSTR::null());
                if event.is_invalid() {
                    return Err(AudioError(
                        "CreateEventW failed for exclusive render".into(),
                    ));
                }
                if let Err(e) = client.SetEventHandle(event) {
                    let _ = CloseHandle(event);
                    return Err(AudioError(format!("SetEventHandle failed: {e}")));
                }
                let buffer_frames = client.GetBufferSize()?;
                let render: IAudioRenderClient = match client.GetService() {
                    Ok(r) => r,
                    Err(e) => {
                        let _ = CloseHandle(event);
                        return Err(e.into());
                    }
                };
                client.Start()?;
                Ok(Self {
                    client,
                    render,
                    event,
                    buffer_frames,
                    rate,
                    channels,
                })
            }
        }

        /// Blocks up to `timeout_ms` for the device's buffer-ready event;
        /// returns `false` on timeout so the caller can re-check `stopped`
        /// instead of waiting forever on a device that stopped signaling.
        pub fn wait(&self, timeout_ms: u32) -> bool {
            unsafe { WaitForSingleObject(self.event, timeout_ms) == WAIT_OBJECT_0 }
        }

        /// Pull exactly the frames the engine is ready for
        /// (`buffer_frames - padding`, normally the whole period right
        /// after the event fires) from `fifo`, padding with silence on
        /// underrun — same contract as [`WasapiOut::fill`], just without
        /// a shared-engine mixer to hide a short write.
        pub fn fill(&self, fifo: &mut std::collections::VecDeque<f32>) -> Result<(), AudioError> {
            let ch = self.channels as usize;
            unsafe {
                let padding = self.client.GetCurrentPadding()?;
                let frames = self.buffer_frames.saturating_sub(padding) as usize;
                if frames == 0 {
                    return Ok(());
                }
                let buf = self.render.GetBuffer(frames as u32)?.cast::<f32>();
                for i in 0..frames * ch {
                    *buf.add(i) = fifo.pop_front().unwrap_or(0.0);
                }
                self.render.ReleaseBuffer(frames as u32, 0)?;
            }
            Ok(())
        }
    }

    impl Drop for WasapiExclusiveOut {
        fn drop(&mut self) {
            unsafe {
                let _ = self.client.Stop();
                let _ = CloseHandle(self.event);
            }
        }
    }
}

#[cfg(windows)]
pub use sink::{ComGuard, WasapiExclusiveOut, WasapiOut};

// ── Format conversion (48 kHz stereo → device format) ─────────────────────

/// Convert interleaved f32 @ `src_rate`/`src_channels` into the device
/// format and append to `fifo`. Rate mismatch uses linear interpolation
/// per source channel (Phase A; the shared engine is 48 kHz virtually
/// always); channelization is applied after resampling — see
/// [`map_channels`] for the channel map.
fn convert_into(
    src: &[f32],
    src_channels: u16,
    src_rate: u32,
    dst_rate: u32,
    dst_channels: u16,
    fifo: &mut VecDeque<f32>,
) {
    let sch = src_channels as usize;
    let dch = dst_channels as usize;
    if sch == 0 || dch == 0 {
        return;
    }
    let src_frames = src.len() / sch;
    if src_frames == 0 {
        return;
    }
    if src_rate == dst_rate {
        for f in 0..src_frames {
            map_channels(&src[f * sch..f * sch + sch], dch, fifo);
        }
        return;
    }
    let dst_frames = (src_frames as u64 * dst_rate as u64 / src_rate as u64) as usize;
    let step = src_rate as f64 / dst_rate as f64;
    // Reused per-frame scratch buffer — one allocation for the whole
    // call, not one per resampled frame.
    let mut frame = vec![0f32; sch];
    for i in 0..dst_frames {
        let pos = i as f64 * step;
        let i0 = pos as usize;
        let i1 = (i0 + 1).min(src_frames - 1);
        let t = (pos - i0 as f64) as f32;
        for c in 0..sch {
            frame[c] = src[i0 * sch + c] * (1.0 - t) + src[i1 * sch + c] * t;
        }
        map_channels(&frame, dch, fifo);
    }
}

/// Channel map from an `src.len()`-channel frame to `dst_channels`:
/// - equal channel counts: interleaved passthrough.
/// - mono → N: replicate to channels 0/1 (L/R), rest silent.
/// - stereo → mono: average L/R (preserves the pre-surround behavior).
/// - stereo → N (N != 1): L/R to channels 0/1, rest silent.
/// - 5.1 (L R C LFE Ls Rs, FFmpeg default order) → stereo: ITU-R BS.775
///   downmix (`L' = L + 0.707*C + 0.707*Ls`, `R' = R + 0.707*C +
///   0.707*Rs`; LFE dropped), clamped to `[-1, 1]`.
/// - general fallback: copy `min(src, dst)` channels directly, extra
///   destination channels silent, extra source channels dropped.
fn map_channels(src: &[f32], dch: usize, fifo: &mut VecDeque<f32>) {
    let sch = src.len();
    if sch == dch {
        fifo.extend(src.iter().copied());
        return;
    }
    match sch {
        1 => {
            // dch != sch here, so dch >= 2 (dch == 0 is rejected by the
            // caller before any frame is mapped).
            let v = src[0];
            fifo.push_back(v);
            fifo.push_back(v);
            for _ in 2..dch {
                fifo.push_back(0.0);
            }
        }
        2 if dch == 1 => {
            fifo.push_back((src[0] + src[1]) * 0.5);
        }
        2 => {
            fifo.push_back(src[0]);
            fifo.push_back(src[1]);
            for _ in 2..dch {
                fifo.push_back(0.0);
            }
        }
        6 if dch == 2 => {
            const K: f32 = 0.707;
            let (l, r, c, _lfe, ls, rs) = (src[0], src[1], src[2], src[3], src[4], src[5]);
            fifo.push_back((l + K * c + K * ls).clamp(-1.0, 1.0));
            fifo.push_back((r + K * c + K * rs).clamp(-1.0, 1.0));
        }
        _ => {
            let n = sch.min(dch);
            for v in &src[..n] {
                fifo.push_back(*v);
            }
            for _ in n..dch {
                fifo.push_back(0.0);
            }
        }
    }
}

/// Silence sample count for the opt-in `BP_AUDIO_DEBUG_DELAY_MS` playout
/// delay (clamped to 2 s — anything longer is a footgun, not a verdict).
fn debug_delay_samples(rate: u32, channels: u16, delay_ms: u64) -> usize {
    let delay_ms = delay_ms.min(2_000);
    (rate as u64 * delay_ms / 1_000) as usize * channels as usize
}

// ── Audio thread ───────────────────────────────────────────────────────────

/// Audio thread body: drain the session's opus queue, decode, fill the
/// default render endpoint. Returns when `stopped` is set.
///
/// `BP_AUDIO_EXCLUSIVE=1` (opt-in) tries the exclusive, event-driven
/// [`WasapiExclusiveOut`] sink first; any init failure — unsupported
/// format, `AUDCLNT_E_*`, event setup — falls back to [`run_shared`], the
/// default WASAPI **shared**-mode path, so audio always works. Unset (or
/// any other value) goes straight to [`run_shared`].
#[cfg(windows)]
pub fn run(core: &RxCore, shared: &AudioShared, stopped: &AtomicBool) {
    let _com = ComGuard::init();
    // BP_AUDIO_DEBUG_DELAY_MS (opt-in, clamped to 2 s): constant playout
    // delay for the same-PC audio verdict — the streamed copy becomes an
    // audible echo behind the host's direct output, so "is remote audio
    // really playing?" needs ears, not meter squinting. Implemented as
    // silence prepended before the first fill (re-armed on sink rebuild);
    // the FIFO cap grows by the same amount so drop-oldest cannot erode
    // the offset over time.
    let delay_ms: u64 = std::env::var("BP_AUDIO_DEBUG_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut dec = match OpusDecoder::new() {
        Ok(d) => Some(d),
        Err(e) => {
            // Decoder init cannot recover by retry — audio stays off.
            tracing::error!(err = %e, "opus decoder init failed — audio off");
            shared.state.store(AUDIO_FAILED, Ordering::Release);
            None
        }
    };

    if shared.exclusive.load(Ordering::Relaxed) {
        const DECODED_CHANNELS: u16 = 2;
        match WasapiExclusiveOut::new(DECODED_CHANNELS) {
            Ok(out) => {
                if run_exclusive(core, shared, stopped, out, &mut dec, delay_ms) {
                    return;
                }
                tracing::warn!(
                    "WASAPI exclusive render failed mid-session — falling back to shared mode"
                );
            }
            Err(e) => {
                tracing::warn!(
                    err = %e,
                    "exclusive audio requested but init failed — falling back to shared mode"
                );
            }
        }
    }

    run_shared(core, shared, stopped, dec, delay_ms);
}

/// Default WASAPI **shared**-mode path: fill the shared audio engine via
/// a plain polling loop. Returns when `stopped` is set. A dead render
/// device (init failure or mid-session invalidation — e.g. the default
/// endpoint switching when a Parsec virtual device attaches) drops the
/// sink and retries every [`SINK_RETRY`]; packets are received-and-dropped
/// meanwhile (keeps the sample queue from backing up). Also the fallback
/// target when the opt-in exclusive path (see [`run`]) fails.
#[cfg(windows)]
fn run_shared(
    core: &RxCore,
    shared: &AudioShared,
    stopped: &AtomicBool,
    mut dec: Option<OpusDecoder>,
    delay_ms: u64,
) {
    // Device-format FIFO between decode and fill; ~250 ms cap (+ debug
    // delay), drop oldest (latency beats continuity for realtime audio).
    let mut fifo: VecDeque<f32> = VecDeque::new();
    let mut pcm: Vec<f32> = Vec::new();
    let mut fifo_cap = 0usize;

    /// Field report 2026-07-15: AUDCLNT_E_DEVICE_INVALIDATED mid-session.
    const SINK_RETRY: Duration = Duration::from_secs(2);
    let mut next_sink_attempt = Instant::now();

    // (Re)build the render sink: fresh FIFO, cap, debug-delay preroll.
    let build_sink = |fifo: &mut VecDeque<f32>, fifo_cap: &mut usize| -> Option<WasapiOut> {
        match WasapiOut::new() {
            Ok(o) => {
                tracing::info!(rate = o.rate, ch = o.channels, "WASAPI shared render up");
                fifo.clear();
                *fifo_cap = o.rate as usize / 4 * o.channels as usize;
                if delay_ms > 0 {
                    let silence = debug_delay_samples(o.rate, o.channels, delay_ms);
                    fifo.extend(std::iter::repeat_n(0.0f32, silence));
                    *fifo_cap += silence;
                    tracing::info!(delay_ms, samples = silence, "audio debug delay armed");
                }
                Some(o)
            }
            Err(e) => {
                tracing::error!(err = %e, "WASAPI init failed — retrying every {SINK_RETRY:?}");
                None
            }
        }
    };

    let mut out = build_sink(&mut fifo, &mut fifo_cap);
    shared.state.store(
        if out.is_some() && dec.is_some() {
            AUDIO_RUNNING
        } else {
            AUDIO_FAILED
        },
        Ordering::Release,
    );

    loop {
        match core.wait_audio(Duration::from_millis(20)) {
            Some(pkt) => {
                shared.packets.fetch_add(1, Ordering::Relaxed);
                if let (Some(dec), Some(out)) = (dec.as_mut(), out.as_ref()) {
                    pcm.clear();
                    if let Err(e) = dec.decode(&pkt, &mut pcm) {
                        shared.decode_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(err = %e, "opus decode failed — skipping packet");
                    } else {
                        // Decoder is opened stereo (see the TODO at the
                        // decoder-open site); pass its actual channel
                        // count once N-channel decode lands.
                        const DECODED_CHANNELS: u16 = 2;
                        convert_into(
                            &pcm,
                            DECODED_CHANNELS,
                            48_000,
                            out.rate,
                            out.channels,
                            &mut fifo,
                        );
                        while fifo.len() > fifo_cap {
                            fifo.pop_front();
                        }
                    }
                }
            }
            None => {
                if stopped.load(Ordering::Acquire) {
                    return;
                }
                // Closed queue returns instantly; cap the spin.
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        if let Some(o) = out.as_ref()
            && let Err(e) = o.fill(&mut fifo)
        {
            // Device invalidated (default endpoint changed, unplugged…):
            // drop the sink — no per-iteration error spam — and rebuild on
            // the retry cadence below.
            tracing::error!(err = %e, "WASAPI fill failed — dropping sink, will rebuild");
            shared.state.store(AUDIO_FAILED, Ordering::Release);
            out = None;
            fifo.clear();
            next_sink_attempt = Instant::now() + SINK_RETRY;
        }
        if out.is_none() && dec.is_some() && Instant::now() >= next_sink_attempt {
            out = build_sink(&mut fifo, &mut fifo_cap);
            if out.is_some() {
                shared.state.store(AUDIO_RUNNING, Ordering::Release);
            } else {
                next_sink_attempt = Instant::now() + SINK_RETRY;
            }
        }
    }
}

/// Exclusive-mode path (`BP_AUDIO_EXCLUSIVE=1`, see [`run`]): a dedicated
/// render thread blocks on the WASAPI buffer-ready event (via
/// [`std::thread::scope`] so it can safely borrow `shared`/`stopped`
/// without a `'static` bound) while this thread keeps draining and
/// decoding the opus queue exactly like [`run_shared`], handing decoded
/// PCM to the render thread through a small mutex-guarded FIFO.
///
/// No mid-session retry here (unlike [`run_shared`]'s `SINK_RETRY`
/// rebuild loop) — a fatal render error just ends the exclusive attempt
/// and the caller falls back to shared mode, which does its own retries.
///
/// Returns `true` when `stopped` ended the session cleanly (caller
/// should return), `false` when the render thread hit a fatal error
/// (caller should fall back to [`run_shared`]).
#[cfg(windows)]
fn run_exclusive(
    core: &RxCore,
    shared: &AudioShared,
    stopped: &AtomicBool,
    out: WasapiExclusiveOut,
    dec: &mut Option<OpusDecoder>,
    delay_ms: u64,
) -> bool {
    let rate = out.rate;
    let channels = out.channels;
    tracing::info!(rate, ch = channels, "WASAPI exclusive render up");
    shared.state.store(AUDIO_RUNNING, Ordering::Release);

    // Device-format FIFO between decode and fill. Exclusive mode targets
    // minimal latency, so the FIFO is deliberately far tighter than
    // run_shared's ~250 ms cap.
    let fifo = std::sync::Mutex::new(VecDeque::<f32>::new());
    let mut fifo_cap = (rate as usize * 40 / 1000) * channels as usize;
    if delay_ms > 0 {
        let silence = debug_delay_samples(rate, channels, delay_ms);
        fifo.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend(std::iter::repeat_n(0.0f32, silence));
        fifo_cap += silence;
        tracing::info!(delay_ms, samples = silence, "audio debug delay armed");
    }
    let failed = AtomicBool::new(false);
    // `move` below must only take ownership of `out`; `fifo`/`failed` stay
    // owned by this function and are shared with the render thread as
    // plain references (both outlive the scope, so this borrows fine).
    let fifo_ref = &fifo;
    let failed_ref = &failed;

    std::thread::scope(|s| {
        // Render thread: owns `out` outright (it is not `Sync`; see its
        // `Send` impl in the sink module) and is the only caller of
        // `wait`/`fill` for the rest of this function's lifetime.
        s.spawn(move || {
            loop {
                if stopped.load(Ordering::Acquire) {
                    return;
                }
                // Poll interval bounds worst-case shutdown latency; a
                // healthy device signals well under this every period.
                if !out.wait(50) {
                    continue;
                }
                let mut f = fifo_ref
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let r = out.fill(&mut f);
                drop(f);
                if let Err(e) = r {
                    tracing::error!(err = %e, "WASAPI exclusive fill failed");
                    shared.state.store(AUDIO_FAILED, Ordering::Release);
                    failed_ref.store(true, Ordering::Release);
                    return;
                }
            }
        });

        // Decode/produce thread: this thread. Mirrors run_shared's
        // packet loop, minus the per-iteration fill call (the render
        // thread above owns that).
        let mut pcm: Vec<f32> = Vec::new();
        loop {
            if failed.load(Ordering::Acquire) {
                return false;
            }
            match core.wait_audio(Duration::from_millis(20)) {
                Some(pkt) => {
                    shared.packets.fetch_add(1, Ordering::Relaxed);
                    if let Some(d) = dec.as_mut() {
                        pcm.clear();
                        if let Err(e) = d.decode(&pkt, &mut pcm) {
                            shared.decode_errors.fetch_add(1, Ordering::Relaxed);
                            tracing::warn!(err = %e, "opus decode failed — skipping packet");
                        } else {
                            const DECODED_CHANNELS: u16 = 2;
                            let mut f = fifo
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            convert_into(&pcm, DECODED_CHANNELS, 48_000, rate, channels, &mut f);
                            while f.len() > fifo_cap {
                                f.pop_front();
                            }
                        }
                    }
                }
                None => {
                    if stopped.load(Ordering::Acquire) {
                        return true;
                    }
                    // Closed queue returns instantly; cap the spin.
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    })
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_decoder_initializes() {
        OpusDecoder::new().expect("libavcodec has the built-in opus decoder");
    }

    /// Pins the delay→samples math (truncation order) and the 2 s clamp.
    #[test]
    fn debug_delay_samples_math_and_clamp() {
        // 300 ms @ 48 kHz stereo = 14400 frames * 2 channels.
        assert_eq!(debug_delay_samples(48_000, 2, 300), 28_800);
        // Truncates sub-millisecond remainders per the u64 division.
        assert_eq!(debug_delay_samples(44_100, 2, 1), 88);
        // Clamp: 10 s request behaves as 2 s.
        assert_eq!(
            debug_delay_samples(48_000, 2, 10_000),
            debug_delay_samples(48_000, 2, 2_000)
        );
        assert_eq!(debug_delay_samples(48_000, 2, 0), 0);
    }

    /// Pins run_exclusive's steady-state FIFO cap formula (40 ms window).
    #[test]
    fn exclusive_fifo_cap_base_formula() {
        let rate = 48_000usize;
        let channels = 2usize;
        let fifo_cap = (rate * 40 / 1000) * channels;
        assert_eq!(fifo_cap, 3_840);
    }

    /// Encode a sine with libavcodec's opus encoder (libopus, or the
    /// native encoder under experimental compliance) and decode it back:
    /// output must be 48 kHz stereo interleaved with real energy.
    #[test]
    fn opus_roundtrip_produces_stereo_pcm() {
        unsafe {
            let mut codec = ff::avcodec_find_encoder_by_name(c"libopus".as_ptr());
            let mut experimental = false;
            if codec.is_null() {
                codec = ff::avcodec_find_encoder(ff::AVCodecID::AV_CODEC_ID_OPUS);
                experimental = true;
            }
            if codec.is_null() {
                eprintln!("skipping: no opus encoder in this FFmpeg build");
                return;
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            assert!(!ctx.is_null());
            (*ctx).sample_rate = 48_000;
            ff::av_channel_layout_default(&mut (*ctx).ch_layout, 2);
            (*ctx).sample_fmt = if experimental {
                (*ctx).strict_std_compliance = ff::FF_COMPLIANCE_EXPERIMENTAL;
                ff::AVSampleFormat::AV_SAMPLE_FMT_FLTP
            } else {
                ff::AVSampleFormat::AV_SAMPLE_FMT_FLT
            };
            (*ctx).bit_rate = 96_000;
            let rc = ff::avcodec_open2(ctx, codec, ptr::null_mut());
            assert!(rc >= 0, "encoder open: {}", err_str(rc));
            let frame_size = (*ctx).frame_size as usize; // typically 960
            assert!(frame_size > 0);

            // 4 frames of a 440 Hz sine.
            let mut packets: Vec<Vec<u8>> = Vec::new();
            let frame = ff::av_frame_alloc();
            let pkt = ff::av_packet_alloc();
            for k in 0..4 {
                ff::av_frame_unref(frame);
                (*frame).nb_samples = frame_size as i32;
                (*frame).format = (*ctx).sample_fmt as i32;
                (*frame).sample_rate = 48_000;
                ff::av_channel_layout_default(&mut (*frame).ch_layout, 2);
                assert!(ff::av_frame_get_buffer(frame, 0) >= 0);
                for i in 0..frame_size {
                    let t = (k * frame_size + i) as f32 / 48_000.0;
                    let s = (t * 440.0 * std::f32::consts::TAU).sin() * 0.5;
                    if experimental {
                        // planar
                        *(*frame).data[0].cast::<f32>().add(i) = s;
                        *(*frame).data[1].cast::<f32>().add(i) = s;
                    } else {
                        // interleaved
                        *(*frame).data[0].cast::<f32>().add(i * 2) = s;
                        *(*frame).data[0].cast::<f32>().add(i * 2 + 1) = s;
                    }
                }
                assert!(ff::avcodec_send_frame(ctx, frame) >= 0);
                while ff::avcodec_receive_packet(ctx, pkt) >= 0 {
                    let data =
                        std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize).to_vec();
                    packets.push(data);
                    ff::av_packet_unref(pkt);
                }
            }
            assert!(!packets.is_empty(), "encoder produced packets");

            let mut dec = OpusDecoder::new().expect("decoder");
            let mut pcm = Vec::new();
            for p in &packets {
                dec.decode(p, &mut pcm).expect("decode");
            }
            assert!(!pcm.is_empty(), "decoded samples");
            assert_eq!(pcm.len() % 2, 0, "interleaved stereo");
            let energy: f32 = pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32;
            assert!(
                energy > 1e-4,
                "sine energy survives the roundtrip: {energy}"
            );

            // Cleanup.
            let mut frame = frame;
            let mut pkt = pkt;
            let mut ctx = ctx;
            ff::av_frame_free(&mut frame);
            ff::av_packet_free(&mut pkt);
            ff::avcodec_free_context(&mut ctx);
        }
    }

    #[test]
    fn convert_passthrough_and_channel_map() {
        // Same rate, 2ch -> 2ch: identity passthrough.
        let mut fifo = VecDeque::new();
        convert_into(&[0.1, -0.1, 0.2, -0.2], 2, 48_000, 48_000, 2, &mut fifo);
        assert_eq!(Vec::from(fifo.clone()), vec![0.1, -0.1, 0.2, -0.2]);
        // Stereo -> mono: average L/R (pre-surround behavior preserved).
        fifo.clear();
        convert_into(&[0.4, 0.2], 2, 48_000, 48_000, 1, &mut fifo);
        assert_eq!(Vec::from(fifo.clone()), vec![0.3]);
        // Stereo -> 4ch: L R 0 0.
        fifo.clear();
        convert_into(&[0.4, 0.2], 2, 48_000, 48_000, 4, &mut fifo);
        assert_eq!(Vec::from(fifo.clone()), vec![0.4, 0.2, 0.0, 0.0]);
        // Stereo -> 6ch: L R then silence.
        fifo.clear();
        convert_into(&[0.4, 0.2], 2, 48_000, 48_000, 6, &mut fifo);
        assert_eq!(Vec::from(fifo.clone()), vec![0.4, 0.2, 0.0, 0.0, 0.0, 0.0]);
        // Mono -> 2ch: replicate to L/R.
        fifo.clear();
        convert_into(&[0.4, 0.2], 1, 48_000, 48_000, 2, &mut fifo);
        assert_eq!(Vec::from(fifo.clone()), vec![0.4, 0.4, 0.2, 0.2]);
        // Mono -> 4ch: replicate to L/R, rest silent.
        fifo.clear();
        convert_into(&[0.5], 1, 48_000, 48_000, 4, &mut fifo);
        assert_eq!(Vec::from(fifo.clone()), vec![0.5, 0.5, 0.0, 0.0]);
        // 5.1 (L R C LFE Ls Rs) -> 6ch: identity passthrough.
        let surround = [0.1_f32, -0.2, 0.3, -0.4, 0.5, -0.6];
        fifo.clear();
        convert_into(&surround, 6, 48_000, 48_000, 6, &mut fifo);
        assert_eq!(Vec::from(fifo.clone()), surround.to_vec());
        // 5.1 -> stereo: ITU-R BS.775 downmix, LFE dropped.
        // L=0.1 R=0.2 C=0.3 LFE=0.9(dropped) Ls=0.4 Rs=0.5
        let surround = [0.1_f32, 0.2, 0.3, 0.9, 0.4, 0.5];
        fifo.clear();
        convert_into(&surround, 6, 48_000, 48_000, 2, &mut fifo);
        let got = Vec::from(fifo.clone());
        assert_eq!(got.len(), 2);
        let expect_l = 0.1 + 0.707 * 0.3 + 0.707 * 0.4;
        let expect_r = 0.2 + 0.707 * 0.3 + 0.707 * 0.5;
        assert!(
            (got[0] - expect_l).abs() < 1e-4,
            "L' = L+0.707C+0.707Ls: got {} expected {expect_l}",
            got[0]
        );
        assert!(
            (got[1] - expect_r).abs() < 1e-4,
            "R' = R+0.707C+0.707Rs: got {} expected {expect_r}",
            got[1]
        );
        // 5.1 -> stereo downmix clamps to [-1, 1].
        let loud = [1.0_f32, 1.0, 1.0, 0.0, 1.0, 1.0];
        fifo.clear();
        convert_into(&loud, 6, 48_000, 48_000, 2, &mut fifo);
        let got = Vec::from(fifo.clone());
        assert_eq!(got, vec![1.0, 1.0]);
        // Rate halving keeps frame count proportional (stereo).
        fifo.clear();
        let src: Vec<f32> = (0..96).map(|i| i as f32 / 96.0).collect(); // 48 frames
        convert_into(&src, 2, 48_000, 24_000, 2, &mut fifo);
        assert_eq!(fifo.len() / 2, 24);
    }

    /// Init the default render endpoint and push 100 ms of silence
    /// through the shared engine. Skips when the machine has no audio
    /// endpoint (bare CI).
    #[cfg(windows)]
    #[test]
    fn wasapi_renders_silence() {
        let _com = ComGuard::init();
        let out = match WasapiOut::new() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("skipping: {e}");
                return;
            }
        };
        assert!(out.rate > 0 && out.channels > 0);
        let mut fifo: VecDeque<f32> =
            std::iter::repeat_n(0.0, out.rate as usize / 10 * out.channels as usize).collect();
        while !fifo.is_empty() {
            out.fill(&mut fifo).expect("fill");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
