//! FFmpeg H.264 decode — A0 slice 2 (design D6 Phase A,
//! docs/design/unified-app-architecture.md §4-1).
//!
//! D3D11VA hwaccel with software fallback. Decoded pictures are
//! downloaded to CPU and converted to RGBA for the interim egui present;
//! slice 3 replaces the present with the raw FLIP_DISCARD swapchain
//! consuming the D3D11 texture directly (D5) — the decode half of this
//! module survives that switch as-is.
//!
//! FrameQueue guarantees complete Annex-B access units (video_rx
//! reassembles), so there is no `av_parser` stage: one `DecodeUnit` ==
//! one `avcodec_send_packet`.

use std::ptr;

use ffmpeg_sys_next as ff;

/// One decoded picture, RGBA8, tightly packed (`width * 4` stride).
pub struct RgbaFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
}

/// One decoded picture as NV12 planes (tight strides), for the GPU present
/// path: Y plane (`width*height` bytes) + interleaved UV plane
/// (`width*(height/2)` bytes) plus the YUV->RGB matrix the shader applies.
/// Avoids the CPU swscale and halves the CPU->GPU upload vs `RgbaFrame`.
pub struct Nv12Frame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub uv: Vec<u8>,
    /// Row-major YUV->RGB: `out_c = m[c][0]*Y + m[c][1]*U + m[c][2]*V + m[c][3]`
    /// with Y,U,V the R8 / R8G8-normalized samples in [0,1].
    pub matrix: [[f32; 4]; 3],
}

/// A decoded picture in whichever form the pump produced: `Rgba` (the
/// swscale CPU path, and the software-decode / egui-fallback form) or
/// `Nv12` (the GPU present path — planes + matrix, no CPU color convert).
pub enum DecodedFrame {
    Rgba(RgbaFrame),
    Nv12(Nv12Frame),
}

impl DecodedFrame {
    /// Picture dimensions (width, height) regardless of form.
    pub fn dims(&self) -> (u32, u32) {
        match self {
            DecodedFrame::Rgba(f) => (f.width as u32, f.height as u32),
            DecodedFrame::Nv12(f) => (f.width as u32, f.height as u32),
        }
    }
}

/// Metadata about a decoder output, populated **only** when FFmpeg actually
/// produced a picture. `is_key` reflects the decoded `AVFrame`'s real key
/// status (`AV_FRAME_FLAG_KEY`), never the submitted packet's `is_key` flag:
/// a submitted key packet that fails, or that FFmpeg swallows without
/// emitting a picture (B-frame reorder / buffering), must never be reported
/// as a decoded key. `epoch`/`frame_id` are the input `DecodeUnit`'s, passed
/// through so the caller can match this output against the epoch it is
/// currently waiting for recovery on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeMeta {
    pub epoch: u32,
    pub frame_id: u32,
    pub is_key: bool,
}

/// A decoded picture paired with the [`DecodeMeta`] of the access unit that
/// produced it.
pub struct Decoded<T> {
    pub frame: T,
    pub meta: DecodeMeta,
}

/// Decode failure. The pump latches needs-IDR upstream and keeps going;
/// nothing here is fatal to the session.
#[derive(Debug)]
pub struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
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

/// `get_format` callback: prefer D3D11 hw surfaces, otherwise fall back to
/// the first (software) format libavcodec offers.
unsafe extern "C" fn pick_pixfmt(
    _ctx: *mut ff::AVCodecContext,
    fmts: *const ff::AVPixelFormat,
) -> ff::AVPixelFormat {
    unsafe {
        let mut i = 0;
        while *fmts.offset(i) != ff::AVPixelFormat::AV_PIX_FMT_NONE {
            if *fmts.offset(i) == ff::AVPixelFormat::AV_PIX_FMT_D3D11 {
                return ff::AVPixelFormat::AV_PIX_FMT_D3D11;
            }
            i += 1;
        }
        // Start of the list == first (most preferred) software format, or
        // AV_PIX_FMT_NONE when the list is empty (libavcodec then fails
        // cleanly).
        *fmts
    }
}

// ── Color space / range (native-grade color accuracy) ──────────────────────
//
// swscale's default YUV→RGB uses BT.601 limited-range coefficients unless
// told otherwise, so an HD (BT.709) stream decoded through the default
// path comes out with a visibly wrong matrix (skin tones / saturated
// colors shift). WebCodecs on the browser client reads the bitstream VUI
// automatically; this native path must apply it explicitly from the
// decoded frame's `colorspace` / `color_range`.

// swscale.h SWS_CS_* coefficient-table selectors (stable ABI values).
const SWS_CS_ITU709_: i32 = 1;
const SWS_CS_ITU601_: i32 = 5; // == SMPTE170M / BT470BG
const SWS_CS_BT2020_: i32 = 9;
const SWS_CS_RGB_: i32 = 5; // dst table for RGB output (unused coeffs)

// AVColorSpace enum values (stable).
const AVCOL_SPC_BT709_: i32 = 1;
const AVCOL_SPC_BT470BG_: i32 = 5;
const AVCOL_SPC_SMPTE170M_: i32 = 6;
const AVCOL_SPC_BT2020_NCL_: i32 = 9;
const AVCOL_SPC_BT2020_CL_: i32 = 10;
// AVColorRange.
const AVCOL_RANGE_JPEG_: i32 = 2; // full range

/// Pick the swscale coefficient table for a decoded frame's `colorspace`.
/// Unspecified/unknown falls back by resolution — the universal heuristic:
/// ≥720p is BT.709, below is BT.601.
fn sws_cs_for(av_colorspace: i32, height: i32) -> i32 {
    match av_colorspace {
        AVCOL_SPC_BT709_ => SWS_CS_ITU709_,
        AVCOL_SPC_BT2020_NCL_ | AVCOL_SPC_BT2020_CL_ => SWS_CS_BT2020_,
        AVCOL_SPC_SMPTE170M_ | AVCOL_SPC_BT470BG_ => SWS_CS_ITU601_,
        _ => {
            if height >= 720 {
                SWS_CS_ITU709_
            } else {
                SWS_CS_ITU601_
            }
        }
    }
}

/// Whether the YUV input is full-range: explicit JPEG range, or a `YUVJ*`
/// pixel format (implicit full range). Otherwise limited (MPEG/TV) range.
fn is_full_range(av_color_range: i32, pix_fmt: i32) -> bool {
    if av_color_range == AVCOL_RANGE_JPEG_ {
        return true;
    }
    pix_fmt == ff::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32
}

/// YUV->RGB matrix rows for a decoded frame's colorspace selector (an
/// `SWS_CS_*` value from [`sws_cs_for`]) and range. Computed from the ITU
/// luma coefficients so the present shader is a fixed dot product; mirrors
/// the swscale path's matrix + range so the GPU output matches the CPU one.
fn yuv_to_rgb_matrix(sws_cs: i32, full_range: bool) -> [[f32; 4]; 3] {
    let (kr, kb) = match sws_cs {
        SWS_CS_ITU601_ => (0.299_f32, 0.114_f32),
        SWS_CS_BT2020_ => (0.2627_f32, 0.0593_f32),
        _ => (0.2126_f32, 0.0722_f32), // BT.709 (and the ≥720p default)
    };
    let kg = 1.0 - kr - kb;
    let cr = 2.0 * (1.0 - kr); // R <- Cr(V)
    let cb = 2.0 * (1.0 - kb); // B <- Cb(U)
    let gu = -kb * cb / kg; // G <- Cb(U)
    let gv = -kr * cr / kg; // G <- Cr(V)
    // Sample normalization: limited range has Y in [16,235]/255 and chroma in
    // [16,240]/255; full range uses the raw [0,1] with a 128/255 chroma bias.
    let (ys, yo, cscale, co) = if full_range {
        (1.0_f32, 0.0_f32, 1.0_f32, 128.0 / 255.0)
    } else {
        (255.0 / 219.0, 16.0 / 255.0, 255.0 / 224.0, 128.0 / 255.0)
    };
    // Y' = (Y-yo)*ys ; Cb = (U-co)*cscale ; Cr = (V-co)*cscale.
    // out = Y' + a*Cb + b*Cr, expanded to [wY, wU, wV, offset].
    let row = |a: f32, b: f32| {
        [
            ys,
            a * cscale,
            b * cscale,
            -ys * yo - a * cscale * co - b * cscale * co,
        ]
    };
    [row(0.0, cr), row(gu, gv), row(cb, 0.0)]
}

pub struct Decoder {
    ctx: *mut ff::AVCodecContext,
    frame: *mut ff::AVFrame,
    sw_frame: *mut ff::AVFrame,
    pkt: *mut ff::AVPacket,
    sws: *mut ff::SwsContext,
    /// A D3D11VA device is attached (negotiation may still pick software).
    pub hw_device: bool,
}

// SAFETY: all pointers are exclusively owned by this struct; FFmpeg
// contexts may move between threads as long as they are not shared. The
// frame pump is the only user.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Result<Self, DecodeError> {
        unsafe {
            let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_H264);
            if codec.is_null() {
                return Err(DecodeError("no H.264 decoder in libavcodec".into()));
            }
            let ctx = ff::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                return Err(DecodeError("avcodec_alloc_context3 failed".into()));
            }
            // From here on `d` owns everything; Drop is null-safe, so any
            // early return cleans up whatever exists.
            let mut d = Self {
                ctx,
                frame: ptr::null_mut(),
                sw_frame: ptr::null_mut(),
                pkt: ptr::null_mut(),
                sws: ptr::null_mut(),
                hw_device: false,
            };

            (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;
            // Frame threading buffers N frames of latency and hwaccel is
            // single-threaded anyway; the stream is realtime, not a file.
            (*ctx).thread_count = 1;

            let mut dev: *mut ff::AVBufferRef = ptr::null_mut();
            let rc = ff::av_hwdevice_ctx_create(
                &mut dev,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA,
                ptr::null(),
                ptr::null_mut(),
                0,
            );
            if rc >= 0 {
                (*ctx).hw_device_ctx = ff::av_buffer_ref(dev);
                ff::av_buffer_unref(&mut dev);
                (*ctx).get_format = Some(pick_pixfmt);
                d.hw_device = !(*ctx).hw_device_ctx.is_null();
            } else {
                tracing::warn!(err = %err_str(rc), "D3D11VA unavailable — software decode");
            }

            let rc = ff::avcodec_open2(ctx, codec, ptr::null_mut());
            if rc < 0 {
                return Err(DecodeError(format!("avcodec_open2: {}", err_str(rc))));
            }

            d.frame = ff::av_frame_alloc();
            d.sw_frame = ff::av_frame_alloc();
            d.pkt = ff::av_packet_alloc();
            if d.frame.is_null() || d.sw_frame.is_null() || d.pkt.is_null() {
                return Err(DecodeError("frame/packet alloc failed".into()));
            }
            Ok(d)
        }
    }

    /// Discontinuity flush: drop the in-flight reference chain and any
    /// buffered/pending output before decoding resumes on a new epoch.
    /// Without this, `avcodec_receive_frame` can keep draining pictures
    /// built from the pre-reset reference chain (stale decoder output
    /// presenting after a reset). Call before the first `send_packet` of
    /// the new epoch.
    pub fn flush(&mut self) {
        unsafe {
            ff::avcodec_flush_buffers(self.ctx);
            ff::av_frame_unref(self.frame);
            ff::av_frame_unref(self.sw_frame);
        }
    }

    /// Feed one complete Annex-B access unit; returns the newest decoded
    /// picture with its [`DecodeMeta`], if FFmpeg actually produced one.
    /// `epoch`/`frame_id` come from the input `DecodeUnit` and are only
    /// ever attached to output that FFmpeg emits in response to this call
    /// — never inferred from the packet alone. `Err` means the reference
    /// chain is suspect — latch needs-IDR upstream and keep pumping.
    pub fn decode(
        &mut self,
        epoch: u32,
        frame_id: u32,
        data: &[u8],
    ) -> Result<Option<Decoded<RgbaFrame>>, DecodeError> {
        if data.is_empty() {
            return Ok(None);
        }
        unsafe {
            let rc = ff::av_new_packet(self.pkt, data.len() as i32);
            if rc < 0 {
                return Err(DecodeError(format!("av_new_packet: {}", err_str(rc))));
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            let rc = ff::avcodec_send_packet(self.ctx, self.pkt);
            ff::av_packet_unref(self.pkt);
            if rc < 0 && rc != ff::AVERROR(libc::EAGAIN) {
                return Err(DecodeError(format!("send_packet: {}", err_str(rc))));
            }

            let mut out = None;
            loop {
                let rc = ff::avcodec_receive_frame(self.ctx, self.frame);
                if rc == ff::AVERROR(libc::EAGAIN) || rc == ff::AVERROR_EOF {
                    break;
                }
                if rc < 0 {
                    return Err(DecodeError(format!("receive_frame: {}", err_str(rc))));
                }
                let is_key = (*self.frame).flags & ff::AV_FRAME_FLAG_KEY as i32 != 0;
                let frame = self.frame_to_rgba()?;
                ff::av_frame_unref(self.frame);
                out = Some(Decoded {
                    frame,
                    meta: DecodeMeta {
                        epoch,
                        frame_id,
                        is_key,
                    },
                });
            }
            Ok(out)
        }
    }

    /// Decode one access unit but discard the output picture. Advances the
    /// decoder state — keeping the P-frame reference chain intact — without
    /// the expensive HW download + swscale that [`Decoder::decode`] does.
    /// Used to skip stale frames when the present side has fallen behind so
    /// only the newest picture is ever converted and shown (bounds
    /// presentation latency under load). Still reports [`DecodeMeta`] for
    /// any picture FFmpeg actually produces — the backlog-drain path must
    /// be able to recognize/ack a real decoded key even when the picture
    /// itself is never presented.
    pub fn decode_drop(
        &mut self,
        epoch: u32,
        frame_id: u32,
        data: &[u8],
    ) -> Result<Option<DecodeMeta>, DecodeError> {
        if data.is_empty() {
            return Ok(None);
        }
        unsafe {
            let rc = ff::av_new_packet(self.pkt, data.len() as i32);
            if rc < 0 {
                return Err(DecodeError(format!("av_new_packet: {}", err_str(rc))));
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            let rc = ff::avcodec_send_packet(self.ctx, self.pkt);
            ff::av_packet_unref(self.pkt);
            if rc < 0 && rc != ff::AVERROR(libc::EAGAIN) {
                return Err(DecodeError(format!("send_packet: {}", err_str(rc))));
            }
            let mut out = None;
            loop {
                let rc = ff::avcodec_receive_frame(self.ctx, self.frame);
                if rc == ff::AVERROR(libc::EAGAIN) || rc == ff::AVERROR_EOF {
                    break;
                }
                if rc < 0 {
                    return Err(DecodeError(format!("receive_frame: {}", err_str(rc))));
                }
                let is_key = (*self.frame).flags & ff::AV_FRAME_FLAG_KEY as i32 != 0;
                ff::av_frame_unref(self.frame);
                out = Some(DecodeMeta {
                    epoch,
                    frame_id,
                    is_key,
                });
            }
            Ok(out)
        }
    }

    /// Download (when on a D3D11 surface) and convert the current
    /// `self.frame` to RGBA.
    unsafe fn frame_to_rgba(&mut self) -> Result<RgbaFrame, DecodeError> {
        unsafe {
            let mut src: *mut ff::AVFrame = self.frame;
            if (*self.frame).format == ff::AVPixelFormat::AV_PIX_FMT_D3D11 as i32 {
                ff::av_frame_unref(self.sw_frame);
                let rc = ff::av_hwframe_transfer_data(self.sw_frame, self.frame, 0);
                if rc < 0 {
                    return Err(DecodeError(format!("hwframe transfer: {}", err_str(rc))));
                }
                src = self.sw_frame;
            }
            let w = (*src).width;
            let h = (*src).height;
            if w <= 0 || h <= 0 {
                return Err(DecodeError("decoded frame has no dimensions".into()));
            }
            // Explicit allowlist instead of transmuting the raw int back
            // into the bindgen enum.
            use ff::AVPixelFormat as P;
            let src_fmt = [
                P::AV_PIX_FMT_NV12,
                P::AV_PIX_FMT_YUV420P,
                P::AV_PIX_FMT_YUVJ420P,
            ]
            .into_iter()
            .find(|f| *f as i32 == (*src).format)
            .ok_or_else(|| {
                DecodeError(format!(
                    "unsupported decoded pixel format {}",
                    (*src).format
                ))
            })?;

            self.sws = ff::sws_getCachedContext(
                self.sws,
                w,
                h,
                src_fmt,
                w,
                h,
                P::AV_PIX_FMT_RGBA,
                ff::SWS_FAST_BILINEAR, // 1:1, pixel-format conversion only
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            );
            if self.sws.is_null() {
                return Err(DecodeError("sws_getCachedContext failed".into()));
            }

            // Apply the stream's real matrix + range (native-grade color).
            // Default swscale coefficients are BT.601 limited; HD is BT.709.
            let cs = sws_cs_for((*src).colorspace as i32, h);
            let src_full = is_full_range((*src).color_range as i32, (*src).format);
            let inv_table = ff::sws_getCoefficients(cs);
            let rgb_table = ff::sws_getCoefficients(SWS_CS_RGB_);
            if !inv_table.is_null() && !rgb_table.is_null() {
                // brightness 0, contrast/saturation unity (1<<16); dst RGB
                // is full-range.
                ff::sws_setColorspaceDetails(
                    self.sws,
                    inv_table,
                    i32::from(src_full),
                    rgb_table,
                    1,
                    0,
                    1 << 16,
                    1 << 16,
                );
            }

            // Fill without the wasted zero-init: sws_scale overwrites every
            // one of `len` bytes below (full frame, tight w*4 stride), so the
            // per-frame 8 MB memset is pure overhead at 60 fps.
            let len = w as usize * h as usize * 4;
            let mut rgba: Vec<u8> = Vec::with_capacity(len);
            let dst_data: [*mut u8; 4] = [
                rgba.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ];
            let dst_stride: [i32; 4] = [w * 4, 0, 0, 0];
            let rc = ff::sws_scale(
                self.sws,
                (*src).data.as_ptr().cast::<*const u8>(),
                (*src).linesize.as_ptr(),
                0,
                h,
                dst_data.as_ptr(),
                dst_stride.as_ptr(),
            );
            if rc < 0 {
                return Err(DecodeError(format!("sws_scale: {}", err_str(rc))));
            }
            // SAFETY: sws_scale returned >= 0 above, so it wrote all `len`
            // destination bytes; the capacity was reserved for exactly `len`.
            #[allow(clippy::uninit_vec)]
            rgba.set_len(len);
            Ok(RgbaFrame {
                width: w as usize,
                height: h as usize,
                rgba,
            })
        }
    }

    /// Like [`Decoder::decode`] but prefers NV12 planes for the GPU present
    /// path (no CPU color conversion): when the decoded picture is NV12,
    /// returns [`DecodedFrame::Nv12`]. When it isn't (e.g. software decode
    /// engaged mid-stream and yielded YUV420P), that is *not* a decode
    /// failure — this falls back to the RGBA/swscale path for that one
    /// picture and returns [`DecodedFrame::Rgba`] instead, exactly like
    /// [`Decoder::decode`] would have. `Err` is reserved for a genuine
    /// decode error (send/receive_packet failure, or the RGBA fallback
    /// itself failing) — never for a merely-non-NV12 picture — so recovery
    /// state/IDR requests upstream are never re-armed for a picture that
    /// actually decoded fine. Same `epoch`/`frame_id`/real-key
    /// [`DecodeMeta`] contract as [`Decoder::decode`].
    pub fn decode_nv12(
        &mut self,
        epoch: u32,
        frame_id: u32,
        data: &[u8],
    ) -> Result<Option<Decoded<DecodedFrame>>, DecodeError> {
        if data.is_empty() {
            return Ok(None);
        }
        unsafe {
            let rc = ff::av_new_packet(self.pkt, data.len() as i32);
            if rc < 0 {
                return Err(DecodeError(format!("av_new_packet: {}", err_str(rc))));
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (*self.pkt).data, data.len());
            let rc = ff::avcodec_send_packet(self.ctx, self.pkt);
            ff::av_packet_unref(self.pkt);
            if rc < 0 && rc != ff::AVERROR(libc::EAGAIN) {
                return Err(DecodeError(format!("send_packet: {}", err_str(rc))));
            }
            let mut out = None;
            loop {
                let rc = ff::avcodec_receive_frame(self.ctx, self.frame);
                if rc == ff::AVERROR(libc::EAGAIN) || rc == ff::AVERROR_EOF {
                    break;
                }
                if rc < 0 {
                    return Err(DecodeError(format!("receive_frame: {}", err_str(rc))));
                }
                let is_key = (*self.frame).flags & ff::AV_FRAME_FLAG_KEY as i32 != 0;
                // Try the NV12 fast path first; a non-NV12 picture isn't a
                // decode failure, so fall back to the RGBA/swscale path
                // for this one picture (frame_to_rgba supports NV12,
                // YUV420P, and YUVJ420P — frame_to_nv12 only accepts a
                // literal NV12 copy) instead of propagating an error.
                let frame = match self.frame_to_nv12() {
                    Ok(nv12) => DecodedFrame::Nv12(nv12),
                    Err(_) => DecodedFrame::Rgba(self.frame_to_rgba()?),
                };
                ff::av_frame_unref(self.frame);
                out = Some(Decoded {
                    frame,
                    meta: DecodeMeta {
                        epoch,
                        frame_id,
                        is_key,
                    },
                });
            }
            Ok(out)
        }
    }

    /// Download (when on a D3D11 surface) the current `self.frame` and copy
    /// its NV12 planes tightly packed. `Err` for non-NV12 formats so the
    /// caller can fall back to the RGBA path.
    unsafe fn frame_to_nv12(&mut self) -> Result<Nv12Frame, DecodeError> {
        unsafe {
            let mut src: *mut ff::AVFrame = self.frame;
            if (*self.frame).format == ff::AVPixelFormat::AV_PIX_FMT_D3D11 as i32 {
                ff::av_frame_unref(self.sw_frame);
                let rc = ff::av_hwframe_transfer_data(self.sw_frame, self.frame, 0);
                if rc < 0 {
                    return Err(DecodeError(format!("hwframe transfer: {}", err_str(rc))));
                }
                src = self.sw_frame;
            }
            if (*src).format != ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32 {
                return Err(DecodeError("decoded frame is not NV12".into()));
            }
            let w = (*src).width;
            let h = (*src).height;
            if w <= 0 || h <= 0 {
                return Err(DecodeError("decoded frame has no dimensions".into()));
            }
            let (w, h) = (w as usize, h as usize);
            let y_ls = (*src).linesize[0] as usize;
            let y_ptr = (*src).data[0];
            let mut y = Vec::with_capacity(w * h);
            for r in 0..h {
                y.extend_from_slice(std::slice::from_raw_parts(y_ptr.add(r * y_ls), w));
            }
            let uv_ls = (*src).linesize[1] as usize;
            let uv_ptr = (*src).data[1];
            let uv_h = h / 2;
            let mut uv = Vec::with_capacity(w * uv_h);
            for r in 0..uv_h {
                uv.extend_from_slice(std::slice::from_raw_parts(uv_ptr.add(r * uv_ls), w));
            }
            let cs = sws_cs_for((*src).colorspace as i32, h as i32);
            let full = is_full_range((*src).color_range as i32, (*src).format);
            Ok(Nv12Frame {
                width: w,
                height: h,
                y,
                uv,
                matrix: yuv_to_rgb_matrix(cs, full),
            })
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            ff::sws_freeContext(self.sws); // NULL-safe
            ff::av_packet_free(&mut self.pkt);
            ff::av_frame_free(&mut self.sw_frame);
            ff::av_frame_free(&mut self.frame);
            ff::avcodec_free_context(&mut self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn apply_matrix(m: &[[f32; 4]; 3], y: f32, u: f32, v: f32) -> [f32; 3] {
        [
            m[0][0] * y + m[0][1] * u + m[0][2] * v + m[0][3],
            m[1][0] * y + m[1][1] * u + m[1][2] * v + m[1][3],
            m[2][0] * y + m[2][1] * u + m[2][2] * v + m[2][3],
        ]
    }

    #[test]
    fn nv12_matrix_grayscale_axis() {
        // BT.709 limited: neutral chroma (128/255) maps the luma ramp
        // [16,235]/255 onto [0,1] RGB gray — the GPU shader must match the
        // swscale path exactly on the achromatic axis.
        let m = yuv_to_rgb_matrix(SWS_CS_ITU709_, false);
        let c = 128.0 / 255.0;
        let black = apply_matrix(&m, 16.0 / 255.0, c, c);
        let white = apply_matrix(&m, 235.0 / 255.0, c, c);
        for ch in 0..3 {
            assert!(black[ch].abs() < 1e-3, "black[{ch}] = {}", black[ch]);
            assert!(
                (white[ch] - 1.0).abs() < 1e-3,
                "white[{ch}] = {}",
                white[ch]
            );
        }
        // Full range: raw [0,1] luma, neutral chroma.
        let mf = yuv_to_rgb_matrix(SWS_CS_ITU709_, true);
        let fb = apply_matrix(&mf, 0.0, c, c);
        let fw = apply_matrix(&mf, 1.0, c, c);
        for ch in 0..3 {
            assert!(fb[ch].abs() < 1e-3, "full black[{ch}] = {}", fb[ch]);
            assert!((fw[ch] - 1.0).abs() < 1e-3, "full white[{ch}] = {}", fw[ch]);
        }
    }

    #[test]
    fn nv12_matrix_chroma_direction() {
        // Cr (V) above neutral pushes red up; Cb (U) above neutral pushes
        // blue up — sanity that the chroma coefficients have the right sign.
        let m = yuv_to_rgb_matrix(SWS_CS_ITU709_, false);
        let c = 128.0 / 255.0;
        let mid = 128.0 / 255.0;
        let neutral = apply_matrix(&m, mid, c, c);
        let more_v = apply_matrix(&m, mid, c, 200.0 / 255.0);
        let more_u = apply_matrix(&m, mid, 200.0 / 255.0, c);
        assert!(more_v[0] > neutral[0] + 0.1, "R must rise with Cr");
        assert!(more_u[2] > neutral[2] + 0.1, "B must rise with Cb");
    }

    /// The pinned FFmpeg CLI from tools/bootstrap-ffmpeg.ps1 (same tree the
    /// `video` feature links against). None → skip fixture generation.
    fn ffmpeg_cli() -> Option<PathBuf> {
        let dir = std::env::var_os("FFMPEG_DIR")?;
        let exe = if cfg!(windows) {
            "ffmpeg.exe"
        } else {
            "ffmpeg"
        };
        let p = PathBuf::from(dir).join("bin").join(exe);
        p.is_file().then_some(p)
    }

    /// Split an Annex-B bitstream into access units: a new AU starts at a
    /// non-VCL NAL (SPS/PPS/SEI/AUD) or a VCL NAL with first_mb_in_slice
    /// == 0 (ue(0) == leading bit set) once the current AU already holds a
    /// VCL NAL. Start codes are preserved so each AU is valid Annex-B.
    fn split_access_units(bs: &[u8]) -> Vec<Vec<u8>> {
        // Offsets of every start code (3- or 4-byte).
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= bs.len() {
            if bs[i] == 0 && bs[i + 1] == 0 && bs[i + 2] == 1 {
                let sc = if i > 0 && bs[i - 1] == 0 { i - 1 } else { i };
                starts.push((sc, i + 3));
                i += 3;
            } else {
                i += 1;
            }
        }
        let mut aus: Vec<Vec<u8>> = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        let mut cur_has_vcl = false;
        for (k, &(sc, payload)) in starts.iter().enumerate() {
            let end = starts.get(k + 1).map_or(bs.len(), |&(next_sc, _)| next_sc);
            let nal_type = bs[payload] & 0x1f;
            let is_vcl = nal_type == 1 || nal_type == 5;
            let first_mb_zero = is_vcl && bs.get(payload + 1).is_some_and(|b| b & 0x80 != 0);
            let new_au = cur_has_vcl && (!is_vcl || first_mb_zero);
            if new_au {
                aus.push(std::mem::take(&mut cur));
                cur_has_vcl = false;
            }
            cur.extend_from_slice(&bs[sc..end]);
            cur_has_vcl |= is_vcl;
        }
        if !cur.is_empty() {
            aus.push(cur);
        }
        aus
    }

    #[test]
    fn decoder_initializes_hw_or_sw() {
        let d = Decoder::new().expect("decoder init");
        // Either outcome is valid; just prove init + teardown don't blow up.
        drop(d);
    }

    #[test]
    fn garbage_input_never_yields_a_frame() {
        let mut d = Decoder::new().expect("decoder init");
        for i in 0..4 {
            if let Ok(Some(_)) = d.decode(0, i, &[0x42u8; 512]) {
                panic!("garbage produced a frame");
            }
        }
    }

    #[test]
    fn decodes_synthetic_openh264_stream() {
        let Some(cli) = ffmpeg_cli() else {
            eprintln!("skip: pinned FFmpeg CLI not present (run tools/bootstrap-ffmpeg.ps1)");
            return;
        };
        let fixture = std::env::temp_dir().join("betterparsec-a0-slice2.h264");
        let status = Command::new(cli)
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=30:duration=1",
                "-c:v",
                "libopenh264",
                "-g",
                "30",
                "-f",
                "h264",
            ])
            .arg(&fixture)
            .status()
            .expect("run ffmpeg CLI");
        assert!(status.success(), "fixture encode failed");
        let bs = std::fs::read(&fixture).expect("read fixture");

        let aus = split_access_units(&bs);
        assert!(
            aus.len() >= 25,
            "expected ~30 access units, got {}",
            aus.len()
        );

        let mut dec = Decoder::new().expect("decoder init");
        let mut frames = 0usize;
        let mut last: Option<RgbaFrame> = None;
        let mut first_meta: Option<DecodeMeta> = None;
        for (i, au) in aus.iter().enumerate() {
            if let Some(d) = dec.decode(0, i as u32, au).expect("decode AU") {
                frames += 1;
                assert_eq!(d.meta.epoch, 0);
                assert_eq!(d.meta.frame_id, i as u32);
                first_meta.get_or_insert(d.meta);
                last = Some(d.frame);
            }
        }
        assert!(frames >= 25, "decoded only {frames} of {} AUs", aus.len());
        // Real AVFrame key status (not the packet's own header) must mark
        // the GOP's first decoded picture as a key — the field this whole
        // slice hangs the recovery-ack decision on.
        assert!(
            first_meta.expect("at least one picture").is_key,
            "first decoded picture of a fresh GOP must report is_key from the real AVFrame"
        );
        let f = last.expect("at least one picture");
        assert_eq!((f.width, f.height), (320, 240));
        assert_eq!(f.rgba.len(), 320 * 240 * 4);
        // testsrc2 is colorful — a flat fill means the convert path lied.
        let first: [u8; 4] = f.rgba[..4].try_into().expect("4-byte pixel");
        assert!(
            f.rgba.chunks_exact(4).any(|px| px != first),
            "decoded frame is a flat fill"
        );
    }

    #[test]
    fn colorspace_selection_matches_stream_signalling() {
        // Explicit signalling is honoured regardless of resolution.
        assert_eq!(sws_cs_for(AVCOL_SPC_BT709_, 480), SWS_CS_ITU709_);
        assert_eq!(sws_cs_for(AVCOL_SPC_SMPTE170M_, 1080), SWS_CS_ITU601_);
        assert_eq!(sws_cs_for(AVCOL_SPC_BT470BG_, 1080), SWS_CS_ITU601_);
        assert_eq!(sws_cs_for(AVCOL_SPC_BT2020_NCL_, 2160), SWS_CS_BT2020_);
        assert_eq!(sws_cs_for(AVCOL_SPC_BT2020_CL_, 2160), SWS_CS_BT2020_);
        // Unspecified (2) falls back by resolution: HD→709, SD→601. This
        // is the case that was previously always wrong (601 default).
        assert_eq!(sws_cs_for(2, 1080), SWS_CS_ITU709_);
        assert_eq!(sws_cs_for(2, 720), SWS_CS_ITU709_);
        assert_eq!(sws_cs_for(2, 576), SWS_CS_ITU601_);
        assert_eq!(sws_cs_for(2, 480), SWS_CS_ITU601_);
    }

    #[test]
    fn range_selection_flags_full_range_sources() {
        let nv12 = ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        let yuvj = ffmpeg_sys_next::AVPixelFormat::AV_PIX_FMT_YUVJ420P as i32;
        // Explicit JPEG range → full, regardless of pixfmt.
        assert!(is_full_range(AVCOL_RANGE_JPEG_, nv12));
        // YUVJ pixel format → implicit full even when range unspecified.
        assert!(is_full_range(0, yuvj));
        // Plain NV12 with unspecified/MPEG range → limited.
        assert!(!is_full_range(0, nv12));
        assert!(!is_full_range(1, nv12));
    }
    #[test]
    fn flush_clears_reference_chain() {
        let Some(cli) = ffmpeg_cli() else {
            eprintln!("skip: pinned FFmpeg CLI not present (run tools/bootstrap-ffmpeg.ps1)");
            return;
        };
        let fixture = std::env::temp_dir().join("betterparsec-a0-slice2-flush.h264");
        let status = Command::new(cli)
            .args([
                "-y",
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=320x240:rate=30:duration=1",
                "-c:v",
                "libopenh264",
                "-g",
                "30",
                "-f",
                "h264",
            ])
            .arg(&fixture)
            .status()
            .expect("run ffmpeg CLI");
        assert!(status.success(), "fixture encode failed");
        let bs = std::fs::read(&fixture).expect("read fixture");
        let aus = split_access_units(&bs);
        assert!(
            aus.len() >= 10,
            "need at least a few AUs, got {}",
            aus.len()
        );

        let mut dec = Decoder::new().expect("decoder init");
        // Warm the reference chain: IDR plus a few deltas.
        for (i, au) in aus.iter().take(5).enumerate() {
            dec.decode(0, i as u32, au).expect("decode AU");
        }
        dec.flush();
        // Post-flush, a delta access unit has no valid reference chain
        // (avcodec_flush_buffers dropped it). FFmpeg must never hand back a
        // picture built from the pre-flush references as if it were fresh
        // output for the new epoch — the defect this flush call exists to
        // close (stale decoder output presenting after a reset).
        let post_flush_delta = &aus[5];
        match dec.decode(1, 999, post_flush_delta) {
            Ok(Some(_)) => panic!(
                "a delta AU decoded immediately after flush must not yield a picture — \
                 the reference chain was just dropped"
            ),
            Ok(None) | Err(_) => {}
        }
    }
}
