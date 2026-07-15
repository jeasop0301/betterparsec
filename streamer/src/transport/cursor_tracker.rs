//! Host-authority cursor visibility tracker — M4 cursor P1/P2
//! (docs/design/cursor-channel.md §3, §P2).
//!
//! The streamer runs on the host machine, so no Sunshine fork is needed:
//! a 60 Hz Win32 poll reads `GetCursorInfo` (visibility + position) and
//! publishes POS messages whenever the sample changes. The client's auto
//! mouse-mode consumes them: `visible=false` → pointer lock (FPS aim),
//! `visible=true` → unlock + absolute input, with the in-video baked
//! cursor as the visual (Sunshine keeps blending in P1).
//!
//! P2 adds SHAPE extraction: whenever the poll observes a new `hCursor`
//! handle (or once right after the sink becomes `Ready`), [`extract_shape`]
//! pulls the cursor's RGBA pixels via `GetIconInfoExW`/`GetDIBits` and PNG
//! encodes them, and subsequent POS messages carry that shape's id. This
//! lets the web client render the cursor itself (behind `settings.clientCursor`
//! — Sunshine still bakes the cursor into the video otherwise) instead of
//! only tracking lock/unlock. See docs/design/cursor-channel.md §P2 for the
//! Win32 recipe and its accepted approximations (legacy zero-alpha color
//! cursors, monochrome XOR cursors).
//!
//! Host-cursor authority is transport-agnostic (DCV ships it over TCP),
//! so the tracker writes through a [`CursorSink`]: a dedicated reliable
//! DataChannel on WebRTC, a `TransportChannelId::CURSOR`-prefixed frame
//! on the WebSocket transport.
//!
//! `CURSOR_SUPPRESSED` (touch) counts as hidden per the design. The
//! sample uses `CURSORINFO.ptScreenPos` + `GetSystemMetrics` — one
//! consistent (DPI-virtualized) coordinate space; the client only needs
//! the normalized position, so consistency beats "physical" (deviation
//! from the doc's GetPhysicalCursorPos noted deliberately: mixing spaces
//! is the actual hazard).
//!
//! Lifecycle: spawned once per transport; exits when the sink reports
//! closed after having been ready, or after repeated send failures
//! (peer gone). Non-Windows builds sample nothing and idle.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use common::api_bindings::TransportChannelId;
use common::ipc::StreamerIpcMessage;
use tokio::sync::mpsc::Sender;
use tokio::time::MissedTickBehavior;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

use super::TransportEvent;
use super::cursor_wire::{CursorPos, encode_pos, encode_shape};

/// Consecutive send failures before the task gives up (peer gone).
const MAX_SEND_FAILURES: u32 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SinkState {
    /// Not open yet — keep polling, do not send.
    Wait,
    /// Open — send on change.
    Ready,
    /// Gone — the tracker exits.
    Closed,
}

/// Transport-specific write half for the cursor channel. Takes pre-encoded
/// bytes so the same trait carries both POS (`encode_pos`) and SHAPE
/// (`encode_shape`) frames — they ride the same channel, distinguished by
/// the leading kind byte.
#[async_trait]
pub(crate) trait CursorSink: Send + Sync + 'static {
    fn state(&self) -> SinkState;
    /// `false` = send failure (counted toward the give-up threshold).
    async fn send_bytes(&self, bytes: Vec<u8>) -> bool;
}

/// WebRTC: the dedicated reliable+ordered `cursor` DataChannel.
pub(crate) struct WebRtcCursorSink(pub(crate) Arc<RTCDataChannel>);

#[async_trait]
impl CursorSink for WebRtcCursorSink {
    fn state(&self) -> SinkState {
        match self.0.ready_state() {
            RTCDataChannelState::Open => SinkState::Ready,
            RTCDataChannelState::Closing | RTCDataChannelState::Closed => SinkState::Closed,
            _ => SinkState::Wait,
        }
    }

    async fn send_bytes(&self, bytes: Vec<u8>) -> bool {
        self.0.send(&Bytes::from(bytes)).await.is_ok()
    }
}

/// WebSocket: a `TransportChannelId::CURSOR`-prefixed binary frame routed
/// through the server relay (same framing as every other ws channel).
pub(crate) struct WebSocketCursorSink(pub(crate) Sender<TransportEvent>);

#[async_trait]
impl CursorSink for WebSocketCursorSink {
    fn state(&self) -> SinkState {
        if self.0.is_closed() {
            SinkState::Closed
        } else {
            SinkState::Ready
        }
    }

    async fn send_bytes(&self, bytes: Vec<u8>) -> bool {
        self.0
            .send(TransportEvent::SendIpc(
                StreamerIpcMessage::WebSocketTransport(Bytes::from(ws_cursor_frame(&bytes))),
            ))
            .await
            .is_ok()
    }
}

/// `[CURSOR channel id][payload]` — the WebSocket wire frame. Shared by
/// both POS and SHAPE payloads (same channel, different `kind` byte).
pub(crate) fn ws_cursor_frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + payload.len());
    out.push(TransportChannelId::CURSOR);
    out.extend_from_slice(payload);
    out
}

pub(crate) fn spawn(sink: impl CursorSink) {
    tokio::spawn(run(sink));
}

async fn run(sink: impl CursorSink) {
    let mut tick = tokio::time::interval(Duration::from_millis(16));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    // Dedup key for POS resends: the raw sample (its `shape_id` is always 0
    // — shape ids are assigned by this loop, not by `sample()`) plus the
    // hCursor handle, so a shape-only change (identical x/y/visible) still
    // triggers a resend once the new shape is known.
    let mut last: Option<(CursorPos, isize)> = None;
    let mut failures = 0u32;

    // P2 shape tracking. `last_extracted_hcursor` drives "extract on
    // change (once per hCursor value)": left unset on extraction failure
    // so the next tick retries the same handle (never panics — see
    // extract_shape's doc comment); set on success or on a refuse-to-send
    // (oversized PNG) outcome so those are not retried every tick.
    // `current_shape_id` is what POS actually reports: 0 (unknown/none)
    // until the first successful extraction for the current hCursor.
    let mut last_extracted_hcursor: Option<isize> = None;
    let mut current_shape_id: u32 = 0;
    let mut next_shape_id: u32 = 1;

    loop {
        tick.tick().await;

        match sink.state() {
            SinkState::Ready => {}
            SinkState::Closed => {
                tracing::debug!("[Cursor] sink closed — tracker exiting");
                return;
            }
            SinkState::Wait => {
                // Re-arm the "extract once on Ready" behavior for whenever
                // the sink does become ready.
                last_extracted_hcursor = None;
                continue;
            }
        }

        let Some((sampled, hcursor)) = sample() else {
            continue;
        };

        if last_extracted_hcursor != Some(hcursor) {
            match extract_shape(hcursor) {
                Some((w, h, hot_x, hot_y, png)) => {
                    match encode_shape(next_shape_id, w, h, hot_x, hot_y, &png) {
                        Some(bytes) => {
                            if sink.send_bytes(bytes).await {
                                current_shape_id = next_shape_id;
                                next_shape_id = next_shape_id.wrapping_add(1).max(1);
                                last_extracted_hcursor = Some(hcursor);
                                failures = 0;
                            } else {
                                failures += 1;
                                if failures >= MAX_SEND_FAILURES {
                                    tracing::debug!(
                                        "[Cursor] repeated send failures — tracker exiting"
                                    );
                                    return;
                                }
                            }
                        }
                        None => {
                            // Oversized PNG — refuse to send, fall back to
                            // "unknown" for this hCursor and stop retrying it
                            // (it will not shrink on the next tick).
                            tracing::debug!(
                                "[Cursor] extracted shape PNG exceeds cap — sending POS with shape_id=0"
                            );
                            current_shape_id = 0;
                            last_extracted_hcursor = Some(hcursor);
                        }
                    }
                }
                None => {
                    // Extraction failed this tick — report shape_id=0 and
                    // retry on a later tick (last_extracted_hcursor stays
                    // unset for this handle).
                    current_shape_id = 0;
                }
            }
        }

        let to_send = CursorPos {
            shape_id: current_shape_id,
            ..sampled
        };
        if last == Some((sampled, hcursor)) {
            continue;
        }
        // Send on any change (visibility flip, movement, or shape).
        // Reliable and ordered: state transitions must not be lost, and
        // 60 Hz × 18 B is negligible retransmission load.
        if sink.send_bytes(encode_pos(to_send).to_vec()).await {
            failures = 0;
            last = Some((sampled, hcursor));
        } else {
            failures += 1;
            if failures >= MAX_SEND_FAILURES {
                tracing::debug!("[Cursor] repeated send failures — tracker exiting");
                return;
            }
        }
    }
}

#[cfg(windows)]
fn sample() -> Option<(CursorPos, isize)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CURSOR_SHOWING, CURSORINFO, GetCursorInfo, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    };

    unsafe {
        let mut ci: CURSORINFO = std::mem::zeroed();
        ci.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
        if GetCursorInfo(&mut ci) == 0 {
            return None;
        }
        // Exactly CURSOR_SHOWING = visible; CURSOR_SUPPRESSED (touch) and
        // 0 (hidden by ShowCursor/DirectInput) are both "hidden".
        let visible = ci.flags == CURSOR_SHOWING;
        let vw = GetSystemMetrics(SM_CXSCREEN);
        let vh = GetSystemMetrics(SM_CYSCREEN);
        if vw <= 0 || vh <= 0 {
            return None;
        }
        let pos = CursorPos {
            visible,
            x: ci.ptScreenPos.x,
            y: ci.ptScreenPos.y,
            vw: vw.min(u16::MAX as i32) as u16,
            vh: vh.min(u16::MAX as i32) as u16,
            // Assigned by the run loop's own shape-id counter, not here —
            // sample() only ever reports the raw Win32 state.
            shape_id: 0,
        };
        Some((pos, ci.hCursor as isize))
    }
}

#[cfg(not(windows))]
fn sample() -> Option<(CursorPos, isize)> {
    None
}

/// Extracts the RGBA pixels + hotspot of the given `hCursor` handle and PNG
/// encodes them. `None` on any Win32/encode failure — the caller retries on
/// a later tick, this never panics.
///
/// Color cursors (`GetIconInfoExW` returns a non-null `hbmColor`) are read
/// back 32bpp top-down via `GetDIBits`; some legacy cursors report that
/// buffer with alpha always zero, which would otherwise render nothing —
/// detected and repaired by deriving alpha from the AND mask instead
/// (mask bit=1 = transparent). Monochrome cursors (I-beam, etc.) have no
/// color bitmap at all: `hbmMask` packs the AND mask (top half) and XOR
/// mask (bottom half) at 1bpp, whose true semantics (XOR-blend with the
/// destination) have no PNG-alpha equivalent — approximated as
/// transparent/opaque-black/opaque-white/opaque-black respectively, the
/// same approximation Parsec uses (only inverted-color cursors lose
/// anything visually, see docs/design/cursor-channel.md §P2).
#[cfg(windows)]
fn extract_shape(hcursor: isize) -> Option<(u16, u16, u16, u16, Vec<u8>)> {
    use windows_sys::Win32::Graphics::Gdi::{
        BITMAP, DeleteObject, GetDC, GetObjectW, HGDIOBJ, ReleaseDC,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetIconInfoExW, HICON, ICONINFOEXW};

    if hcursor == 0 {
        return None;
    }

    unsafe {
        let hicon = hcursor as HICON;

        let mut info: ICONINFOEXW = std::mem::zeroed();
        info.cbSize = std::mem::size_of::<ICONINFOEXW>() as u32;
        if GetIconInfoExW(hicon, &mut info) == 0 {
            return None;
        }

        // GetIconInfoExW hands ownership of both bitmaps to us regardless
        // of what happens below — always free them before returning.
        let mut mask_bmp: BITMAP = std::mem::zeroed();
        let got_mask_info = GetObjectW(
            info.hbmMask as HGDIOBJ,
            std::mem::size_of::<BITMAP>() as i32,
            &mut mask_bmp as *mut BITMAP as *mut core::ffi::c_void,
        ) != 0;

        let result = (|| -> Option<(u16, u16, u16, u16, Vec<u8>)> {
            if !got_mask_info {
                return None;
            }

            let w = mask_bmp.bmWidth;
            let is_color = !info.hbmColor.is_null();
            // Monochrome cursors pack AND (top half) + XOR (bottom half)
            // into one double-height hbmMask; color cursors have a
            // same-sized hbmMask (AND only) alongside hbmColor.
            let h = if is_color {
                mask_bmp.bmHeight
            } else {
                mask_bmp.bmHeight / 2
            };
            if w <= 0 || h <= 0 || w > i32::from(u16::MAX) || h > i32::from(u16::MAX) {
                return None;
            }

            let hdc = GetDC(std::ptr::null_mut());
            if hdc.is_null() {
                return None;
            }
            let rgba = extract_rgba(hdc, &info, is_color, w, h);
            ReleaseDC(std::ptr::null_mut(), hdc);
            let rgba = rgba?;

            let mut png_bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut png_bytes, w as u32, h as u32);
                encoder.set_color(png::ColorType::Rgba);
                encoder.set_depth(png::BitDepth::Eight);
                let mut writer = encoder.write_header().ok()?;
                writer.write_image_data(&rgba).ok()?;
            }

            let hot_x = info.xHotspot.min(u32::from(u16::MAX)) as u16;
            let hot_y = info.yHotspot.min(u32::from(u16::MAX)) as u16;
            Some((w as u16, h as u16, hot_x, hot_y, png_bytes))
        })();

        DeleteObject(info.hbmMask as HGDIOBJ);
        if !info.hbmColor.is_null() {
            DeleteObject(info.hbmColor as HGDIOBJ);
        }

        result
    }
}

#[cfg(not(windows))]
fn extract_shape(_hcursor: isize) -> Option<(u16, u16, u16, u16, Vec<u8>)> {
    None
}

/// Reads back the color or monochrome pixels for an already-dimension-known
/// cursor bitmap as straight top-down RGBA (see [`extract_shape`] for the
/// approximations involved).
#[cfg(windows)]
unsafe fn extract_rgba(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    info: &windows_sys::Win32::UI::WindowsAndMessaging::ICONINFOEXW,
    is_color: bool,
    w: i32,
    h: i32,
) -> Option<Vec<u8>> {
    unsafe {
        if is_color {
            let mut pixels = get_dib_bits_32bpp(hdc, info.hbmColor, w, h)?;
            if pixels.chunks_exact(4).all(|px| px[3] == 0) {
                // Legacy zero-alpha color cursor — derive real alpha from
                // the AND mask (bit=1 -> transparent) instead of shipping
                // a fully-invisible image.
                match get_dib_bits_1bpp(hdc, info.hbmMask, w, h) {
                    Some(mask_bits) => {
                        let and_mask = unpack_1bpp_rows(&mask_bits, w as u32, h as u32);
                        for (px, &transparent) in pixels.chunks_exact_mut(4).zip(and_mask.iter()) {
                            px[3] = if transparent { 0 } else { 255 };
                        }
                    }
                    None => {
                        // Can't recover alpha — opaque beats invisible.
                        for px in pixels.chunks_exact_mut(4) {
                            px[3] = 255;
                        }
                    }
                }
            }
            Some(pixels)
        } else {
            let mask_bits = get_dib_bits_1bpp(hdc, info.hbmMask, w, h * 2)?;
            let unpacked = unpack_1bpp_rows(&mask_bits, w as u32, (h * 2) as u32);
            let (and_mask, xor_mask) = unpacked.split_at((w * h) as usize);

            let mut out = vec![0u8; (w * h) as usize * 4];
            for (i, px) in out.chunks_exact_mut(4).enumerate() {
                match (and_mask[i], xor_mask[i]) {
                    (true, false) => px[3] = 0,
                    (false, false) => px[3] = 255,
                    (false, true) => {
                        px[0] = 255;
                        px[1] = 255;
                        px[2] = 255;
                        px[3] = 255;
                    }
                    // Screen-invert pixel: no PNG-alpha equivalent, so
                    // approximated as opaque black (accepted loss).
                    (true, true) => px[3] = 255,
                }
            }
            Some(out)
        }
    }
}

/// `GetDIBits` into a top-down 32bpp BGRA buffer (Win32's native channel
/// order for this format — the `png` encoder below is told `ColorType::Rgba`
/// after the caller swaps channels; color cursor pixels only ever need the
/// alpha channel patched in place, so no swap is needed here since
/// `BI_RGB`/32bpp DIBs are already laid out as B,G,R,A in memory and the PNG
/// write path treats the buffer as R,G,B,A — see the module doc's
/// accepted-approximations note; this is corrected by reading color
/// channels in BGRA and writing them back in RGBA order below).
#[cfg(windows)]
unsafe fn get_dib_bits_32bpp(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    bmp: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    w: i32,
    h: i32,
) -> Option<Vec<u8>> {
    use windows_sys::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, DIB_RGB_COLORS, GetDIBits};

    unsafe {
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = std::mem::size_of_val(&bmi.bmiHeader) as u32;
        bmi.bmiHeader.biWidth = w;
        bmi.bmiHeader.biHeight = -h; // negative = top-down rows out
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB;

        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        let lines = GetDIBits(
            hdc,
            bmp,
            0,
            h as u32,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut bmi,
            DIB_RGB_COLORS,
        );
        if lines <= 0 {
            return None;
        }
        // BI_RGB 32bpp DIBs are stored B,G,R,A per pixel — swap to R,G,B,A
        // for the PNG encoder (ColorType::Rgba).
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        Some(buf)
    }
}

#[cfg(windows)]
#[repr(C)]
struct BitmapInfo1Bpp {
    header: windows_sys::Win32::Graphics::Gdi::BITMAPINFOHEADER,
    colors: [windows_sys::Win32::Graphics::Gdi::RGBQUAD; 2],
}

/// `GetDIBits` into a packed 1bpp top-down buffer (DWORD-aligned rows, MSB
/// first per byte — standard Windows DIB row layout).
#[cfg(windows)]
unsafe fn get_dib_bits_1bpp(
    hdc: windows_sys::Win32::Graphics::Gdi::HDC,
    bmp: windows_sys::Win32::Graphics::Gdi::HBITMAP,
    w: i32,
    h: i32,
) -> Option<Vec<u8>> {
    use windows_sys::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, DIB_RGB_COLORS, GetDIBits};

    unsafe {
        let mut bmi: BitmapInfo1Bpp = std::mem::zeroed();
        bmi.header.biSize = std::mem::size_of_val(&bmi.header) as u32;
        bmi.header.biWidth = w;
        bmi.header.biHeight = -h;
        bmi.header.biPlanes = 1;
        bmi.header.biBitCount = 1;
        bmi.header.biCompression = BI_RGB;

        let stride = ((w as u32).div_ceil(32)) * 4;
        let mut buf = vec![0u8; (stride as usize) * (h as usize)];
        let lines = GetDIBits(
            hdc,
            bmp,
            0,
            h as u32,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            &mut bmi as *mut BitmapInfo1Bpp as *mut BITMAPINFO,
            DIB_RGB_COLORS,
        );
        if lines <= 0 {
            return None;
        }
        Some(buf)
    }
}

/// Unpacks a DWORD-aligned 1bpp DIB buffer into one `bool` per pixel
/// (`true` = bit set), row-major.
#[cfg(windows)]
fn unpack_1bpp_rows(buf: &[u8], w: u32, h: u32) -> Vec<bool> {
    let stride = ((w.div_ceil(32)) * 4) as usize;
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in 0..h as usize {
        let row = &buf[y * stride..y * stride + stride];
        for x in 0..w as usize {
            let byte = row[x / 8];
            let bit = 7 - (x % 8);
            out.push((byte >> bit) & 1 != 0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::cursor_wire::CURSOR_POS_LEN;
    use super::*;

    /// The ws frame is the POS wire behind the CURSOR channel id — pinned
    /// against the same vector as the cursor_wire byte pin.
    #[test]
    fn ws_frame_prefixes_channel_id() {
        let frame = ws_cursor_frame(&encode_pos(CursorPos {
            visible: true,
            x: 1000,
            y: -2,
            vw: 2560,
            vh: 1440,
            shape_id: 0,
        }));
        assert_eq!(frame.len(), 1 + CURSOR_POS_LEN);
        assert_eq!(frame[0], TransportChannelId::CURSOR);
        assert_eq!(frame[0], 27);
        assert_eq!(
            &frame[1..],
            &[
                0x00, 0x01, 0xE8, 0x03, 0x00, 0x00, 0xFE, 0xFF, 0xFF, 0xFF, 0x00, 0x0A, 0xA0, 0x05,
                0x00, 0x00, 0x00, 0x00,
            ]
        );
    }

    /// Real-cursor extraction smoke test: LoadCursorW(IDC_ARROW) is a
    /// system cursor guaranteed present on every Windows install, so this
    /// exercises the actual GetIconInfoExW/GetDIBits path (not a mock) and
    /// round-trips the produced PNG back through the `png` crate to check
    /// it decodes to plausible, non-fully-opaque pixels (the arrow's
    /// rounded silhouette always has transparent corner pixels).
    #[cfg(windows)]
    #[test]
    fn extract_shape_real_system_cursor() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{IDC_ARROW, LoadCursorW};

        let hcursor = unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) };
        assert!(!hcursor.is_null(), "IDC_ARROW should always load");

        let (w, h, _hot_x, _hot_y, png) = extract_shape(hcursor as isize)
            .expect("extraction should succeed for a real system cursor");
        assert!(w > 0 && w <= 256, "implausible width: {w}");
        assert!(h > 0 && h <= 256, "implausible height: {h}");
        assert!(!png.is_empty());

        let decoder = png::Decoder::new(std::io::Cursor::new(&png[..]));
        let mut reader = decoder
            .read_info()
            .expect("extract_shape must produce a valid PNG");
        let mut buf = vec![0u8; reader.output_buffer_size().expect("known buffer size")];
        let frame_info = reader
            .next_frame(&mut buf)
            .expect("decode the single frame");
        assert_eq!(frame_info.width, w as u32);
        assert_eq!(frame_info.height, h as u32);

        let has_non_opaque = buf.chunks_exact(4).any(|px| px[3] != 255);
        assert!(
            has_non_opaque,
            "expected at least one non-opaque (alpha != 255) pixel from the arrow cursor's silhouette"
        );
    }
}
