//! `cursor` DataChannel wire decode + session→shell readback — M4 cursor
//! P1/P2 (docs/design/cursor-channel.md §3, §P2).
//!
//! POS message, little-endian, 14 bytes (v1) or 18 bytes (v2, P2 —
//! `shape_id` appended; v1 frames decode with `shape_id = 0`):
//! `u8 kind=0 | u8 visible(0/1) | i32 x | i32 y | u16 vw | u16 vh [| u32 shape_id]`
//!
//! `x`,`y` are cursor screen coordinates and `vw`,`vh` the captured
//! monitor's size in the same coordinate space. `shape_id` is 0 for
//! "unknown/none" and otherwise refers to the last SHAPE message with
//! that id.
//!
//! SHAPE message, little-endian, 17-byte header + PNG payload:
//! `u8 kind=1 | u32 shape_id | u16 w | u16 h | u16 hot_x | u16 hot_y | u32 png_len | png bytes`
//! The host sends SHAPE only when the cursor shape (hCursor) changes and
//! assigns a **fresh id per change** (`cursor_tracker`), so the latest
//! shape is always complete state — a single-slot store suffices here.
//!
//! Mirror: `streamer/src/transport/cursor_wire.rs` (encode) and
//! `web/stream/cursor_wire.ts` (parse) — keep the byte-pinned tests in
//! lockstep across all three (qu_wire pattern). This crate only decodes
//! (the client never encodes; the host is the sole cursor authority).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use common::desktop_control::cursor::CursorOwner;
use common::desktop_control::{ControlGeneration, OwnershipLease};

pub const CURSOR_KIND_POS: u8 = 0;
pub const CURSOR_KIND_SHAPE: u8 = 1;
/// Minimum (v1) POS frame length; v2 appends a u32 `shape_id`.
pub const CURSOR_POS_LEN: usize = 14;
pub const CURSOR_POS_V2_LEN: usize = 18;
/// Fixed portion of a SHAPE frame, before the variable-length PNG payload.
pub const CURSOR_SHAPE_HEADER_LEN: usize = 17;
/// Refuse to accept a PNG payload bigger than this — bounds a single
/// cursor shape frame regardless of source cursor size (encode-side twin
/// lives in `streamer/src/transport/cursor_wire.rs`).
pub const CURSOR_SHAPE_MAX_PNG_LEN: usize = 262_144;

/// One cursor state sample (host authority: visibility + position + the
/// shape currently in effect; `shape_id == 0` = unknown/none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPos {
    pub visible: bool,
    pub x: i32,
    pub y: i32,
    pub vw: u16,
    pub vh: u16,
    pub shape_id: u32,
}

/// Decode one `cursor` DataChannel message. Returns `None` for buffers
/// shorter than the POS frame or an unrecognised `kind` byte (forward
/// compatible with future message kinds this client doesn't understand
/// yet).
pub fn decode_pos(bytes: &[u8]) -> Option<CursorPos> {
    if bytes.is_empty() || bytes[0] != CURSOR_KIND_POS {
        return None;
    }
    if bytes.len() < CURSOR_POS_LEN {
        return None;
    }
    let shape_id = if bytes.len() >= CURSOR_POS_V2_LEN {
        u32::from_le_bytes(bytes[14..18].try_into().expect("4-byte slice"))
    } else {
        0
    };
    Some(CursorPos {
        visible: bytes[1] != 0,
        x: i32::from_le_bytes(bytes[2..6].try_into().expect("4-byte slice")),
        y: i32::from_le_bytes(bytes[6..10].try_into().expect("4-byte slice")),
        vw: u16::from_le_bytes(bytes[10..12].try_into().expect("2-byte slice")),
        vh: u16::from_le_bytes(bytes[12..14].try_into().expect("2-byte slice")),
        shape_id,
    })
}

/// One decoded SHAPE message: the host cursor image (RGBA PNG) plus its
/// hotspot. `w`/`h` are the host-declared pixel dimensions (informational
/// — the PNG itself is authoritative for decoders).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub shape_id: u32,
    pub w: u16,
    pub h: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub png: Vec<u8>,
}

/// Decode one SHAPE frame. `None` on a truncated header, a declared
/// `png_len` over [`CURSOR_SHAPE_MAX_PNG_LEN`], a body shorter than the
/// declared length, or an unrecognised kind. Trailing bytes beyond
/// `png_len` are tolerated (exactly `png_len` bytes are taken) — matches
/// `web/stream/cursor_wire.ts` `parseCursorMessage`.
pub fn decode_shape(bytes: &[u8]) -> Option<CursorShape> {
    if bytes.len() < CURSOR_SHAPE_HEADER_LEN || bytes[0] != CURSOR_KIND_SHAPE {
        return None;
    }
    let png_len = u32::from_le_bytes(bytes[13..17].try_into().expect("4-byte slice")) as usize;
    if png_len > CURSOR_SHAPE_MAX_PNG_LEN {
        return None;
    }
    if bytes.len() < CURSOR_SHAPE_HEADER_LEN + png_len {
        return None;
    }
    Some(CursorShape {
        shape_id: u32::from_le_bytes(bytes[1..5].try_into().expect("4-byte slice")),
        w: u16::from_le_bytes(bytes[5..7].try_into().expect("2-byte slice")),
        h: u16::from_le_bytes(bytes[7..9].try_into().expect("2-byte slice")),
        hot_x: u16::from_le_bytes(bytes[9..11].try_into().expect("2-byte slice")),
        hot_y: u16::from_le_bytes(bytes[11..13].try_into().expect("2-byte slice")),
        png: bytes[CURSOR_SHAPE_HEADER_LEN..CURSOR_SHAPE_HEADER_LEN + png_len].to_vec(),
    })
}

/// M4 cursor readback shared with the shell: latest host-reported
/// visibility + position, stored via plain atomics.
///
/// The session's `cursor` DataChannel callback stores directly into this
/// struct (see `session::create_peer`) instead of routing through the
/// `LocalEvent` loop: there is no ordering dependency on any other session
/// state (a stale-by-one-frame position is harmless, unlike e.g. FEC data
/// which must stay ordered with the rest of the receive pipeline), so a
/// bare atomic store from the WebRTC callback thread is both correct and
/// avoids an extra channel hop on the hot path of every mouse-move sample.
///
/// `pos` packs `x` and `y` (both `i32`) into one `u64`: high 32 bits = `x`,
/// low 32 bits = `y`, each reinterpreted through its unsigned bit pattern
/// (`i32 as u32`) so negative coordinates round-trip exactly.
///
/// `shape` (P2) is the latest decoded SHAPE message behind a `Mutex` —
/// unlike POS this is off the hot path (the host sends SHAPE only when
/// the cursor shape changes), so a lock is fine and the payload (`Vec`)
/// rules out plain atomics anyway.
#[derive(Debug)]
pub struct CursorShared {
    visible: AtomicBool,
    pos: AtomicU64,
    shape_id: AtomicU32,
    shape: Mutex<Option<std::sync::Arc<CursorShape>>>,
}

impl Default for CursorShared {
    /// Host cursor is assumed visible until the first POS sample says
    /// otherwise (matches the web `CursorAutoMode`'s starting posture).
    fn default() -> Self {
        Self {
            visible: AtomicBool::new(true),
            pos: AtomicU64::new(0),
            shape_id: AtomicU32::new(0),
            shape: Mutex::new(None),
        }
    }
}

impl CursorShared {
    /// Store one decoded POS sample (called from the data-channel
    /// callback thread — see the doc comment on this struct for why plain
    /// atomic stores are sufficient here).
    pub fn store(&self, p: CursorPos) {
        let prev = self.visible.swap(p.visible, Ordering::AcqRel);
        if prev != p.visible {
            // Transition-only, so this stays quiet at 60 Hz — live-debug
            // evidence for the host-authority mouse-mode auto-switch
            // (clip follows this flag; a stuck value cages the cursor).
            tracing::info!(visible = p.visible, "host cursor visibility changed");
        }
        let packed = ((p.x as u32 as u64) << 32) | (p.y as u32 as u64);
        self.pos.store(packed, Ordering::Release);
        self.shape_id.store(p.shape_id, Ordering::Release);
    }

    /// Host cursor visibility (shell readback for mouse-mode wiring).
    pub fn visible(&self) -> bool {
        self.visible.load(Ordering::Acquire)
    }

    /// Last reported host cursor position, `(x, y)`.
    pub fn pos(&self) -> (i32, i32) {
        let packed = self.pos.load(Ordering::Acquire);
        ((packed >> 32) as u32 as i32, packed as u32 as i32)
    }

    /// Shape id the last POS sample reported (0 = unknown/none).
    pub fn shape_id(&self) -> u32 {
        self.shape_id.load(Ordering::Acquire)
    }

    /// Store one decoded SHAPE message (data-channel callback thread).
    pub fn store_shape(&self, s: CursorShape) {
        *self.shape.lock().expect("cursor shape lock") = Some(std::sync::Arc::new(s));
    }

    /// Latest host cursor shape, if any has arrived this session.
    pub fn shape(&self) -> Option<std::sync::Arc<CursorShape>> {
        self.shape.lock().expect("cursor shape lock").clone()
    }
}

/// G010 client-render authority decision (pure, timer-free). Consumes the G009
/// desktop-control cursor-authority domain — a generation-stamped [`CursorOwner`]
/// admitted through an [`OwnershipLease`] — plus cursor shape ids, and decides
/// exactly one visual: the host-baked cursor OR a client-rendered cursor, never
/// both (no double cursor). It dedups shape application by id and restores the
/// last shape across a reconnect once client authority is re-established.
#[derive(Debug)]
pub struct ClientCursorAuthority {
    lease: OwnershipLease,
    owner: CursorOwner,
    latest_shape_id: u32,
    applied_shape_id: Option<u32>,
    visible: bool,
}

impl Default for ClientCursorAuthority {
    fn default() -> Self {
        // Host bakes the cursor until authority is negotiated to the client.
        Self {
            lease: OwnershipLease::new(),
            owner: CursorOwner::Host,
            latest_shape_id: 0,
            applied_shape_id: None,
            visible: true,
        }
    }
}

impl ClientCursorAuthority {
    /// Applies a generation-stamped authority message. Returns `true` when it was
    /// admitted (first claim / same-generation refresh / strictly newer), `false`
    /// when the generation is stale — in which case the owner is left unchanged.
    pub fn apply_authority(&mut self, generation: ControlGeneration, owner: CursorOwner) -> bool {
        if self.lease.admit(generation) {
            self.owner = owner;
            true
        } else {
            false
        }
    }

    /// The side that currently renders the cursor.
    pub fn owner(&self) -> CursorOwner {
        self.owner
    }

    /// Whether the client should render the cursor from shape messages.
    pub fn render_client(&self) -> bool {
        matches!(self.owner, CursorOwner::Client)
    }

    /// Whether the host-baked cursor is the visual (mutually exclusive with
    /// [`Self::render_client`] — this pair is the "no double cursor" invariant).
    pub fn host_baked(&self) -> bool {
        matches!(self.owner, CursorOwner::Host)
    }

    /// Records the latest cursor shape id (retained across reconnect so it can be
    /// restored). `0` means "none/unknown".
    pub fn note_shape(&mut self, shape_id: u32) {
        self.latest_shape_id = shape_id;
    }

    /// Records host cursor visibility.
    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    /// Host cursor visibility.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Returns the shape id the client should render now, or `None` when the host
    /// is baking, there is no shape, or the current shape was already applied
    /// (dedup). Calling it marks the returned id as applied.
    pub fn take_shape_change(&mut self) -> Option<u32> {
        if !self.render_client() || self.latest_shape_id == 0 {
            return None;
        }
        if self.applied_shape_id == Some(self.latest_shape_id) {
            return None;
        }
        self.applied_shape_id = Some(self.latest_shape_id);
        Some(self.latest_shape_id)
    }

    /// Reconnect: authority must be re-negotiated, so ownership reverts to the
    /// host-baked cursor (never leave a client cursor rendering while the host
    /// may also bake). The last shape id is retained and the applied marker is
    /// cleared so it re-renders (restoration) once the client re-claims authority.
    pub fn on_reconnect(&mut self) {
        self.lease.release();
        self.owner = CursorOwner::Host;
        self.applied_shape_id = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared byte pin with `streamer/src/transport/webrtc/cursor_wire.rs`
    /// and `tests/cursor_wire.test.mjs` — change all three or none.
    #[test]
    fn pos_byte_pin() {
        let bytes: [u8; CURSOR_POS_LEN] = [
            0x00, 0x01, // kind, visible
            0xE8, 0x03, 0x00, 0x00, // x = 1000
            0xFE, 0xFF, 0xFF, 0xFF, // y = -2
            0x00, 0x0A, // vw = 2560
            0xA0, 0x05, // vh = 1440
        ];
        let p = decode_pos(&bytes).expect("decodes");
        assert_eq!(
            p,
            CursorPos {
                visible: true,
                x: 1000,
                y: -2,
                vw: 2560,
                vh: 1440,
                shape_id: 0, // v1 frame — no shape_id on the wire
            }
        );
    }

    /// POS v2 (cursor P2) appends a u32 shape_id — shared byte pin with
    /// `streamer/src/transport/cursor_wire.rs` `pos_byte_pin` and
    /// `tests/cursor_wire.test.mjs`; change all three or none.
    #[test]
    fn pos_v2_byte_pin() {
        let mut bytes = vec![
            0x00, 0x01, // kind, visible
            0xE8, 0x03, 0x00, 0x00, // x = 1000
            0xFE, 0xFF, 0xFF, 0xFF, // y = -2
            0x00, 0x0A, // vw = 2560
            0xA0, 0x05, // vh = 1440
        ];
        bytes.extend_from_slice(&7u32.to_le_bytes()); // shape_id = 7
        let p = decode_pos(&bytes).expect("v2 decodes");
        assert_eq!(p.x, 1000);
        assert_eq!(p.vh, 1440);
        assert_eq!(p.shape_id, 7);
    }

    #[test]
    fn hidden_pin() {
        let bytes: [u8; CURSOR_POS_LEN] = [
            0x00, 0x00, // kind, visible = false
            0x00, 0x00, 0x00, 0x00, // x = 0
            0x00, 0x00, 0x00, 0x00, // y = 0
            0x80, 0x07, // vw = 1920
            0x38, 0x04, // vh = 1080
        ];
        let p = decode_pos(&bytes).expect("decodes");
        assert!(!p.visible);
        assert_eq!(p.x, 0);
        assert_eq!(p.y, 0);
        assert_eq!(p.vw, 1920);
        assert_eq!(p.vh, 1080);
    }

    #[test]
    fn empty_is_none() {
        assert_eq!(decode_pos(&[]), None);
    }

    #[test]
    fn unknown_kind_is_none() {
        let mut bytes = [0u8; CURSOR_POS_LEN];
        bytes[0] = 0x02; // no such kind is defined
        assert_eq!(decode_pos(&bytes), None);
    }

    #[test]
    fn short_buffer_is_none() {
        // 13 bytes: correct kind, one byte short of a full POS frame.
        let bytes = [0u8; CURSOR_POS_LEN - 1];
        assert_eq!(decode_pos(&bytes), None);
    }

    /// Shared byte pin with `streamer/src/transport/cursor_wire.rs`
    /// `shape_byte_pin` and `tests/cursor_wire.test.mjs` — change all
    /// three or none.
    #[test]
    fn shape_byte_pin() {
        let bytes = [
            0x01, // kind
            0x07, 0x00, 0x00, 0x00, // shape_id = 7
            0x20, 0x00, // w = 32
            0x20, 0x00, // h = 32
            0x03, 0x00, // hot_x = 3
            0x04, 0x00, // hot_y = 4
            0x03, 0x00, 0x00, 0x00, // png_len = 3
            0xAA, 0xBB, 0xCC, // png bytes
        ];
        let s = decode_shape(&bytes).expect("decodes");
        assert_eq!(
            s,
            CursorShape {
                shape_id: 7,
                w: 32,
                h: 32,
                hot_x: 3,
                hot_y: 4,
                png: vec![0xAA, 0xBB, 0xCC],
            }
        );
    }

    #[test]
    fn shape_rejects_png_len_over_cap() {
        let mut bytes = vec![0x01u8, 1, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0];
        bytes.extend_from_slice(&((CURSOR_SHAPE_MAX_PNG_LEN + 1) as u32).to_le_bytes());
        bytes.resize(CURSOR_SHAPE_HEADER_LEN + CURSOR_SHAPE_MAX_PNG_LEN + 1, 0);
        assert_eq!(decode_shape(&bytes), None);
    }

    #[test]
    fn shape_truncated_body_is_none() {
        // Header claims 3 png bytes but only 2 are present.
        let mut bytes = vec![0x01u8, 7, 0, 0, 0, 32, 0, 32, 0, 3, 0, 4, 0];
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&[0xAA, 0xBB]);
        assert_eq!(decode_shape(&bytes), None);
    }

    #[test]
    fn shape_truncated_header_is_none() {
        assert_eq!(decode_shape(&[0x01u8; CURSOR_SHAPE_HEADER_LEN - 1]), None);
        assert_eq!(decode_shape(&[]), None);
    }

    #[test]
    fn shape_trailing_bytes_are_tolerated() {
        // Matches web parse: exactly png_len bytes are taken.
        let mut bytes = vec![0x01u8, 7, 0, 0, 0, 32, 0, 32, 0, 3, 0, 4, 0];
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0xAA, 0xBB, 0xEE, 0xEE]); // 2 png + 2 trailing
        let s = decode_shape(&bytes).expect("decodes");
        assert_eq!(s.png, vec![0xAA, 0xBB]);
    }

    #[test]
    fn shape_wrong_kind_is_none() {
        let mut bytes = vec![0x00u8, 7, 0, 0, 0, 32, 0, 32, 0, 3, 0, 4, 0];
        bytes.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode_shape(&bytes), None);
    }

    #[test]
    fn shared_shape_store_readback() {
        let shared = CursorShared::default();
        assert_eq!(shared.shape_id(), 0);
        assert!(shared.shape().is_none());

        shared.store_shape(CursorShape {
            shape_id: 3,
            w: 32,
            h: 32,
            hot_x: 1,
            hot_y: 2,
            png: vec![1, 2, 3],
        });
        let s = shared.shape().expect("stored");
        assert_eq!(s.shape_id, 3);
        assert_eq!(s.png, vec![1, 2, 3]);

        // POS carries the id the shell should match against.
        shared.store(CursorPos {
            visible: true,
            x: 0,
            y: 0,
            vw: 1,
            vh: 1,
            shape_id: 3,
        });
        assert_eq!(shared.shape_id(), 3);
    }

    #[test]
    fn shared_store_readback_roundtrip() {
        let shared = CursorShared::default();
        assert!(shared.visible());
        assert_eq!(shared.pos(), (0, 0));

        shared.store(CursorPos {
            visible: false,
            x: -100,
            y: 2000,
            vw: 1920,
            vh: 1080,
            shape_id: 0,
        });
        assert!(!shared.visible());
        assert_eq!(shared.pos(), (-100, 2000));

        shared.store(CursorPos {
            visible: true,
            x: i32::MIN,
            y: i32::MAX,
            vw: 0,
            vh: 0,
            shape_id: 9,
        });
        assert!(shared.visible());
        assert_eq!(shared.pos(), (i32::MIN, i32::MAX));
    }

    #[test]
    fn cursor_authority_defaults_to_host_baked() {
        let auth = ClientCursorAuthority::default();
        assert_eq!(auth.owner(), CursorOwner::Host);
        assert!(auth.host_baked());
        assert!(!auth.render_client());
        // Exactly one visual is ever chosen (no double cursor).
        assert_ne!(auth.host_baked(), auth.render_client());
    }

    #[test]
    fn cursor_authority_generation_gates_ownership() {
        let mut auth = ClientCursorAuthority::default();
        assert!(auth.apply_authority(ControlGeneration(5), CursorOwner::Client));
        assert!(auth.render_client());
        assert!(!auth.host_baked());
        // A stale generation is rejected and leaves ownership unchanged.
        assert!(!auth.apply_authority(ControlGeneration(3), CursorOwner::Host));
        assert!(auth.render_client());
        // A strictly newer generation takes over.
        assert!(auth.apply_authority(ControlGeneration(6), CursorOwner::Host));
        assert!(auth.host_baked());
        assert_ne!(auth.host_baked(), auth.render_client());
    }

    #[test]
    fn cursor_authority_dedups_shape_and_gates_on_owner() {
        let mut auth = ClientCursorAuthority::default();
        // Host-baked: the client never renders a shape.
        auth.note_shape(7);
        assert_eq!(auth.take_shape_change(), None);
        // Client authority: the shape renders once, then dedups.
        auth.apply_authority(ControlGeneration(1), CursorOwner::Client);
        assert_eq!(auth.take_shape_change(), Some(7));
        assert_eq!(auth.take_shape_change(), None);
        // A new shape id renders once.
        auth.note_shape(8);
        assert_eq!(auth.take_shape_change(), Some(8));
        assert_eq!(auth.take_shape_change(), None);
        // shape_id 0 (none) never renders.
        auth.note_shape(0);
        assert_eq!(auth.take_shape_change(), None);
    }

    #[test]
    fn cursor_authority_reconnect_reverts_to_host_and_restores_shape() {
        let mut auth = ClientCursorAuthority::default();
        auth.apply_authority(ControlGeneration(2), CursorOwner::Client);
        auth.note_shape(42);
        assert_eq!(auth.take_shape_change(), Some(42));
        // Reconnect reverts to host-baked (no double cursor during the gap) and
        // keeps the last shape for restoration.
        auth.on_reconnect();
        assert!(auth.host_baked());
        assert_eq!(auth.take_shape_change(), None);
        // Re-claiming client authority (a fresh, even lower, generation is fine
        // after release) restores the retained shape exactly once.
        assert!(auth.apply_authority(ControlGeneration(1), CursorOwner::Client));
        assert_eq!(auth.take_shape_change(), Some(42));
        assert_eq!(auth.take_shape_change(), None);
    }
}
