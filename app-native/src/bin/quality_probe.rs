//! `quality-probe` — quality-per-bit measurement of the "free lunch" NVENC
//! levers (quality-efficiency-audit.md §A/D: the host encoder defaults to
//! preset `p1` (fastest/lowest quality) with spatial `adaptive_quantization`
//! OFF, even though both are essentially free at fixed CBR bitrate on modern
//! NVENC hardware).
//!
//! Standalone measurement binary — no library code depends on it, and it
//! does not import from the crate's private modules (mirrors the decode
//! setup in `app-native/src/video.rs` conceptually, but self-contained).
//! Encodes synthetic 1920x1080 NV12 frames through `h264_nvenc` across a
//! `preset x spatial_aq` cartesian at a fixed CBR bitrate, decodes the
//! produced Annex-B back with the software `h264` decoder, and reports
//! PSNR/SSIM of decoded luma vs. source luma — the quality gained (or not)
//! per bit for each lever, holding bitrate constant.
//!
//! Run: `cargo run --release -p app-native --features video --bin quality-probe`
//!
//! Exits non-zero (no panic) if `h264_nvenc` or the `h264` decoder is
//! missing, or any config fails to open/encode/decode; the table for
//! whatever configs did succeed is still printed first.

use std::ffi::CStr;
use std::ptr;

use ffmpeg_sys_next as ff;

const WIDTH: i32 = 1920;
const HEIGHT: i32 = 1080;
const FPS: i32 = 60;
const GOP: i32 = 120;
const BITRATE: i64 = 8_000_000; // fixed 8 Mbps CBR — isolate quality-per-bit, not bit budget.
const FRAMES: u32 = 120;
const TUNE: &CStr = c"ull";

/// `(display name, av_opt_set "preset" value)`.
const PRESETS: [(&str, &CStr); 3] = [("p1", c"p1"), ("p4", c"p4"), ("p6", c"p6")];
/// `(display name, spatial_aq on/off)`.
const AQ_MODES: [(&str, bool); 2] = [("aq-off", false), ("aq-on", true)];

// ── Pure metric functions (unit-tested below) ───────────────────────────────

/// PSNR in dB between two equal-length 8-bit luma buffers. Identical buffers
/// clamp to a high sentinel (99.0) instead of returning infinity.
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len(), "psnr: buffer length mismatch");
    if a.is_empty() {
        return 99.0;
    }
    let sq_err_sum: f64 = a
        .iter()
        .zip(b.iter())
        .map(|(&x, &y)| {
            let d = x as f64 - y as f64;
            d * d
        })
        .sum();
    let mse = sq_err_sum / a.len() as f64;
    if mse <= 0.0 {
        return 99.0;
    }
    let db = 10.0 * (255.0f64 * 255.0 / mse).log10();
    db.min(99.0)
}

const SSIM_C1: f64 = 0.01 * 0.01 * 255.0 * 255.0;
const SSIM_C2: f64 = 0.03 * 0.03 * 255.0 * 255.0;

/// SSIM over one square window of equal-length luma samples (any shape, as
/// long as `a.len() == b.len()`).
fn ssim_window(a: &[u8], b: &[u8]) -> f64 {
    let n = a.len() as f64;
    let mean_a = a.iter().map(|&v| v as f64).sum::<f64>() / n;
    let mean_b = b.iter().map(|&v| v as f64).sum::<f64>() / n;
    let mut var_a = 0.0;
    let mut var_b = 0.0;
    let mut covar = 0.0;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let dx = x as f64 - mean_a;
        let dy = y as f64 - mean_b;
        var_a += dx * dx;
        var_b += dy * dy;
        covar += dx * dy;
    }
    var_a /= n;
    var_b /= n;
    covar /= n;
    let numerator = (2.0 * mean_a * mean_b + SSIM_C1) * (2.0 * covar + SSIM_C2);
    let denominator = (mean_a * mean_a + mean_b * mean_b + SSIM_C1) * (var_a + var_b + SSIM_C2);
    numerator / denominator
}

/// Windowed (8x8) mean SSIM over a `w x h` luma plane, tightly packed
/// (`a.len() == b.len() == w * h`).
fn ssim(a: &[u8], b: &[u8], w: usize, h: usize) -> f64 {
    assert_eq!(a.len(), w * h, "ssim: a length != w*h");
    assert_eq!(b.len(), w * h, "ssim: b length != w*h");
    const WIN: usize = 8;
    let mut sum = 0.0f64;
    let mut count = 0u32;
    let mut abuf: Vec<u8> = Vec::with_capacity(WIN * WIN);
    let mut bbuf: Vec<u8> = Vec::with_capacity(WIN * WIN);
    let mut by = 0usize;
    while by < h {
        let bh = WIN.min(h - by);
        let mut bx = 0usize;
        while bx < w {
            let bw = WIN.min(w - bx);
            abuf.clear();
            bbuf.clear();
            for row in 0..bh {
                let base = (by + row) * w + bx;
                abuf.extend_from_slice(&a[base..base + bw]);
                bbuf.extend_from_slice(&b[base..base + bw]);
            }
            sum += ssim_window(&abuf, &bbuf);
            count += 1;
            bx += WIN;
        }
        by += WIN;
    }
    if count == 0 {
        return 1.0;
    }
    sum / count as f64
}

// ── Deterministic stress content generator ──────────────────────────────────

/// xorshift32, seeded per-pixel-per-frame — deterministic, no external RNG
/// dependency.
fn xorshift32(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

/// Luma value at `(x, y)` for `frame_idx`: a moving diagonal gradient (so
/// motion estimation has real work to do) XOR'd with a per-pixel
/// checkerboard (Nyquist-frequency edges NVENC can't just skip) plus
/// deterministic pseudo-random noise (so flat-region fast paths don't
/// dominate). This is the single source of truth for both the encoder
/// input and the PSNR/SSIM reference — the decoded frame at index `i` is
/// compared against `luma_at(x, y, i)`.
fn luma_at(x: u32, y: u32, frame_idx: u32) -> u8 {
    let grad = (x.wrapping_add(y).wrapping_add(frame_idx.wrapping_mul(3))) & 0xff;
    let checker: i32 = if (x ^ y) & 1 == 1 { 24 } else { -24 };
    let seed = x.wrapping_mul(374_761_393)
        ^ y.wrapping_mul(668_265_263)
        ^ frame_idx.wrapping_mul(2_654_435_761);
    let noise = (xorshift32(seed.wrapping_add(1)) & 0x1f) as i32 - 16; // [-16, 15]
    (grad as i32 + checker + noise).clamp(0, 255) as u8
}

unsafe fn fill_stress_frame(frame: *mut ff::AVFrame, frame_idx: u32) {
    unsafe {
        let y_stride = (*frame).linesize[0] as usize;
        let y_plane = (*frame).data[0];
        for row in 0..HEIGHT as u32 {
            let dst = y_plane.add(row as usize * y_stride);
            for col in 0..WIDTH as u32 {
                *dst.add(col as usize) = luma_at(col, row, frame_idx);
            }
        }
        // Chroma is not part of the quality comparison — fill with a
        // steady mid-gray so it doesn't starve the luma rate-control
        // budget on this synthetic content.
        let uv_stride = (*frame).linesize[1] as usize;
        let uv_plane = (*frame).data[1];
        for row in 0..(HEIGHT as usize / 2) {
            let dst = uv_plane.add(row * uv_stride);
            for pair in 0..(WIDTH as usize / 2) {
                *dst.add(pair * 2) = 128; // U
                *dst.add(pair * 2 + 1) = 128; // V
            }
        }
    }
}

// ── FFmpeg scaffolding ───────────────────────────────────────────────────────

fn err_str(rc: i32) -> String {
    let mut buf = [0u8; 128];
    let ok = unsafe { ff::av_strerror(rc, buf.as_mut_ptr().cast(), buf.len()) } >= 0;
    if !ok {
        return format!("ffmpeg error {rc}");
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    format!("{} ({rc})", String::from_utf8_lossy(&buf[..end]))
}

/// Allocate + configure + open an `h264_nvenc` context for one config.
/// Frees the context itself on any failure; the caller only ever sees a
/// live, opened context or an error.
unsafe fn open_encoder(preset: (&str, &CStr), aq: bool) -> Result<*mut ff::AVCodecContext, String> {
    unsafe {
        let codec = ff::avcodec_find_encoder_by_name(c"h264_nvenc".as_ptr());
        if codec.is_null() {
            return Err("h264_nvenc not found in the linked libavcodec".into());
        }
        let mut ctx = ff::avcodec_alloc_context3(codec);
        if ctx.is_null() {
            return Err("avcodec_alloc_context3 failed".into());
        }

        (*ctx).width = WIDTH;
        (*ctx).height = HEIGHT;
        (*ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_NV12;
        (*ctx).time_base = ff::AVRational { num: 1, den: FPS };
        (*ctx).framerate = ff::AVRational { num: FPS, den: 1 };
        (*ctx).gop_size = GOP;
        (*ctx).max_b_frames = 0;
        (*ctx).bit_rate = BITRATE;
        (*ctx).rc_max_rate = BITRATE;
        (*ctx).rc_buffer_size = (BITRATE / FPS as i64 * 2) as i32; // tight VBV — CBR, low latency
        (*ctx).flags |= ff::AV_CODEC_FLAG_LOW_DELAY as i32;

        let priv_data = (*ctx).priv_data;
        let rc = ff::av_opt_set(priv_data, c"preset".as_ptr(), preset.1.as_ptr(), 0);
        if rc < 0 {
            ff::avcodec_free_context(&mut ctx);
            return Err(format!("av_opt_set(preset={}): {}", preset.0, err_str(rc)));
        }
        let rc = ff::av_opt_set(priv_data, c"tune".as_ptr(), TUNE.as_ptr(), 0);
        if rc < 0 {
            ff::avcodec_free_context(&mut ctx);
            return Err(format!("av_opt_set(tune=ull): {}", err_str(rc)));
        }
        let rc = ff::av_opt_set(priv_data, c"rc".as_ptr(), c"cbr".as_ptr(), 0);
        if rc < 0 {
            ff::avcodec_free_context(&mut ctx);
            return Err(format!("av_opt_set(rc=cbr): {}", err_str(rc)));
        }
        let rc = ff::av_opt_set_int(priv_data, c"delay".as_ptr(), 0, 0);
        if rc < 0 {
            ff::avcodec_free_context(&mut ctx);
            return Err(format!("av_opt_set_int(delay=0): {}", err_str(rc)));
        }

        // spatial_aq: the h264_nvenc AVOption is named "spatial_aq"
        // (underscore) as of FFmpeg 7.1's libavcodec/nvenc.c. Fall back to
        // the hyphenated spelling if the underscore form is rejected, and
        // note it — this probe exists specifically to verify the free
        // lunch levers actually take effect, not just that we asked.
        let aq_val: i64 = if aq { 1 } else { 0 };
        let rc = ff::av_opt_set_int(priv_data, c"spatial_aq".as_ptr(), aq_val, 0);
        if rc < 0 {
            let rc2 = ff::av_opt_set_int(priv_data, c"spatial-aq".as_ptr(), aq_val, 0);
            if rc2 < 0 {
                ff::avcodec_free_context(&mut ctx);
                return Err(format!(
                    "av_opt_set_int(spatial_aq={aq_val}): {} (spatial-aq fallback also failed: {})",
                    err_str(rc),
                    err_str(rc2)
                ));
            }
            eprintln!(
                "quality-probe: note: h264_nvenc rejected \"spatial_aq\", used \"spatial-aq\" instead"
            );
        }

        let rc = ff::avcodec_open2(ctx, codec, ptr::null_mut());
        if rc < 0 {
            let msg = format!(
                "avcodec_open2(preset={}, aq={aq}): {}",
                preset.0,
                err_str(rc)
            );
            ff::avcodec_free_context(&mut ctx);
            return Err(msg);
        }
        Ok(ctx)
    }
}

unsafe fn alloc_frame() -> Result<*mut ff::AVFrame, String> {
    unsafe {
        let mut frame = ff::av_frame_alloc();
        if frame.is_null() {
            return Err("av_frame_alloc failed".into());
        }
        (*frame).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        (*frame).width = WIDTH;
        (*frame).height = HEIGHT;
        let rc = ff::av_frame_get_buffer(frame, 32);
        if rc < 0 {
            ff::av_frame_free(&mut frame);
            return Err(format!("av_frame_get_buffer: {}", err_str(rc)));
        }
        Ok(frame)
    }
}

/// Open a plain software `h264` decoder context (no hwaccel — this probe
/// only needs correctness, not real-time speed).
unsafe fn open_decoder() -> Result<*mut ff::AVCodecContext, String> {
    unsafe {
        let codec = ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_H264);
        if codec.is_null() {
            return Err("h264 decoder not found in the linked libavcodec".into());
        }
        let mut ctx = ff::avcodec_alloc_context3(codec);
        if ctx.is_null() {
            return Err("avcodec_alloc_context3 (decoder) failed".into());
        }
        let rc = ff::avcodec_open2(ctx, codec, ptr::null_mut());
        if rc < 0 {
            let msg = format!("avcodec_open2 (decoder): {}", err_str(rc));
            ff::avcodec_free_context(&mut ctx);
            return Err(msg);
        }
        Ok(ctx)
    }
}

/// Copy a decoded frame's luma plane into a tightly packed `WIDTH * HEIGHT`
/// buffer, respecting `linesize` (decoder output is commonly row-aligned
/// wider than `WIDTH`).
unsafe fn copy_luma_plane(frame: *const ff::AVFrame) -> Vec<u8> {
    unsafe {
        let stride = (*frame).linesize[0] as usize;
        let plane = (*frame).data[0];
        let mut out = vec![0u8; WIDTH as usize * HEIGHT as usize];
        for row in 0..HEIGHT as usize {
            let src = std::slice::from_raw_parts(plane.add(row * stride), WIDTH as usize);
            out[row * WIDTH as usize..(row + 1) * WIDTH as usize].copy_from_slice(src);
        }
        out
    }
}

struct Row {
    preset: &'static str,
    aq_label: &'static str,
    aq: bool,
    bitrate_mbps: f64,
    psnr_db: f64,
    ssim: f64,
    error: Option<String>,
}

unsafe fn run_config(preset: (&'static str, &'static CStr), aq_mode: (&'static str, bool)) -> Row {
    let (aq_label, aq) = aq_mode;
    let fail = |msg: String| Row {
        preset: preset.0,
        aq_label,
        aq,
        bitrate_mbps: BITRATE as f64 / 1_000_000.0,
        psnr_db: 0.0,
        ssim: 0.0,
        error: Some(msg),
    };

    let mut enc_ctx = match unsafe { open_encoder(preset, aq) } {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let mut frame = match unsafe { alloc_frame() } {
        Ok(f) => f,
        Err(e) => {
            unsafe { ff::avcodec_free_context(&mut enc_ctx) };
            return fail(e);
        }
    };
    let mut pkt = unsafe { ff::av_packet_alloc() };
    if pkt.is_null() {
        unsafe {
            ff::av_frame_free(&mut frame);
            ff::avcodec_free_context(&mut enc_ctx);
        }
        return fail("av_packet_alloc failed".into());
    }

    // Encode all frames; collect each output access unit's bytes in order
    // (encode output order == input submission order here: max_b_frames=0,
    // delay=0, so no reordering).
    let mut access_units: Vec<Vec<u8>> = Vec::with_capacity(FRAMES as usize);
    let mut run_err: Option<String> = None;

    'frames: for i in 0..FRAMES {
        unsafe {
            let rc = ff::av_frame_make_writable(frame);
            if rc < 0 {
                run_err = Some(format!(
                    "av_frame_make_writable(frame {i}): {}",
                    err_str(rc)
                ));
                break 'frames;
            }
            fill_stress_frame(frame, i);
            (*frame).pts = i as i64;
        }

        let rc = unsafe { ff::avcodec_send_frame(enc_ctx, frame) };
        if rc < 0 {
            run_err = Some(format!("avcodec_send_frame(frame {i}): {}", err_str(rc)));
            break 'frames;
        }
        loop {
            let rc = unsafe { ff::avcodec_receive_packet(enc_ctx, pkt) };
            if rc == ff::AVERROR(libc::EAGAIN) || rc == ff::AVERROR_EOF {
                break;
            }
            if rc < 0 {
                run_err = Some(format!(
                    "avcodec_receive_packet(frame {i}): {}",
                    err_str(rc)
                ));
                unsafe { ff::av_packet_unref(pkt) };
                break 'frames;
            }
            unsafe {
                let data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
                access_units.push(data.to_vec());
                ff::av_packet_unref(pkt);
            }
        }
    }

    if run_err.is_none() {
        unsafe {
            let _ = ff::avcodec_send_frame(enc_ctx, ptr::null());
            loop {
                let rc = ff::avcodec_receive_packet(enc_ctx, pkt);
                if rc == ff::AVERROR(libc::EAGAIN) || rc == ff::AVERROR_EOF {
                    break;
                }
                if rc < 0 {
                    break;
                }
                let data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
                access_units.push(data.to_vec());
                ff::av_packet_unref(pkt);
            }
        }
    }

    unsafe {
        ff::av_packet_free(&mut pkt);
        ff::av_frame_free(&mut frame);
        ff::avcodec_free_context(&mut enc_ctx);
    }

    if let Some(err) = run_err {
        return fail(err);
    }
    if access_units.is_empty() {
        return fail("no access units produced by the encoder".into());
    }

    // Decode every access unit and compare each decoded luma plane against
    // the deterministic source generator at the matching frame index.
    let mut dec_ctx = match unsafe { open_decoder() } {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let mut dec_pkt = unsafe { ff::av_packet_alloc() };
    let mut dec_frame = unsafe { ff::av_frame_alloc() };
    if dec_pkt.is_null() || dec_frame.is_null() {
        unsafe {
            if !dec_pkt.is_null() {
                ff::av_packet_free(&mut dec_pkt);
            }
            if !dec_frame.is_null() {
                ff::av_frame_free(&mut dec_frame);
            }
            ff::avcodec_free_context(&mut dec_ctx);
        }
        return fail("decoder av_packet_alloc/av_frame_alloc failed".into());
    }

    let mut psnr_sum = 0.0f64;
    let mut ssim_sum = 0.0f64;
    let mut decoded_count = 0u32;
    let mut dec_err: Option<String> = None;

    let mut feed = |data: &[u8], eof: bool, dec_err: &mut Option<String>| unsafe {
        if !eof {
            let rc = ff::av_new_packet(dec_pkt, data.len() as i32);
            if rc < 0 {
                *dec_err = Some(format!("av_new_packet: {}", err_str(rc)));
                return;
            }
            ptr::copy_nonoverlapping(data.as_ptr(), (*dec_pkt).data, data.len());
        }
        let send_rc = ff::avcodec_send_packet(dec_ctx, if eof { ptr::null() } else { dec_pkt });
        if !eof {
            ff::av_packet_unref(dec_pkt);
        }
        if send_rc < 0 && send_rc != ff::AVERROR_EOF {
            *dec_err = Some(format!("avcodec_send_packet: {}", err_str(send_rc)));
            return;
        }
        loop {
            let rc = ff::avcodec_receive_frame(dec_ctx, dec_frame);
            if rc == ff::AVERROR(libc::EAGAIN) || rc == ff::AVERROR_EOF {
                break;
            }
            if rc < 0 {
                *dec_err = Some(format!("avcodec_receive_frame: {}", err_str(rc)));
                return;
            }
            if (*dec_frame).width != WIDTH || (*dec_frame).height != HEIGHT {
                eprintln!(
                    "quality-probe: decoded frame size {}x{} != source {}x{}, skipping",
                    (*dec_frame).width,
                    (*dec_frame).height,
                    WIDTH,
                    HEIGHT
                );
                continue;
            }
            let decoded_luma = copy_luma_plane(dec_frame);
            let idx = decoded_count;
            let mut source_luma = vec![0u8; WIDTH as usize * HEIGHT as usize];
            for row in 0..HEIGHT as u32 {
                for col in 0..WIDTH as u32 {
                    source_luma[(row * WIDTH as u32 + col) as usize] = luma_at(col, row, idx);
                }
            }
            psnr_sum += psnr(&decoded_luma, &source_luma);
            ssim_sum += ssim(&decoded_luma, &source_luma, WIDTH as usize, HEIGHT as usize);
            decoded_count += 1;
        }
    };

    for au in &access_units {
        if dec_err.is_some() {
            break;
        }
        feed(au, false, &mut dec_err);
    }
    if dec_err.is_none() {
        feed(&[], true, &mut dec_err);
    }

    unsafe {
        ff::av_frame_free(&mut dec_frame);
        ff::av_packet_free(&mut dec_pkt);
        ff::avcodec_free_context(&mut dec_ctx);
    }

    if let Some(err) = dec_err {
        return fail(err);
    }
    if decoded_count == 0 {
        return fail("no frames decoded from the encoded bitstream".into());
    }

    Row {
        preset: preset.0,
        aq_label,
        aq,
        bitrate_mbps: BITRATE as f64 / 1_000_000.0,
        psnr_db: psnr_sum / decoded_count as f64,
        ssim: ssim_sum / decoded_count as f64,
        error: None,
    }
}

fn print_table(rows: &[Row]) {
    println!("| config | bitrate (Mbps) | PSNR (dB) | SSIM |");
    println!("|---|---|---|---|");
    for r in rows {
        let config = format!("preset={} tune=ull {}", r.preset, r.aq_label);
        if let Some(err) = &r.error {
            println!("| {config} | {:.1} | FAILED | FAILED |", r.bitrate_mbps);
            eprintln!("quality-probe: {config}: {err}");
            continue;
        }
        println!(
            "| {config} | {:.1} | {:.2} | {:.4} |",
            r.bitrate_mbps, r.psnr_db, r.ssim
        );
    }
}

fn print_verdict(rows: &[Row]) {
    let baseline = rows
        .iter()
        .find(|r| r.preset == "p1" && !r.aq && r.error.is_none());
    let free_lunch = rows
        .iter()
        .find(|r| r.preset == "p4" && r.aq && r.error.is_none());
    match (baseline, free_lunch) {
        (Some(base), Some(candidate)) => {
            let psnr_delta = candidate.psnr_db - base.psnr_db;
            let ssim_delta = candidate.ssim - base.ssim;
            println!(
                "Verdict: (preset=p4, aq-on) vs (preset=p1, aq-off) at the same {:.1} Mbps CBR -> \
                 PSNR {psnr_delta:+.2} dB, SSIM {ssim_delta:+.4} (free-lunch quality-per-bit gain).",
                base.bitrate_mbps
            );
        }
        _ => {
            println!(
                "Verdict: insufficient successful configs to compare (preset=p4, aq-on) vs \
                 (preset=p1, aq-off)."
            );
        }
    }
}

fn main() {
    let encoder_present =
        unsafe { !ff::avcodec_find_encoder_by_name(c"h264_nvenc".as_ptr()).is_null() };
    if !encoder_present {
        eprintln!(
            "quality-probe: h264_nvenc encoder not found in the linked libavcodec \
             (check third-party/ffmpeg — see tools/bootstrap-ffmpeg.ps1)"
        );
        std::process::exit(1);
    }
    let decoder_present =
        unsafe { !ff::avcodec_find_decoder(ff::AVCodecID::AV_CODEC_ID_H264).is_null() };
    if !decoder_present {
        eprintln!("quality-probe: h264 decoder not found in the linked libavcodec");
        std::process::exit(1);
    }

    let mut rows: Vec<Row> = Vec::new();
    let mut any_failed = false;
    for preset in PRESETS {
        for aq_mode in AQ_MODES {
            eprintln!(
                "quality-probe: running preset={} tune=ull {} ...",
                preset.0, aq_mode.0
            );
            let row = unsafe { run_config(preset, aq_mode) };
            any_failed |= row.error.is_some();
            rows.push(row);
        }
    }

    print_table(&rows);
    print_verdict(&rows);

    if any_failed {
        eprintln!(
            "quality-probe: one or more configs failed to open/encode/decode; see errors above."
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psnr_identical_buffers_clamps_to_sentinel() {
        let a = [1u8, 2, 3, 4, 250, 0, 128, 200];
        assert_eq!(psnr(&a, &a), 99.0);
    }

    #[test]
    fn psnr_differing_buffers_is_below_sentinel() {
        let a = [10u8; 64];
        let b = [50u8; 64];
        let p = psnr(&a, &b);
        assert!(p < 99.0);
        assert!(p.is_finite());
    }

    #[test]
    fn psnr_more_error_means_lower_value() {
        let a = [100u8; 64];
        let small_err = {
            let mut b = a;
            for v in b.iter_mut() {
                *v = v.saturating_add(2);
            }
            b
        };
        let big_err = {
            let mut b = a;
            for v in b.iter_mut() {
                *v = v.saturating_add(40);
            }
            b
        };
        assert!(psnr(&a, &small_err) > psnr(&a, &big_err));
    }

    #[test]
    fn ssim_identical_buffers_is_one() {
        let w = 16;
        let h = 16;
        let mut a = vec![0u8; w * h];
        for (i, v) in a.iter_mut().enumerate() {
            *v = (i % 255) as u8;
        }
        let s = ssim(&a, &a, w, h);
        assert!((s - 1.0).abs() < 1e-9, "expected ~1.0, got {s}");
    }

    #[test]
    fn ssim_decreases_when_noise_added() {
        let w = 32;
        let h = 32;
        let mut a = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                a[y * w + x] = luma_at(x as u32, y as u32, 0);
            }
        }
        let mut noisy = a.clone();
        for (i, v) in noisy.iter_mut().enumerate() {
            let n = xorshift32(i as u32 + 1) & 0x3f; // 0..63
            *v = v.saturating_add(n as u8);
        }
        let s_identical = ssim(&a, &a, w, h);
        let s_noisy = ssim(&a, &noisy, w, h);
        assert!(
            s_noisy < s_identical,
            "noisy SSIM {s_noisy} should be < identical SSIM {s_identical}"
        );
    }

    #[test]
    fn luma_at_is_deterministic() {
        assert_eq!(luma_at(5, 7, 3), luma_at(5, 7, 3));
    }
}
