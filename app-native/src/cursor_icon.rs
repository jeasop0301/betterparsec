//! Host cursor shape → local Win32 cursor — M4 cursor P2, native render
//! half (docs/design/cursor-channel.md §P2).
//!
//! The session stores the latest SHAPE message (RGBA PNG + hotspot) in
//! `client_transport::cursor::CursorShared`; the shell decodes it here,
//! builds an alpha `HCURSOR`, and the stream child's `WM_SETCURSOR`
//! applies it instead of plain `SetCursor(NULL)` (input.rs). Opt-in via
//! the in-app "Client-rendered cursor" toggle (settings store), with
//! `BP_CLIENT_CURSOR=1` as a dev override; the host still bakes the cursor
//! into the video by default (`capture_cursor`), and the default flip
//! follows the host-side `capture_cursor=false` live verdict (ROADMAP
//! cursor P2).
//!
//! PNG decode + BGRA swizzle are pure and unit-tested headless; the
//! `HCURSOR` build runs against real USER32/GDI (fine in headless CI
//! sessions, same as the present.rs swapchain tests).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};

use client_transport::cursor::CursorShared;

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyIcon, HICON, ICONINFO};
use windows::core::BOOL;

/// Decoded cursor image, BGRA8 top-down (DIB byte order).
pub struct ShapeImage {
    pub w: u32,
    pub h: u32,
    pub bgra: Vec<u8>,
}

/// Decode the host's SHAPE PNG (always RGBA8 — `cursor_tracker` encodes
/// nothing else) and swizzle to BGRA for the DIB. `None` on any decode
/// error or an unexpected pixel format; callers treat that as a permanent
/// miss for this shape id (no per-frame retry).
pub fn decode_png_bgra(png: &[u8]) -> Option<ShapeImage> {
    let decoder = png::Decoder::new(png);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    if (info.width as usize) * (info.height as usize) * 4 != buf.len() {
        return None;
    }
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2); // RGBA → BGRA
    }
    Some(ShapeImage {
        w: info.width,
        h: info.height,
        bgra: buf,
    })
}

/// Hotspots are host-declared and must stay inside the image
/// (`CreateIconIndirect` is tolerant, but a clamped value keeps the
/// click point sane on malformed input).
pub fn clamp_hotspot(hot: u16, extent: u32) -> u32 {
    u32::from(hot).min(extent.saturating_sub(1))
}

/// An owning `HCURSOR` wrapper — `DestroyIcon` on drop. The shell keeps a
/// short ring of these so a handle currently applied by the wndproc is
/// never destroyed while still in use (shapes change rarely; the ring
/// outlives any in-flight `SetCursor`).
pub struct OwnedCursor(isize);

// SAFETY: an HCURSOR is a process-global USER handle, not tied to the
// creating thread; the wrapper only stores the raw value.
unsafe impl Send for OwnedCursor {}

impl OwnedCursor {
    /// Raw handle value for the wndproc-shared [`ActiveCursor`] slot.
    pub fn handle(&self) -> isize {
        self.0
    }
}

impl Drop for OwnedCursor {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyIcon(HICON(self.0 as *mut core::ffi::c_void));
        }
    }
}

/// Build an alpha cursor from a decoded shape: 32bpp top-down DIB (the
/// alpha channel drives blending) + a zeroed monochrome AND mask.
pub fn build_cursor(img: &ShapeImage, hot_x: u16, hot_y: u16) -> Option<OwnedCursor> {
    if img.w == 0 || img.h == 0 || img.bgra.len() != (img.w as usize) * (img.h as usize) * 4 {
        return None;
    }
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: img.w as i32,
                biHeight: -(img.h as i32), // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        if bits.is_null() {
            let _ = DeleteObject(color.into());
            return None;
        }
        core::ptr::copy_nonoverlapping(img.bgra.as_ptr(), bits as *mut u8, img.bgra.len());

        // Monochrome AND mask, zeroed (CreateBitmap without bits leaves
        // the content uninitialized). Rows are word-aligned.
        let stride = ((img.w as usize).div_ceil(16)) * 2;
        let zeros = vec![0u8; stride * img.h as usize];
        let mask = CreateBitmap(
            img.w as i32,
            img.h as i32,
            1,
            1,
            Some(zeros.as_ptr() as *const core::ffi::c_void),
        );
        if mask.is_invalid() {
            let _ = DeleteObject(color.into());
            return None;
        }

        let info = ICONINFO {
            fIcon: BOOL(0), // FALSE = cursor (hotspot honoured)
            xHotspot: clamp_hotspot(hot_x, img.w),
            yHotspot: clamp_hotspot(hot_y, img.h),
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info);
        // The icon owns copies; the source bitmaps go regardless of outcome.
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon.ok().map(|h| OwnedCursor(h.0 as isize))
    }
}

/// Shell→wndproc shared slot: the cursor the stream child should show
/// over its client area (`0` = hide, the P1 behaviour). Written by the
/// shell pump every frame, read on `WM_SETCURSOR`/`WM_MOUSEMOVE`.
#[derive(Debug, Default)]
pub struct ActiveCursor(AtomicIsize);

impl ActiveCursor {
    pub fn set(&self, handle: isize) {
        self.0.store(handle, Ordering::Release);
    }

    pub fn get(&self) -> isize {
        self.0.load(Ordering::Acquire)
    }
}

/// Per-connection client-cursor state owned by the shell: builds an
/// HCURSOR once per shape id and publishes the current handle to the
/// wndproc-shared [`ActiveCursor`] slot.
#[derive(Default)]
pub struct ClientCursor {
    active: Arc<ActiveCursor>,
    /// Recently built cursors, newest last. Depth 4: a handle the wndproc
    /// may still have applied is never destroyed while in use (shapes
    /// change rarely; the ring outlives any in-flight `SetCursor`).
    ring: VecDeque<OwnedCursor>,
    /// Last shape id a build was attempted for (hit or permanent miss —
    /// a failed decode/build is not retried every frame).
    built_id: u32,
}

impl ClientCursor {
    /// The slot the stream child's `InputCtx` reads on `WM_SETCURSOR`.
    pub fn active_slot(&self) -> Arc<ActiveCursor> {
        self.active.clone()
    }

    /// Publish the newest host cursor shape: build on shape-id change,
    /// then set the slot from visibility + newest handle.
    pub fn pump(&mut self, cs: &CursorShared) {
        let want = cs.shape_id();
        if want != 0
            && want != self.built_id
            && let Some(shape) = cs.shape()
            && shape.shape_id == want
        {
            self.built_id = want; // attempted — hit or permanent miss
            match decode_png_bgra(&shape.png)
                .and_then(|img| build_cursor(&img, shape.hot_x, shape.hot_y))
            {
                Some(cur) => {
                    self.ring.push_back(cur);
                    while self.ring.len() > 4 {
                        drop(self.ring.pop_front());
                    }
                }
                None => {
                    tracing::warn!(shape_id = want, "cursor shape decode/build failed");
                }
            }
        }
        let handle = if cs.visible() {
            self.ring.back().map_or(0, OwnedCursor::handle)
        } else {
            0
        };
        self.active.set(handle);
    }

    /// Drop per-connection state (disconnect/reconnect): hide first so
    /// the wndproc can no longer apply a handle the ring is about to
    /// destroy.
    pub fn reset(&mut self) {
        self.active.set(0);
        self.ring.clear();
        self.built_id = 0;
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn encode_png(w: u32, h: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(color);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().expect("png header");
            writer.write_image_data(data).expect("png data");
        }
        out
    }

    #[test]
    fn decode_swizzles_rgba_to_bgra() {
        // 2x1: red (opaque), half-transparent blue.
        let rgba = [0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0x80];
        let png = encode_png(2, 1, png::ColorType::Rgba, &rgba);
        let img = decode_png_bgra(&png).expect("decodes");
        assert_eq!((img.w, img.h), (2, 1));
        assert_eq!(
            img.bgra,
            vec![0x00, 0x00, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x80]
        );
    }

    #[test]
    fn decode_rejects_non_rgba() {
        let png = encode_png(2, 1, png::ColorType::Grayscale, &[0x10, 0x20]);
        assert!(decode_png_bgra(&png).is_none());
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_png_bgra(&[0xAA, 0xBB, 0xCC]).is_none());
        assert!(decode_png_bgra(&[]).is_none());
    }

    #[test]
    fn hotspot_clamps_inside_the_image() {
        assert_eq!(clamp_hotspot(3, 32), 3);
        assert_eq!(clamp_hotspot(40, 32), 31);
        assert_eq!(clamp_hotspot(0, 0), 0);
    }

    #[test]
    fn active_cursor_roundtrip() {
        let a = ActiveCursor::default();
        assert_eq!(a.get(), 0);
        a.set(42);
        assert_eq!(a.get(), 42);
        a.set(0);
        assert_eq!(a.get(), 0);
    }

    /// Real USER32/GDI roundtrip: build an alpha cursor, read its hotspot
    /// back via GetIconInfo, destroy on drop.
    #[test]
    fn build_cursor_roundtrips_hotspot() {
        use windows::Win32::UI::WindowsAndMessaging::GetIconInfo;

        let img = ShapeImage {
            w: 2,
            h: 2,
            bgra: vec![0u8; 16],
        };
        let cur = build_cursor(&img, 1, 40).expect("builds");
        assert_ne!(cur.handle(), 0);

        let mut info = ICONINFO::default();
        unsafe {
            GetIconInfo(HICON(cur.handle() as *mut core::ffi::c_void), &mut info)
                .expect("icon info");
        }
        assert_eq!(info.fIcon, BOOL(0)); // cursor, not icon
        assert_eq!(info.xHotspot, 1);
        assert_eq!(info.yHotspot, 1); // clamped from 40 to h-1
        unsafe {
            let _ = DeleteObject(info.hbmMask.into());
            let _ = DeleteObject(info.hbmColor.into());
        }
    }

    #[test]
    fn build_cursor_rejects_mismatched_dimensions() {
        let img = ShapeImage {
            w: 4,
            h: 4,
            bgra: vec![0u8; 8], // not 4*4*4
        };
        assert!(build_cursor(&img, 0, 0).is_none());
        let empty = ShapeImage {
            w: 0,
            h: 0,
            bgra: vec![],
        };
        assert!(build_cursor(&empty, 0, 0).is_none());
    }

    fn shared_with(shape_id: u32, png: Vec<u8>, visible: bool) -> CursorShared {
        use client_transport::cursor::{CursorPos, CursorShape};
        let cs = CursorShared::default();
        cs.store_shape(CursorShape {
            shape_id,
            w: 2,
            h: 1,
            hot_x: 0,
            hot_y: 0,
            png,
        });
        cs.store(CursorPos {
            visible,
            x: 0,
            y: 0,
            vw: 1920,
            vh: 1080,
            shape_id,
        });
        cs
    }

    /// Full pump path against a real HCURSOR build: shape → handle,
    /// hidden → 0, restored on visible without a rebuild.
    #[test]
    fn pump_publishes_and_hides() {
        use client_transport::cursor::CursorPos;

        let rgba = [0xFF, 0x00, 0x00, 0xFF, 0x00, 0x00, 0xFF, 0x80];
        let png = encode_png(2, 1, png::ColorType::Rgba, &rgba);
        let cs = shared_with(5, png, true);

        let mut cc = ClientCursor::default();
        let slot = cc.active_slot();
        cc.pump(&cs);
        let handle = slot.get();
        assert_ne!(handle, 0, "visible shape publishes a handle");

        cs.store(CursorPos {
            visible: false,
            x: 0,
            y: 0,
            vw: 1920,
            vh: 1080,
            shape_id: 5,
        });
        cc.pump(&cs);
        assert_eq!(slot.get(), 0, "host-hidden hides");

        cs.store(CursorPos {
            visible: true,
            x: 0,
            y: 0,
            vw: 1920,
            vh: 1080,
            shape_id: 5,
        });
        cc.pump(&cs);
        assert_eq!(slot.get(), handle, "same shape id reuses the built handle");

        cc.reset();
        assert_eq!(slot.get(), 0, "reset hides");
    }

    /// A bad PNG is a permanent miss for its shape id — no per-frame
    /// retry, slot stays 0 (P1 hide posture).
    #[test]
    fn pump_bad_png_is_a_permanent_miss() {
        let cs = shared_with(9, vec![0xDE, 0xAD], true);
        let mut cc = ClientCursor::default();
        cc.pump(&cs);
        cc.pump(&cs); // second pump must not re-attempt the build
        assert_eq!(cc.active_slot().get(), 0);
        assert_eq!(cc.built_id, 9);
        assert!(cc.ring.is_empty());
    }
}
