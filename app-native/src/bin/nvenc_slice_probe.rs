//! `nvenc-slice-probe` — RTX 4070 ULL/slice NVENC encode-latency probe
//! (ROADMAP P0: "RTX 4070 ULL/슬라이스 인코드 지연 자체 실측", resolving the
//! docs/research/04-sota-feasibility-and-strategy.md §3-1 literature
//! conflict ahead of the U3 `slices_per_frame` tuning decision).
//!
//! Standalone measurement binary — no library code depends on it. Encodes
//! synthetic 1920x1080 NV12 frames through `h264_nvenc` across a
//! `slices x preset` cartesian (tune is pinned to `ull`) and reports:
//!   - per-frame encode wall time (`avcodec_send_frame` + drain
//!     `avcodec_receive_packet` until EAGAIN), p50/p95/mean in µs
//!   - the slice count *actually observed* in the encoded bitstream (VCL
//!     NAL count per access unit), because `h264_nvenc` exposes no private
//!     `slices` AVOption — the portable lever is the generic
//!     `AVCodecContext.slices` field, and this probe verifies it took
//!     effect instead of trusting the request
//!
//! Run: `cargo run --release -p app-native --features video --bin nvenc-slice-probe`
//!
//! Exits non-zero (no panic) if `h264_nvenc` is missing or any config fails
//! to open/encode; the table for whatever configs did succeed is still
//! printed first.

use std::ffi::CStr;
use std::ptr;
use std::time::Instant;

use ffmpeg_sys_next as ff;

const WIDTH: i32 = 1920;
const HEIGHT: i32 = 1080;
const FPS: i32 = 60;
const GOP: i32 = 300;
const BITRATE: i64 = 10_000_000; // ~10 Mbps CBR
const WARMUP_FRAMES: u32 = 30;
const MEASURED_FRAMES: u32 = 300;
const TUNE: &CStr = c"ull";

/// `(display name, av_opt_set "preset" value)`.
const PRESETS: [(&str, &CStr); 2] = [("p1", c"p1"), ("p4", c"p4")];
const SLICE_COUNTS: [i32; 3] = [1, 2, 4];

struct Row {
    slices_req: i32,
    preset: &'static str,
    /// Modal VCL-NAL-per-AU count observed across the measured phase.
    /// `None` means the config errored before any packet was produced.
    observed_slices: Option<i32>,
    /// `observed_slices != Some(slices_req)`.
    suspect: bool,
    p50_us: f64,
    p95_us: f64,
    mean_us: f64,
    fps_equiv: f64,
    error: Option<String>,
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

/// Count Annex-B VCL NAL units (type 1 non-IDR slice, type 5 IDR slice) in
/// one encoded access unit — this is the ground truth for "how many slices
/// did NVENC actually emit", independent of what we asked for.
fn count_vcl_nals(data: &[u8]) -> i32 {
    let mut count = 0i32;
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(&hdr) = data.get(i + 3) {
                let nal_type = hdr & 0x1f;
                if nal_type == 1 || nal_type == 5 {
                    count += 1;
                }
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    count
}

fn mode(values: &[i32]) -> Option<i32> {
    use std::collections::HashMap;
    let mut counts: HashMap<i32, u32> = HashMap::new();
    for &v in values {
        *counts.entry(v).or_insert(0) += 1;
    }
    counts.into_iter().max_by_key(|&(_, c)| c).map(|(v, _)| v)
}

/// Linear-interpolated percentile over a pre-sorted slice (`p` in `0.0..=1.0`).
fn percentile(sorted: &[u64], p: f64) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n == 1 {
        return sorted[0] as f64;
    }
    let pos = p * (n - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    if lo == hi {
        return sorted[lo] as f64;
    }
    let frac = pos - lo as f64;
    sorted[lo] as f64 * (1.0 - frac) + sorted[hi] as f64 * frac
}

/// Allocate + configure + open an `h264_nvenc` context for one config.
/// Frees the context itself on any failure; the caller only ever sees a
/// live, opened context or an error.
unsafe fn open_encoder(
    slices: i32,
    preset: (&str, &CStr),
) -> Result<*mut ff::AVCodecContext, String> {
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
        (*ctx).slices = slices;
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
        // zerolatency-equivalent: 0 frames of internal reorder/output delay.
        let rc = ff::av_opt_set_int(priv_data, c"delay".as_ptr(), 0, 0);
        if rc < 0 {
            ff::avcodec_free_context(&mut ctx);
            return Err(format!("av_opt_set_int(delay=0): {}", err_str(rc)));
        }

        let rc = ff::avcodec_open2(ctx, codec, ptr::null_mut());
        if rc < 0 {
            let msg = format!(
                "avcodec_open2(slices={slices}, preset={}): {}",
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

/// Fill NV12 planes with a moving gradient (position + frame index) so
/// content isn't static — a flat/static source would let NVENC skip real
/// work and understate latency.
unsafe fn fill_gradient(frame: *mut ff::AVFrame, frame_idx: u32) {
    unsafe {
        let shift = frame_idx as usize;
        let y_stride = (*frame).linesize[0] as usize;
        let y_plane = (*frame).data[0];
        for row in 0..HEIGHT as usize {
            let dst = y_plane.add(row * y_stride);
            for col in 0..WIDTH as usize {
                *dst.add(col) = ((col + row + shift) & 0xff) as u8;
            }
        }
        let uv_stride = (*frame).linesize[1] as usize;
        let uv_plane = (*frame).data[1];
        for row in 0..(HEIGHT as usize / 2) {
            let dst = uv_plane.add(row * uv_stride);
            for pair in 0..(WIDTH as usize / 2) {
                *dst.add(pair * 2) = ((row + shift) & 0xff) as u8; // U
                *dst.add(pair * 2 + 1) = ((pair + shift) & 0xff) as u8; // V
            }
        }
    }
}

unsafe fn run_config(slices: i32, preset: (&'static str, &'static CStr)) -> Row {
    let fail = |msg: String| Row {
        slices_req: slices,
        preset: preset.0,
        observed_slices: None,
        suspect: false,
        p50_us: 0.0,
        p95_us: 0.0,
        mean_us: 0.0,
        fps_equiv: 0.0,
        error: Some(msg),
    };

    let mut ctx = match unsafe { open_encoder(slices, preset) } {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let mut frame = match unsafe { alloc_frame() } {
        Ok(f) => f,
        Err(e) => {
            unsafe { ff::avcodec_free_context(&mut ctx) };
            return fail(e);
        }
    };
    let mut pkt = unsafe { ff::av_packet_alloc() };
    if pkt.is_null() {
        unsafe {
            ff::av_frame_free(&mut frame);
            ff::avcodec_free_context(&mut ctx);
        }
        return fail("av_packet_alloc failed".into());
    }

    let mut durations_us: Vec<u64> = Vec::with_capacity(MEASURED_FRAMES as usize);
    let mut observed_counts: Vec<i32> = Vec::new();
    let mut run_err: Option<String> = None;

    let total = WARMUP_FRAMES + MEASURED_FRAMES;
    'frames: for i in 0..total {
        unsafe {
            let rc = ff::av_frame_make_writable(frame);
            if rc < 0 {
                run_err = Some(format!(
                    "av_frame_make_writable(frame {i}): {}",
                    err_str(rc)
                ));
                break 'frames;
            }
            fill_gradient(frame, i);
            (*frame).pts = i as i64;
        }

        let measuring = i >= WARMUP_FRAMES;
        let start = measuring.then(Instant::now);

        let rc = unsafe { ff::avcodec_send_frame(ctx, frame) };
        if rc < 0 {
            run_err = Some(format!("avcodec_send_frame(frame {i}): {}", err_str(rc)));
            break 'frames;
        }
        loop {
            let rc = unsafe { ff::avcodec_receive_packet(ctx, pkt) };
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
            if measuring {
                let data = unsafe { std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize) };
                observed_counts.push(count_vcl_nals(data));
            }
            unsafe { ff::av_packet_unref(pkt) };
        }
        if let Some(t0) = start {
            durations_us.push(t0.elapsed().as_micros() as u64);
        }
    }

    // Flush trailing packets (any frames still buffered internally). Not
    // part of the per-frame latency series — those are attributed to the
    // send that produced them — but they still count toward slice
    // verification.
    if run_err.is_none() {
        unsafe {
            let _ = ff::avcodec_send_frame(ctx, ptr::null());
            loop {
                let rc = ff::avcodec_receive_packet(ctx, pkt);
                if rc == ff::AVERROR(libc::EAGAIN) || rc == ff::AVERROR_EOF {
                    break;
                }
                if rc < 0 {
                    break;
                }
                let data = std::slice::from_raw_parts((*pkt).data, (*pkt).size as usize);
                observed_counts.push(count_vcl_nals(data));
                ff::av_packet_unref(pkt);
            }
        }
    }

    unsafe {
        ff::av_packet_free(&mut pkt);
        ff::av_frame_free(&mut frame);
        ff::avcodec_free_context(&mut ctx);
    }

    if let Some(err) = run_err {
        return fail(err);
    }
    if durations_us.is_empty() {
        return fail("no measured frames produced any timing sample".into());
    }

    durations_us.sort_unstable();
    let p50_us = percentile(&durations_us, 0.50);
    let p95_us = percentile(&durations_us, 0.95);
    let mean_us = durations_us.iter().sum::<u64>() as f64 / durations_us.len() as f64;
    let fps_equiv = 1_000_000.0 / mean_us;

    let observed_slices = mode(&observed_counts);
    let suspect = observed_slices != Some(slices);

    Row {
        slices_req: slices,
        preset: preset.0,
        observed_slices,
        suspect,
        p50_us,
        p95_us,
        mean_us,
        fps_equiv,
        error: None,
    }
}

fn print_table(rows: &[Row]) {
    println!(
        "| config | requested/observed slices | p50 (us) | p95 (us) | mean (us) | fps-equivalent |"
    );
    println!("|---|---|---|---|---|---|");
    for r in rows {
        let config = format!("slices={} preset={} tune=ull", r.slices_req, r.preset);
        if let Some(err) = &r.error {
            println!("| {config} | {}/FAILED | - | - | - | - |", r.slices_req);
            eprintln!("nvenc-slice-probe: {config}: {err}");
            continue;
        }
        let obs = r
            .observed_slices
            .map(|v| v.to_string())
            .unwrap_or_else(|| "?".into());
        let flag = if r.suspect { " SUSPECT" } else { "" };
        println!(
            "| {config} | {}/{obs}{flag} | {:.1} | {:.1} | {:.1} | {:.1} |",
            r.slices_req, r.p50_us, r.p95_us, r.mean_us, r.fps_equiv
        );
    }
}

fn print_verdict(rows: &[Row]) {
    let mut parts: Vec<String> = Vec::new();
    for (preset_name, _) in PRESETS {
        let Some(base) = rows
            .iter()
            .find(|r| r.preset == preset_name && r.slices_req == 1 && r.error.is_none())
        else {
            continue;
        };
        for &slices in &[2, 4] {
            if let Some(r) = rows
                .iter()
                .find(|r| r.preset == preset_name && r.slices_req == slices && r.error.is_none())
            {
                let delta = (r.p50_us - base.p50_us) / base.p50_us * 100.0;
                parts.push(format!("{preset_name}/slices={slices}: {delta:+.1}%"));
            }
        }
    }
    if parts.is_empty() {
        println!(
            "Verdict: insufficient successful configs to compare slices vs the slices=1 baseline."
        );
    } else {
        println!("Verdict: p50 delta vs slices=1 -> {}", parts.join(", "));
    }
}

fn main() {
    // Fail fast with one clear message instead of repeating the same
    // "encoder not found" error across every config.
    let codec_present =
        unsafe { !ff::avcodec_find_encoder_by_name(c"h264_nvenc".as_ptr()).is_null() };
    if !codec_present {
        eprintln!(
            "nvenc-slice-probe: h264_nvenc encoder not found in the linked libavcodec \
             (check third-party/ffmpeg — see tools/bootstrap-ffmpeg.ps1)"
        );
        std::process::exit(1);
    }

    let mut rows: Vec<Row> = Vec::new();
    let mut any_failed = false;
    for slices in SLICE_COUNTS {
        for preset in PRESETS {
            eprintln!(
                "nvenc-slice-probe: running slices={slices} preset={} tune=ull ...",
                preset.0
            );
            let row = unsafe { run_config(slices, preset) };
            any_failed |= row.error.is_some();
            rows.push(row);
        }
    }

    print_table(&rows);
    print_verdict(&rows);

    if any_failed {
        eprintln!(
            "nvenc-slice-probe: one or more configs failed to open/encode; see errors above."
        );
        std::process::exit(1);
    }
}
