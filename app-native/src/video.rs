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

    /// Feed one complete Annex-B access unit; returns the newest decoded
    /// picture, if any. `Err` means the reference chain is suspect —
    /// latch needs-IDR upstream and keep pumping.
    pub fn decode(&mut self, data: &[u8]) -> Result<Option<RgbaFrame>, DecodeError> {
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
                out = Some(self.frame_to_rgba()?);
                ff::av_frame_unref(self.frame);
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

            let mut rgba = vec![0u8; w as usize * h as usize * 4];
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
            Ok(RgbaFrame {
                width: w as usize,
                height: h as usize,
                rgba,
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
        for _ in 0..4 {
            if let Ok(Some(_)) = d.decode(&[0x42u8; 512]) {
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
        for au in &aus {
            if let Some(f) = dec.decode(au).expect("decode AU") {
                frames += 1;
                last = Some(f);
            }
        }
        assert!(frames >= 25, "decoded only {frames} of {} AUs", aus.len());
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
}
