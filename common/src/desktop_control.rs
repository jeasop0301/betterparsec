//! Desktop control & ownership contract — the versioned envelope, monotonic
//! generation, typed downgrade reasons, and single-owner lease shared by every
//! desktop-control domain (cursor authority, display/output selection,
//! privacy/display lease, clipboard kinds, and file-transfer control).
//!
//! This is the contract-first predecessor for the R1 desktop train: it exists
//! so the cursor/display/privacy/clipboard/file lanes speak one framing with
//! one ownership rule instead of hand-mirroring five ad-hoc schemas.
//!
//! Wire framing is **little-endian**, matching the `cursor` / `video_fec` wire
//! family (the opposite of the big-endian `input_wire`). A control envelope is
//! a 12-byte header followed by a bounded payload:
//!
//! `u8 version | u8 domain | u16 kind | u32 generation | u32 payload_len | payload`
//!
//! Forward compatibility mirrors the cursor wire: an unknown envelope
//! `version` decodes to `None` (ignored), while an unknown `domain`/`kind`
//! still parses so the receiver can skip it without desyncing the stream.
//!
//! `generation` is an RFC 1982 serial number (same wrap semantics as the FEC
//! sequence space): a newer generation supersedes an older one across the u32
//! wrap, and **a stale generation can never mutate owned state**
//! ([`OwnershipLease`]). Keep any TS mirror byte-pinned against
//! [`tests`] like the other wire modules.

/// Current desktop-control envelope version. A decoder rejects (ignores) any
/// other version so a future revision is forward-compatible.
pub const CONTROL_VERSION: u8 = 1;

/// Fixed envelope header size, before the variable-length payload.
pub const CONTROL_HEADER_LEN: usize = 12;

/// Upper bound on a single control-message payload. Bulk data (large clipboard
/// images, file-transfer chunks) travels on its own bounded reliable module —
/// this caps a single control/metadata frame regardless of domain.
pub const CONTROL_MAX_PAYLOAD: usize = 262_144;

/// The ownership domain an envelope addresses. Kept as a raw `u8` on the wire
/// so an unknown future domain still parses (the receiver ignores it) instead
/// of desyncing the framing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlDomain {
    /// Host cursor authority (visibility / shape / hotspot).
    Cursor,
    /// Output identity and resolution/FPS/mode request/result.
    Display,
    /// Privacy / display lease (blank + local-input policy).
    Privacy,
    /// Clipboard kind negotiation and payloads.
    Clipboard,
    /// File-transfer control (manifest / progress / cancel).
    FileTransfer,
}

impl ControlDomain {
    /// Stable wire code for this domain.
    pub const fn to_u8(self) -> u8 {
        match self {
            ControlDomain::Cursor => 1,
            ControlDomain::Display => 2,
            ControlDomain::Privacy => 3,
            ControlDomain::Clipboard => 4,
            ControlDomain::FileTransfer => 5,
        }
    }

    /// Maps a wire code back to a known domain; `None` for a code this build
    /// does not understand (the envelope still parses — the caller skips it).
    pub const fn from_u8(code: u8) -> Option<ControlDomain> {
        match code {
            1 => Some(ControlDomain::Cursor),
            2 => Some(ControlDomain::Display),
            3 => Some(ControlDomain::Privacy),
            4 => Some(ControlDomain::Clipboard),
            5 => Some(ControlDomain::FileTransfer),
            _ => None,
        }
    }
}

/// Why a requested capability/ownership transition was refused or degraded.
/// An unknown wire code decodes to [`DowngradeReason::Unknown`] carrying the
/// raw byte so telemetry never silently drops a reason it does not recognise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DowngradeReason {
    /// The peer does not support the requested capability at all.
    Unsupported,
    /// Capabilities were understood but do not intersect.
    CapabilityMismatch,
    /// The resource is currently owned by another lease.
    ResourceBusy,
    /// The request carried a stale generation and was rejected.
    StaleGeneration,
    /// Policy (privacy / consent / allowlist) denied the request.
    PolicyDenied,
    /// A transient error; the request may be retried.
    TransientError,
    /// Protocol/schema version mismatch.
    VersionMismatch,
    /// A reason code newer than this build understands.
    Unknown(u8),
}

impl DowngradeReason {
    /// Stable wire code. [`DowngradeReason::Unknown`] round-trips its raw byte.
    pub const fn to_u8(self) -> u8 {
        match self {
            DowngradeReason::Unsupported => 1,
            DowngradeReason::CapabilityMismatch => 2,
            DowngradeReason::ResourceBusy => 3,
            DowngradeReason::StaleGeneration => 4,
            DowngradeReason::PolicyDenied => 5,
            DowngradeReason::TransientError => 6,
            DowngradeReason::VersionMismatch => 7,
            DowngradeReason::Unknown(raw) => raw,
        }
    }

    /// Total, forward-compatible decode: any unrecognised non-zero code becomes
    /// [`DowngradeReason::Unknown`]. Code `0` is reserved (no reason) and yields
    /// `None`.
    pub const fn from_u8(code: u8) -> Option<DowngradeReason> {
        match code {
            0 => None,
            1 => Some(DowngradeReason::Unsupported),
            2 => Some(DowngradeReason::CapabilityMismatch),
            3 => Some(DowngradeReason::ResourceBusy),
            4 => Some(DowngradeReason::StaleGeneration),
            5 => Some(DowngradeReason::PolicyDenied),
            6 => Some(DowngradeReason::TransientError),
            7 => Some(DowngradeReason::VersionMismatch),
            other => Some(DowngradeReason::Unknown(other)),
        }
    }
}

/// A monotonic control generation with RFC 1982 (32-bit) wrap semantics. A
/// sender bumps it whenever it claims or re-claims ownership; a receiver uses
/// [`ControlGeneration::supersedes`] to reject anything that is not strictly
/// newer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ControlGeneration(pub u32);

impl ControlGeneration {
    /// The generation one step newer than this one (wraps at `u32::MAX`).
    pub const fn next(self) -> ControlGeneration {
        ControlGeneration(self.0.wrapping_add(1))
    }

    /// True when `self` is strictly newer than `other` within the RFC 1982
    /// half-range. Equal generations never supersede each other.
    pub const fn supersedes(self, other: ControlGeneration) -> bool {
        self.0 != other.0 && self.0.wrapping_sub(other.0) < 0x8000_0000
    }
}

/// Single-owner lease guarding a desktop-control resource. Encodes the
/// contract invariant "exactly one owner; a stale generation cannot mutate
/// state": [`OwnershipLease::admit`] accepts a first claim, an idempotent
/// refresh at the same generation, or a strictly newer generation, and rejects
/// anything older without touching state.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OwnershipLease {
    generation: Option<ControlGeneration>,
}

impl OwnershipLease {
    /// A lease with no current owner.
    pub const fn new() -> Self {
        OwnershipLease { generation: None }
    }

    /// The generation currently owning the resource, if any.
    pub const fn current(self) -> Option<ControlGeneration> {
        self.generation
    }

    /// Admits an update stamped with `gen`. Returns `true` when the caller may
    /// mutate the owned state (first claim / same-generation refresh / strictly
    /// newer takeover) and `false` when `gen` is stale — in which case the
    /// lease is left unchanged.
    pub fn admit(&mut self, incoming: ControlGeneration) -> bool {
        match self.generation {
            None => {
                self.generation = Some(incoming);
                true
            }
            Some(current) if incoming == current || incoming.supersedes(current) => {
                self.generation = Some(incoming);
                true
            }
            Some(_) => false,
        }
    }

    /// Drops ownership so the next [`OwnershipLease::admit`] of any generation
    /// succeeds (e.g. after a clean teardown or reconnect).
    pub fn release(&mut self) {
        self.generation = None;
    }
}

/// A decoded control envelope borrowing its payload from the input buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlEnvelope<'a> {
    /// Raw domain code (use [`ControlEnvelope::domain`] for the typed value).
    pub domain: u8,
    /// Domain-specific message kind.
    pub kind: u16,
    /// The sender's ownership generation for this message.
    pub generation: ControlGeneration,
    /// The payload bytes (exactly `payload_len`; trailing bytes are ignored).
    pub payload: &'a [u8],
}

impl ControlEnvelope<'_> {
    /// The typed domain, or `None` when this build does not know the code.
    pub fn typed_domain(&self) -> Option<ControlDomain> {
        ControlDomain::from_u8(self.domain)
    }
}

/// Encodes a control envelope. Returns `None` when `payload` exceeds
/// [`CONTROL_MAX_PAYLOAD`] (refuse-to-send, matching the cursor SHAPE cap).
pub fn encode_envelope(
    domain: ControlDomain,
    kind: u16,
    generation: ControlGeneration,
    payload: &[u8],
) -> Option<Vec<u8>> {
    if payload.len() > CONTROL_MAX_PAYLOAD {
        return None;
    }
    let mut out = Vec::with_capacity(CONTROL_HEADER_LEN + payload.len());
    out.push(CONTROL_VERSION);
    out.push(domain.to_u8());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&generation.0.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    Some(out)
}

/// Decodes one control envelope. Returns `None` for: a buffer shorter than the
/// header, an unknown `version` (forward-compat ignore), a declared
/// `payload_len` over [`CONTROL_MAX_PAYLOAD`], or a body shorter than declared.
/// Trailing bytes beyond `payload_len` are tolerated (exactly `payload_len`
/// bytes are borrowed), matching the cursor wire.
pub fn decode_envelope(bytes: &[u8]) -> Option<ControlEnvelope<'_>> {
    if bytes.len() < CONTROL_HEADER_LEN {
        return None;
    }
    if bytes[0] != CONTROL_VERSION {
        return None;
    }
    let domain = bytes[1];
    let kind = u16::from_le_bytes([bytes[2], bytes[3]]);
    let generation =
        ControlGeneration(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]));
    let payload_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    if payload_len > CONTROL_MAX_PAYLOAD {
        return None;
    }
    let end = CONTROL_HEADER_LEN.checked_add(payload_len)?;
    if bytes.len() < end {
        return None;
    }
    Some(ControlEnvelope {
        domain,
        kind,
        generation,
        payload: &bytes[CONTROL_HEADER_LEN..end],
    })
}

/// Whether a requested capability/ownership transition was applied exactly,
/// applied in a degraded form, or refused. Shared across control domains
/// (display mode, privacy lease, …); pair a non-[`TransitionStatus::Applied`]
/// status with a [`DowngradeReason`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionStatus {
    /// The exact request was applied.
    Applied,
    /// A different, degraded form was applied.
    Downgraded,
    /// Nothing was applied.
    Rejected,
}

impl TransitionStatus {
    /// Stable wire code.
    pub const fn to_u8(self) -> u8 {
        match self {
            TransitionStatus::Applied => 0,
            TransitionStatus::Downgraded => 1,
            TransitionStatus::Rejected => 2,
        }
    }

    /// Decodes a status code; `None` for an unknown code (reject the frame).
    pub const fn from_u8(code: u8) -> Option<TransitionStatus> {
        match code {
            0 => Some(TransitionStatus::Applied),
            1 => Some(TransitionStatus::Downgraded),
            2 => Some(TransitionStatus::Rejected),
            _ => None,
        }
    }
}

/// Display / output-selection domain messages, carried in a control envelope
/// with `domain = ControlDomain::Display`. A client names an output and a
/// desired resolution/refresh; the host answers with the effective mode plus an
/// applied/downgraded/rejected status and an optional [`DowngradeReason`].
pub mod display {
    use super::DowngradeReason;

    /// Envelope `kind` for a [`ModeRequest`].
    pub const DISPLAY_KIND_MODE_REQUEST: u16 = 1;
    /// Envelope `kind` for a [`ModeResult`].
    pub const DISPLAY_KIND_MODE_RESULT: u16 = 2;

    /// Fixed encoded size of a mode-request payload.
    pub const MODE_REQUEST_LEN: usize = 10;
    /// Fixed encoded size of a mode-result payload.
    pub const MODE_RESULT_LEN: usize = 12;

    /// A client request to drive one output at a given mode. `output_id == 0`
    /// selects the primary / unspecified output; `refresh_mhz` is millihertz
    /// (e.g. `144_000` = 144 Hz), `0` meaning "unspecified / keep current".
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ModeRequest {
        pub output_id: u16,
        pub width: u16,
        pub height: u16,
        pub refresh_mhz: u32,
    }

    /// Outcome of a [`ModeRequest`] — the shared [`TransitionStatus`].
    pub use super::TransitionStatus as ModeStatus;

    /// The host's answer to a [`ModeRequest`]: the effective output/mode plus
    /// the status and, when not [`ModeStatus::Applied`], a reason.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ModeResult {
        pub output_id: u16,
        pub width: u16,
        pub height: u16,
        pub refresh_mhz: u32,
        pub status: ModeStatus,
        pub reason: Option<DowngradeReason>,
    }

    /// Encodes a mode request (little-endian, fixed [`MODE_REQUEST_LEN`]).
    pub fn encode_mode_request(req: ModeRequest) -> [u8; MODE_REQUEST_LEN] {
        let mut out = [0u8; MODE_REQUEST_LEN];
        out[0..2].copy_from_slice(&req.output_id.to_le_bytes());
        out[2..4].copy_from_slice(&req.width.to_le_bytes());
        out[4..6].copy_from_slice(&req.height.to_le_bytes());
        out[6..10].copy_from_slice(&req.refresh_mhz.to_le_bytes());
        out
    }

    /// Decodes a mode request; `None` if shorter than [`MODE_REQUEST_LEN`].
    /// Trailing bytes are tolerated.
    pub fn decode_mode_request(bytes: &[u8]) -> Option<ModeRequest> {
        if bytes.len() < MODE_REQUEST_LEN {
            return None;
        }
        Some(ModeRequest {
            output_id: u16::from_le_bytes([bytes[0], bytes[1]]),
            width: u16::from_le_bytes([bytes[2], bytes[3]]),
            height: u16::from_le_bytes([bytes[4], bytes[5]]),
            refresh_mhz: u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]),
        })
    }

    /// Encodes a mode result (little-endian, fixed [`MODE_RESULT_LEN`]). The
    /// reason byte is `0` when `reason` is `None`.
    pub fn encode_mode_result(res: ModeResult) -> [u8; MODE_RESULT_LEN] {
        let mut out = [0u8; MODE_RESULT_LEN];
        out[0..2].copy_from_slice(&res.output_id.to_le_bytes());
        out[2..4].copy_from_slice(&res.width.to_le_bytes());
        out[4..6].copy_from_slice(&res.height.to_le_bytes());
        out[6..10].copy_from_slice(&res.refresh_mhz.to_le_bytes());
        out[10] = res.status.to_u8();
        out[11] = res.reason.map_or(0, DowngradeReason::to_u8);
        out
    }

    /// Decodes a mode result; `None` if shorter than [`MODE_RESULT_LEN`] or the
    /// status code is unknown. Trailing bytes are tolerated.
    pub fn decode_mode_result(bytes: &[u8]) -> Option<ModeResult> {
        if bytes.len() < MODE_RESULT_LEN {
            return None;
        }
        let status = ModeStatus::from_u8(bytes[10])?;
        Some(ModeResult {
            output_id: u16::from_le_bytes([bytes[0], bytes[1]]),
            width: u16::from_le_bytes([bytes[2], bytes[3]]),
            height: u16::from_le_bytes([bytes[4], bytes[5]]),
            refresh_mhz: u32::from_le_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]),
            status,
            reason: DowngradeReason::from_u8(bytes[11]),
        })
    }

    /// One confirmed output mode currently in effect.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct AppliedMode {
        pub output_id: u16,
        pub width: u16,
        pub height: u16,
        pub refresh_mhz: u32,
    }

    /// Client-side output-selection state (G011): tracks the last-applied mode
    /// for one active output and an in-flight request, reconciling host
    /// [`ModeResult`]s with rollback on rejection and hot-unplug fallback. Pure
    /// and timer-free; the caller drives the actual switch from the returned
    /// requests/telemetry.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct OutputSelection {
        applied: Option<AppliedMode>,
        pending: Option<ModeRequest>,
    }

    impl OutputSelection {
        /// Records an in-flight mode request (the requested side of the
        /// requested-vs-applied telemetry).
        pub fn request(&mut self, req: ModeRequest) {
            self.pending = Some(req);
        }

        /// The in-flight request, if any.
        pub fn pending(&self) -> Option<ModeRequest> {
            self.pending
        }

        /// The last confirmed applied mode, if any.
        pub fn applied(&self) -> Option<AppliedMode> {
            self.applied
        }

        /// Reconciles a host [`ModeResult`]. On `Applied`/`Downgraded` the
        /// effective mode becomes the applied mode; on `Rejected` the previous
        /// applied mode is kept (rollback). The in-flight request is cleared.
        pub fn reconcile(&mut self, result: ModeResult) -> ModeStatus {
            self.pending = None;
            match result.status {
                ModeStatus::Applied | ModeStatus::Downgraded => {
                    self.applied = Some(AppliedMode {
                        output_id: result.output_id,
                        width: result.width,
                        height: result.height,
                        refresh_mhz: result.refresh_mhz,
                    });
                }
                ModeStatus::Rejected => {}
            }
            result.status
        }

        /// A monitor was removed/hot-unplugged. If it is the applied output, the
        /// applied mode is cleared and a fallback request to `fallback_output` is
        /// staged and returned so the caller can drive the switch; otherwise
        /// `None` (the applied output is unaffected).
        pub fn on_output_removed(
            &mut self,
            output_id: u16,
            fallback_output: u16,
        ) -> Option<ModeRequest> {
            if self.applied.map(|m| m.output_id) == Some(output_id) {
                self.applied = None;
                let req = ModeRequest {
                    output_id: fallback_output,
                    width: 0,
                    height: 0,
                    refresh_mhz: 0,
                };
                self.pending = Some(req);
                Some(req)
            } else {
                None
            }
        }
    }
}

/// Privacy / display-lease domain messages, carried in a control envelope with
/// `domain = ControlDomain::Privacy`. A client requests entering/leaving
/// privacy (host physical display blanked and/or local input blocked); the host
/// reports the effective state. The transition is transactional: a request is
/// [`super::TransitionStatus::Applied`] only when every requested protection is
/// in force, otherwise `Downgraded`/`Rejected` with a [`DowngradeReason`].
pub mod privacy {
    use super::{DowngradeReason, TransitionStatus};

    /// Envelope `kind` for a [`PrivacyRequest`].
    pub const PRIVACY_KIND_REQUEST: u16 = 1;
    /// Envelope `kind` for a [`PrivacyState`].
    pub const PRIVACY_KIND_STATE: u16 = 2;

    /// Blank the host's physical display while privacy is active.
    pub const PROTECT_BLANK_DISPLAY: u8 = 0x01;
    /// Block the host's local keyboard/mouse while privacy is active.
    pub const PROTECT_BLOCK_LOCAL_INPUT: u8 = 0x02;
    /// Mask of all defined protection bits (unknown bits are rejected).
    pub const PROTECT_ALL: u8 = PROTECT_BLANK_DISPLAY | PROTECT_BLOCK_LOCAL_INPUT;

    /// Fixed encoded size of a privacy request payload.
    pub const PRIVACY_REQUEST_LEN: usize = 2;
    /// Fixed encoded size of a privacy state payload.
    pub const PRIVACY_STATE_LEN: usize = 4;

    /// A request to enter (`enable = true`) or leave privacy with the given
    /// protection bits (`PROTECT_*`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PrivacyRequest {
        pub enable: bool,
        pub protections: u8,
    }

    /// The host's effective privacy state: whether privacy is active, which
    /// protections are actually in force, the transition status, and a reason
    /// when the request was not applied exactly.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct PrivacyState {
        pub active: bool,
        pub protections_effective: u8,
        pub status: TransitionStatus,
        pub reason: Option<DowngradeReason>,
    }

    /// Encodes a privacy request (little-endian, fixed [`PRIVACY_REQUEST_LEN`]).
    pub fn encode_request(req: PrivacyRequest) -> [u8; PRIVACY_REQUEST_LEN] {
        [u8::from(req.enable), req.protections]
    }

    /// Decodes a privacy request; `None` if shorter than [`PRIVACY_REQUEST_LEN`]
    /// or a reserved protection bit is set. Trailing bytes are tolerated.
    pub fn decode_request(bytes: &[u8]) -> Option<PrivacyRequest> {
        if bytes.len() < PRIVACY_REQUEST_LEN {
            return None;
        }
        let protections = bytes[1];
        if protections & !PROTECT_ALL != 0 {
            return None;
        }
        Some(PrivacyRequest {
            enable: bytes[0] != 0,
            protections,
        })
    }

    /// Encodes a privacy state (little-endian, fixed [`PRIVACY_STATE_LEN`]).
    pub fn encode_state(state: PrivacyState) -> [u8; PRIVACY_STATE_LEN] {
        [
            u8::from(state.active),
            state.protections_effective,
            state.status.to_u8(),
            state.reason.map_or(0, DowngradeReason::to_u8),
        ]
    }

    /// Decodes a privacy state; `None` if shorter than [`PRIVACY_STATE_LEN`] or
    /// the status code is unknown. Trailing bytes are tolerated.
    pub fn decode_state(bytes: &[u8]) -> Option<PrivacyState> {
        if bytes.len() < PRIVACY_STATE_LEN {
            return None;
        }
        let status = TransitionStatus::from_u8(bytes[2])?;
        Some(PrivacyState {
            active: bytes[0] != 0,
            protections_effective: bytes[1],
            status,
            reason: DowngradeReason::from_u8(bytes[3]),
        })
    }
}

/// Cursor-authority domain (envelope `domain = ControlDomain::Cursor`). Names
/// who renders the cursor so a host-baked cursor and a client-rendered cursor
/// are never shown together; the envelope generation gates ownership via
/// [`OwnershipLease`].
pub mod cursor {
    /// Envelope `kind` for a cursor-authority message.
    pub const CURSOR_KIND_AUTHORITY: u16 = 1;
    /// Fixed encoded size of a cursor-authority payload.
    pub const CURSOR_AUTHORITY_LEN: usize = 1;

    /// Which side renders the cursor.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CursorOwner {
        /// The host bakes the cursor into the video (default).
        Host,
        /// The client renders the cursor from shape messages (zero-latency).
        Client,
    }

    impl CursorOwner {
        /// Stable wire code.
        pub const fn to_u8(self) -> u8 {
            match self {
                CursorOwner::Host => 0,
                CursorOwner::Client => 1,
            }
        }

        /// Decodes an owner code; `None` for an unknown code.
        pub const fn from_u8(code: u8) -> Option<CursorOwner> {
            match code {
                0 => Some(CursorOwner::Host),
                1 => Some(CursorOwner::Client),
                _ => None,
            }
        }
    }

    /// Encodes a cursor-authority message.
    pub fn encode_authority(owner: CursorOwner) -> [u8; CURSOR_AUTHORITY_LEN] {
        [owner.to_u8()]
    }

    /// Decodes a cursor-authority message; `None` if truncated or the code is
    /// unknown. Trailing bytes are tolerated.
    pub fn decode_authority(bytes: &[u8]) -> Option<CursorOwner> {
        if bytes.len() < CURSOR_AUTHORITY_LEN {
            return None;
        }
        CursorOwner::from_u8(bytes[0])
    }
}

/// Clipboard-kind negotiation (envelope `domain = ControlDomain::Clipboard`). An
/// OFFER advertises the content kinds a side currently holds; a REQUEST asks for
/// exactly one kind. Bulk payloads travel on their own modules, not here.
pub mod clipboard {
    /// Envelope `kind` for an offer bitset.
    pub const CLIPBOARD_KIND_OFFER: u16 = 1;
    /// Envelope `kind` for a single-kind request.
    pub const CLIPBOARD_KIND_REQUEST: u16 = 2;

    /// Plain UTF-8 text is available.
    pub const CLIP_TEXT: u8 = 0x01;
    /// A PNG image is available.
    pub const CLIP_PNG: u8 = 0x02;
    /// A file list is available.
    pub const CLIP_FILE_LIST: u8 = 0x04;
    /// Mask of all defined clipboard kinds.
    pub const CLIP_ALL: u8 = CLIP_TEXT | CLIP_PNG | CLIP_FILE_LIST;

    /// Fixed encoded size of an offer payload.
    pub const CLIPBOARD_OFFER_LEN: usize = 1;
    /// Fixed encoded size of a request payload.
    pub const CLIPBOARD_REQUEST_LEN: usize = 1;

    /// Encodes an offer bitset; unknown bits are masked off.
    pub fn encode_offer(kinds: u8) -> [u8; CLIPBOARD_OFFER_LEN] {
        [kinds & CLIP_ALL]
    }

    /// Decodes an offer; unknown bits are ignored (masked to known kinds).
    /// `None` only if truncated.
    pub fn decode_offer(bytes: &[u8]) -> Option<u8> {
        if bytes.len() < CLIPBOARD_OFFER_LEN {
            return None;
        }
        Some(bytes[0] & CLIP_ALL)
    }

    /// Encodes a single-kind request.
    pub fn encode_request(kind: u8) -> [u8; CLIPBOARD_REQUEST_LEN] {
        [kind]
    }

    /// Decodes a request; `None` unless exactly one known kind bit is set.
    pub fn decode_request(bytes: &[u8]) -> Option<u8> {
        if bytes.len() < CLIPBOARD_REQUEST_LEN {
            return None;
        }
        let kind = bytes[0];
        if kind & !CLIP_ALL != 0 || kind.count_ones() != 1 {
            return None;
        }
        Some(kind)
    }
}

/// File-transfer control (envelope `domain = ControlDomain::FileTransfer`). Bulk
/// bytes travel on a separate bounded reliable module; these frames only
/// negotiate and track a transfer.
pub mod file_transfer {
    use super::DowngradeReason;

    /// Envelope `kind` for a [`FileOffer`].
    pub const FT_KIND_OFFER: u16 = 1;
    /// Envelope `kind` for an accept (`u32 transfer_id`).
    pub const FT_KIND_ACCEPT: u16 = 2;
    /// Envelope `kind` for a cancel (`u32 transfer_id | u8 reason`).
    pub const FT_KIND_CANCEL: u16 = 3;
    /// Envelope `kind` for progress (`u32 transfer_id | u64 bytes_done`).
    pub const FT_KIND_PROGRESS: u16 = 4;

    /// Fixed header of an offer before the variable-length name.
    pub const FT_OFFER_HEADER_LEN: usize = 14;
    /// Upper bound on an encoded file name (UTF-8 bytes).
    pub const FT_MAX_NAME_LEN: usize = 1024;

    /// An offer to send one file: id, total size, and a bounded UTF-8 name.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct FileOffer {
        pub transfer_id: u32,
        pub total_size: u64,
        pub name: String,
    }

    /// Encodes an offer (`u32 transfer_id | u64 total_size | u16 name_len |
    /// name`); `None` if the name exceeds [`FT_MAX_NAME_LEN`] bytes.
    pub fn encode_offer(offer: &FileOffer) -> Option<Vec<u8>> {
        let name = offer.name.as_bytes();
        if name.len() > FT_MAX_NAME_LEN {
            return None;
        }
        let mut out = Vec::with_capacity(FT_OFFER_HEADER_LEN + name.len());
        out.extend_from_slice(&offer.transfer_id.to_le_bytes());
        out.extend_from_slice(&offer.total_size.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name);
        Some(out)
    }

    /// Decodes an offer; `None` on truncation, an over-cap name, or invalid
    /// UTF-8. Trailing bytes beyond the declared name are tolerated.
    pub fn decode_offer(bytes: &[u8]) -> Option<FileOffer> {
        if bytes.len() < FT_OFFER_HEADER_LEN {
            return None;
        }
        let transfer_id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let total_size = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
        let name_len = u16::from_le_bytes([bytes[12], bytes[13]]) as usize;
        if name_len > FT_MAX_NAME_LEN {
            return None;
        }
        let end = FT_OFFER_HEADER_LEN.checked_add(name_len)?;
        if bytes.len() < end {
            return None;
        }
        let name = std::str::from_utf8(&bytes[FT_OFFER_HEADER_LEN..end])
            .ok()?
            .to_owned();
        Some(FileOffer {
            transfer_id,
            total_size,
            name,
        })
    }

    /// Encodes an accept (`u32 transfer_id`).
    pub fn encode_accept(transfer_id: u32) -> [u8; 4] {
        transfer_id.to_le_bytes()
    }

    /// Decodes an accept; `None` if truncated.
    pub fn decode_accept(bytes: &[u8]) -> Option<u32> {
        if bytes.len() < 4 {
            return None;
        }
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Encodes a cancel (`u32 transfer_id | u8 reason`; `0` = no reason).
    pub fn encode_cancel(transfer_id: u32, reason: Option<DowngradeReason>) -> [u8; 5] {
        let mut out = [0u8; 5];
        out[0..4].copy_from_slice(&transfer_id.to_le_bytes());
        out[4] = reason.map_or(0, DowngradeReason::to_u8);
        out
    }

    /// Decodes a cancel; `None` if truncated.
    pub fn decode_cancel(bytes: &[u8]) -> Option<(u32, Option<DowngradeReason>)> {
        if bytes.len() < 5 {
            return None;
        }
        let id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Some((id, DowngradeReason::from_u8(bytes[4])))
    }

    /// Encodes progress (`u32 transfer_id | u64 bytes_done`).
    pub fn encode_progress(transfer_id: u32, bytes_done: u64) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[0..4].copy_from_slice(&transfer_id.to_le_bytes());
        out[4..12].copy_from_slice(&bytes_done.to_le_bytes());
        out
    }

    /// Decodes progress; `None` if truncated.
    pub fn decode_progress(bytes: &[u8]) -> Option<(u32, u64)> {
        if bytes.len() < 12 {
            return None;
        }
        let id = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let done = u64::from_le_bytes(bytes[4..12].try_into().ok()?);
        Some((id, done))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_wrap_safe_supersede() {
        assert!(ControlGeneration(5).supersedes(ControlGeneration(3)));
        assert!(!ControlGeneration(3).supersedes(ControlGeneration(5)));
        // No self-supersession.
        assert!(!ControlGeneration(7).supersedes(ControlGeneration(7)));
        // Across the u32 wrap, 0 is one step after u32::MAX.
        assert!(ControlGeneration(0).supersedes(ControlGeneration(u32::MAX)));
        assert!(!ControlGeneration(u32::MAX).supersedes(ControlGeneration(0)));
        assert_eq!(ControlGeneration(u32::MAX).next(), ControlGeneration(0));
        // Half-range boundary: +2^31 is NOT newer (ambiguous), +(2^31 - 1) is.
        assert!(ControlGeneration(0x7FFF_FFFF).supersedes(ControlGeneration(0)));
        assert!(!ControlGeneration(0x8000_0000).supersedes(ControlGeneration(0)));
    }

    #[test]
    fn ownership_lease_rejects_stale_without_mutating() {
        let mut lease = OwnershipLease::new();
        // First claim wins.
        assert!(lease.admit(ControlGeneration(10)));
        assert_eq!(lease.current(), Some(ControlGeneration(10)));
        // Idempotent same-generation refresh is admitted.
        assert!(lease.admit(ControlGeneration(10)));
        // A strictly newer generation takes over.
        assert!(lease.admit(ControlGeneration(11)));
        assert_eq!(lease.current(), Some(ControlGeneration(11)));
        // A stale generation is rejected and leaves the lease untouched.
        assert!(!lease.admit(ControlGeneration(10)));
        assert_eq!(lease.current(), Some(ControlGeneration(11)));
        // Release drops ownership so any next generation is admitted.
        lease.release();
        assert_eq!(lease.current(), None);
        assert!(lease.admit(ControlGeneration(4)));
        // Wrap-around takeover is admitted.
        let mut wrap = OwnershipLease::new();
        assert!(wrap.admit(ControlGeneration(u32::MAX)));
        assert!(wrap.admit(ControlGeneration(0)));
        assert_eq!(wrap.current(), Some(ControlGeneration(0)));
    }

    #[test]
    fn downgrade_reason_round_trips_and_forward_compat() {
        for reason in [
            DowngradeReason::Unsupported,
            DowngradeReason::CapabilityMismatch,
            DowngradeReason::ResourceBusy,
            DowngradeReason::StaleGeneration,
            DowngradeReason::PolicyDenied,
            DowngradeReason::TransientError,
            DowngradeReason::VersionMismatch,
        ] {
            assert_eq!(DowngradeReason::from_u8(reason.to_u8()), Some(reason));
        }
        // Reserved 0 = no reason.
        assert_eq!(DowngradeReason::from_u8(0), None);
        // Unknown non-zero codes survive as Unknown(raw) and round-trip.
        assert_eq!(
            DowngradeReason::from_u8(200),
            Some(DowngradeReason::Unknown(200))
        );
        assert_eq!(DowngradeReason::Unknown(200).to_u8(), 200);
    }

    #[test]
    fn domain_codes_are_stable() {
        for domain in [
            ControlDomain::Cursor,
            ControlDomain::Display,
            ControlDomain::Privacy,
            ControlDomain::Clipboard,
            ControlDomain::FileTransfer,
        ] {
            assert_eq!(ControlDomain::from_u8(domain.to_u8()), Some(domain));
        }
        assert_eq!(ControlDomain::from_u8(0), None);
        assert_eq!(ControlDomain::from_u8(99), None);
    }

    #[test]
    fn envelope_byte_pin_and_round_trip() {
        let payload = [0xAA, 0xBB, 0xCC];
        let bytes = encode_envelope(
            ControlDomain::Display,
            0x0102,
            ControlGeneration(0x0403_0201),
            &payload,
        )
        .expect("payload within cap");
        // Byte-pinned little-endian header + payload (mirror in TS).
        assert_eq!(
            bytes,
            vec![
                0x01, // version
                0x02, // domain = Display
                0x02, 0x01, // kind 0x0102 LE
                0x01, 0x02, 0x03, 0x04, // generation 0x04030201 LE
                0x03, 0x00, 0x00, 0x00, // payload_len 3 LE
                0xAA, 0xBB, 0xCC, // payload
            ]
        );
        let env = decode_envelope(&bytes).expect("valid envelope");
        assert_eq!(env.typed_domain(), Some(ControlDomain::Display));
        assert_eq!(env.kind, 0x0102);
        assert_eq!(env.generation, ControlGeneration(0x0403_0201));
        assert_eq!(env.payload, &payload);
    }

    #[test]
    fn envelope_rejects_malformed_and_tolerates_trailing() {
        let good = encode_envelope(ControlDomain::Cursor, 1, ControlGeneration(1), &[9, 9])
            .expect("payload within cap");
        // Truncated header.
        assert!(decode_envelope(&good[..CONTROL_HEADER_LEN - 1]).is_none());
        // Unknown version is ignored.
        let mut bad_version = good.clone();
        bad_version[0] = 2;
        assert!(decode_envelope(&bad_version).is_none());
        // Body shorter than declared payload_len.
        let mut short_body = good.clone();
        short_body.pop();
        assert!(decode_envelope(&short_body).is_none());
        // Oversized declared payload_len is rejected.
        let mut oversize = good.clone();
        oversize[8..12].copy_from_slice(&((CONTROL_MAX_PAYLOAD as u32) + 1).to_le_bytes());
        assert!(decode_envelope(&oversize).is_none());
        // Trailing bytes beyond payload_len are tolerated.
        let mut trailing = good.clone();
        trailing.extend_from_slice(&[0xFF, 0xFF]);
        let env = decode_envelope(&trailing).expect("trailing tolerated");
        assert_eq!(env.payload, &[9, 9]);
    }

    #[test]
    fn envelope_refuses_oversized_payload_on_encode() {
        let too_big = vec![0u8; CONTROL_MAX_PAYLOAD + 1];
        assert!(
            encode_envelope(ControlDomain::Clipboard, 0, ControlGeneration(0), &too_big).is_none()
        );
        let at_cap = vec![0u8; CONTROL_MAX_PAYLOAD];
        assert!(
            encode_envelope(ControlDomain::Clipboard, 0, ControlGeneration(0), &at_cap).is_some()
        );
    }

    #[test]
    fn display_mode_request_byte_pin_and_round_trip() {
        let req = display::ModeRequest {
            output_id: 2,
            width: 3840,
            height: 2160,
            refresh_mhz: 144_000,
        };
        let bytes = display::encode_mode_request(req);
        assert_eq!(
            bytes,
            [
                0x02, 0x00, // output_id = 2
                0x00, 0x0F, // width 3840 LE
                0x70, 0x08, // height 2160 LE
                0x80, 0x32, 0x02, 0x00, // refresh 144000 mHz LE
            ]
        );
        assert_eq!(display::decode_mode_request(&bytes), Some(req));
        // Trailing tolerated; truncation rejected.
        let mut trailing = bytes.to_vec();
        trailing.push(0xEE);
        assert_eq!(display::decode_mode_request(&trailing), Some(req));
        assert!(display::decode_mode_request(&bytes[..display::MODE_REQUEST_LEN - 1]).is_none());
    }

    #[test]
    fn display_mode_result_byte_pin_and_status() {
        let res = display::ModeResult {
            output_id: 1,
            width: 2560,
            height: 1440,
            refresh_mhz: 60_000,
            status: display::ModeStatus::Downgraded,
            reason: Some(DowngradeReason::CapabilityMismatch),
        };
        let bytes = display::encode_mode_result(res);
        assert_eq!(
            bytes,
            [
                0x01, 0x00, // output_id = 1
                0x00, 0x0A, // width 2560 LE
                0xA0, 0x05, // height 1440 LE
                0x60, 0xEA, 0x00, 0x00, // refresh 60000 mHz LE
                0x01, // status Downgraded
                0x02, // reason CapabilityMismatch
            ]
        );
        assert_eq!(display::decode_mode_result(&bytes), Some(res));
        // Applied result with no reason.
        let applied = display::ModeResult {
            status: display::ModeStatus::Applied,
            reason: None,
            ..res
        };
        let ab = display::encode_mode_result(applied);
        assert_eq!(ab[10], 0);
        assert_eq!(ab[11], 0);
        assert_eq!(display::decode_mode_result(&ab), Some(applied));
        // Unknown status code is rejected.
        let mut bad = bytes;
        bad[10] = 9;
        assert!(display::decode_mode_result(&bad).is_none());
        assert!(display::decode_mode_result(&bytes[..display::MODE_RESULT_LEN - 1]).is_none());
    }

    #[test]
    fn display_request_over_envelope_is_generation_gated() {
        let mut lease = OwnershipLease::new();
        let req = display::ModeRequest {
            output_id: 0,
            width: 1920,
            height: 1080,
            refresh_mhz: 0,
        };
        // A first request at generation 5 is admitted and decodes.
        let env_bytes = encode_envelope(
            ControlDomain::Display,
            display::DISPLAY_KIND_MODE_REQUEST,
            ControlGeneration(5),
            &display::encode_mode_request(req),
        )
        .expect("payload within cap");
        let env = decode_envelope(&env_bytes).expect("valid envelope");
        assert_eq!(env.typed_domain(), Some(ControlDomain::Display));
        assert_eq!(env.kind, display::DISPLAY_KIND_MODE_REQUEST);
        assert!(lease.admit(env.generation));
        assert_eq!(display::decode_mode_request(env.payload), Some(req));
        // A stale generation-3 request is rejected by the lease (state unchanged).
        let stale = encode_envelope(
            ControlDomain::Display,
            display::DISPLAY_KIND_MODE_REQUEST,
            ControlGeneration(3),
            &display::encode_mode_request(req),
        )
        .expect("payload within cap");
        let stale_env = decode_envelope(&stale).expect("valid envelope");
        assert!(!lease.admit(stale_env.generation));
        assert_eq!(lease.current(), Some(ControlGeneration(5)));
    }

    #[test]
    fn privacy_request_round_trip_and_reserved_bits() {
        let req = privacy::PrivacyRequest {
            enable: true,
            protections: privacy::PROTECT_BLANK_DISPLAY | privacy::PROTECT_BLOCK_LOCAL_INPUT,
        };
        let bytes = privacy::encode_request(req);
        assert_eq!(bytes, [0x01, 0x03]);
        assert_eq!(privacy::decode_request(&bytes), Some(req));
        // Leaving privacy with no protections.
        assert_eq!(
            privacy::decode_request(&[0x00, 0x00]),
            Some(privacy::PrivacyRequest {
                enable: false,
                protections: 0
            })
        );
        // A reserved protection bit is rejected.
        assert!(privacy::decode_request(&[0x01, 0x80]).is_none());
        // Truncation rejected.
        assert!(privacy::decode_request(&bytes[..privacy::PRIVACY_REQUEST_LEN - 1]).is_none());
    }

    #[test]
    fn privacy_state_byte_pin_and_status() {
        let state = privacy::PrivacyState {
            active: true,
            protections_effective: privacy::PROTECT_BLANK_DISPLAY,
            status: TransitionStatus::Downgraded,
            reason: Some(DowngradeReason::PolicyDenied),
        };
        let bytes = privacy::encode_state(state);
        assert_eq!(bytes, [0x01, 0x01, 0x01, 0x05]);
        assert_eq!(privacy::decode_state(&bytes), Some(state));
        // Fully applied, no reason.
        let applied = privacy::PrivacyState {
            active: true,
            protections_effective: privacy::PROTECT_ALL,
            status: TransitionStatus::Applied,
            reason: None,
        };
        let ab = privacy::encode_state(applied);
        assert_eq!(ab, [0x01, 0x03, 0x00, 0x00]);
        assert_eq!(privacy::decode_state(&ab), Some(applied));
        // Unknown status rejected; truncation rejected.
        let mut bad = bytes;
        bad[2] = 9;
        assert!(privacy::decode_state(&bad).is_none());
        assert!(privacy::decode_state(&bytes[..privacy::PRIVACY_STATE_LEN - 1]).is_none());
    }

    #[test]
    fn shared_transition_status_round_trips() {
        for status in [
            TransitionStatus::Applied,
            TransitionStatus::Downgraded,
            TransitionStatus::Rejected,
        ] {
            assert_eq!(TransitionStatus::from_u8(status.to_u8()), Some(status));
        }
        assert!(TransitionStatus::from_u8(3).is_none());
        // Display's ModeStatus is the same shared type.
        assert_eq!(
            display::ModeStatus::Downgraded,
            TransitionStatus::Downgraded
        );
    }

    #[test]
    fn cursor_authority_round_trip_and_unknown() {
        assert_eq!(cursor::encode_authority(cursor::CursorOwner::Host), [0x00]);
        assert_eq!(
            cursor::encode_authority(cursor::CursorOwner::Client),
            [0x01]
        );
        assert_eq!(
            cursor::decode_authority(&[0]),
            Some(cursor::CursorOwner::Host)
        );
        assert_eq!(
            cursor::decode_authority(&[1]),
            Some(cursor::CursorOwner::Client)
        );
        assert!(cursor::decode_authority(&[2]).is_none());
        assert!(cursor::decode_authority(&[]).is_none());
        // Trailing tolerated.
        assert_eq!(
            cursor::decode_authority(&[1, 9]),
            Some(cursor::CursorOwner::Client)
        );
    }

    #[test]
    fn clipboard_offer_masks_and_request_is_single_kind() {
        // Offer masks off unknown bits, keeps known kinds.
        assert_eq!(
            clipboard::encode_offer(clipboard::CLIP_ALL | 0x80),
            [clipboard::CLIP_ALL]
        );
        assert_eq!(clipboard::decode_offer(&[0xFF]), Some(clipboard::CLIP_ALL));
        assert!(clipboard::decode_offer(&[]).is_none());
        // Request must be exactly one known kind.
        assert_eq!(
            clipboard::decode_request(&[clipboard::CLIP_PNG]),
            Some(clipboard::CLIP_PNG)
        );
        assert!(clipboard::decode_request(&[clipboard::CLIP_TEXT | clipboard::CLIP_PNG]).is_none());
        assert!(clipboard::decode_request(&[0x00]).is_none());
        assert!(clipboard::decode_request(&[0x80]).is_none());
    }

    #[test]
    fn file_transfer_offer_round_trip_and_bounds() {
        let offer = file_transfer::FileOffer {
            transfer_id: 0x0A0B_0C0D,
            total_size: 5_000_000_000,
            name: "movie.mkv".to_owned(),
        };
        let bytes = file_transfer::encode_offer(&offer).expect("name within cap");
        // Header byte-pin: id LE, size LE, name_len LE, then UTF-8 name.
        assert_eq!(&bytes[0..4], &[0x0D, 0x0C, 0x0B, 0x0A]);
        assert_eq!(&bytes[12..14], &[0x09, 0x00]);
        assert_eq!(&bytes[14..], b"movie.mkv");
        assert_eq!(file_transfer::decode_offer(&bytes), Some(offer.clone()));
        // Trailing tolerated.
        let mut trailing = bytes.clone();
        trailing.push(0xEE);
        assert_eq!(file_transfer::decode_offer(&trailing), Some(offer));
        // Truncated header and truncated name rejected.
        assert!(
            file_transfer::decode_offer(&bytes[..file_transfer::FT_OFFER_HEADER_LEN - 1]).is_none()
        );
        assert!(file_transfer::decode_offer(&bytes[..bytes.len() - 1]).is_none());
        // Over-cap name refused on encode.
        let big = file_transfer::FileOffer {
            transfer_id: 1,
            total_size: 0,
            name: "x".repeat(file_transfer::FT_MAX_NAME_LEN + 1),
        };
        assert!(file_transfer::encode_offer(&big).is_none());
        // Invalid UTF-8 name rejected on decode.
        let mut bad_utf8 = file_transfer::encode_offer(&file_transfer::FileOffer {
            transfer_id: 1,
            total_size: 0,
            name: "ab".to_owned(),
        })
        .expect("within cap");
        bad_utf8[14] = 0xFF;
        assert!(file_transfer::decode_offer(&bad_utf8).is_none());
    }

    #[test]
    fn file_transfer_accept_cancel_progress_round_trip() {
        assert_eq!(
            file_transfer::decode_accept(&file_transfer::encode_accept(42)),
            Some(42)
        );
        assert!(file_transfer::decode_accept(&[0, 0, 0]).is_none());

        let cancel = file_transfer::encode_cancel(7, Some(DowngradeReason::PolicyDenied));
        assert_eq!(cancel, [0x07, 0x00, 0x00, 0x00, 0x05]);
        assert_eq!(
            file_transfer::decode_cancel(&cancel),
            Some((7, Some(DowngradeReason::PolicyDenied)))
        );
        // No-reason cancel decodes reason None.
        let plain = file_transfer::encode_cancel(7, None);
        assert_eq!(file_transfer::decode_cancel(&plain), Some((7, None)));
        assert!(file_transfer::decode_cancel(&[0, 0, 0, 0]).is_none());

        let prog = file_transfer::encode_progress(9, 1_234_567);
        assert_eq!(file_transfer::decode_progress(&prog), Some((9, 1_234_567)));
        assert!(file_transfer::decode_progress(&[0; 11]).is_none());
    }

    #[test]
    fn output_selection_tracks_requested_vs_applied_and_rolls_back() {
        use display::{ModeRequest, ModeResult, ModeStatus, OutputSelection};
        let mut sel = OutputSelection::default();
        assert_eq!(sel.applied(), None);
        assert_eq!(sel.pending(), None);

        let req = ModeRequest {
            output_id: 1,
            width: 3840,
            height: 2160,
            refresh_mhz: 144_000,
        };
        sel.request(req);
        assert_eq!(sel.pending(), Some(req));

        // Applied result becomes the applied mode and clears the request.
        let status = sel.reconcile(ModeResult {
            output_id: 1,
            width: 3840,
            height: 2160,
            refresh_mhz: 144_000,
            status: ModeStatus::Applied,
            reason: None,
        });
        assert_eq!(status, ModeStatus::Applied);
        assert_eq!(sel.pending(), None);
        let applied = sel.applied().expect("applied mode");
        assert_eq!(
            (applied.output_id, applied.width, applied.height),
            (1, 3840, 2160)
        );

        // A rejected request rolls back to the previous applied mode.
        sel.request(ModeRequest {
            output_id: 1,
            width: 7680,
            height: 4320,
            refresh_mhz: 60_000,
        });
        let status = sel.reconcile(ModeResult {
            output_id: 1,
            width: 0,
            height: 0,
            refresh_mhz: 0,
            status: ModeStatus::Rejected,
            reason: Some(DowngradeReason::Unsupported),
        });
        assert_eq!(status, ModeStatus::Rejected);
        assert_eq!(sel.pending(), None);
        assert_eq!(sel.applied().map(|m| m.width), Some(3840)); // rolled back

        // A downgraded result adopts the effective (echoed) mode.
        sel.request(ModeRequest {
            output_id: 1,
            width: 2560,
            height: 1440,
            refresh_mhz: 240_000,
        });
        sel.reconcile(ModeResult {
            output_id: 1,
            width: 2560,
            height: 1440,
            refresh_mhz: 144_000,
            status: ModeStatus::Downgraded,
            reason: Some(DowngradeReason::CapabilityMismatch),
        });
        assert_eq!(sel.applied().map(|m| m.refresh_mhz), Some(144_000));
    }

    #[test]
    fn output_selection_hot_unplug_falls_back_only_for_the_active_output() {
        use display::{ModeRequest, ModeResult, ModeStatus, OutputSelection};
        let mut sel = OutputSelection::default();
        sel.request(ModeRequest {
            output_id: 2,
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
        });
        sel.reconcile(ModeResult {
            output_id: 2,
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
            status: ModeStatus::Applied,
            reason: None,
        });
        // Removing an unrelated output does nothing.
        assert_eq!(sel.on_output_removed(5, 0), None);
        assert!(sel.applied().is_some());
        // Removing the active output clears it and stages a fallback request.
        let fallback = sel.on_output_removed(2, 0).expect("fallback request");
        assert_eq!(fallback.output_id, 0);
        assert_eq!(sel.applied(), None);
        assert_eq!(sel.pending(), Some(fallback));
    }
}
