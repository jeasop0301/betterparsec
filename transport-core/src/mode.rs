//! Pure mode→knobs engine — maps a coarse user-facing [`StreamMode`] plus
//! optional per-field [`UserTradeoffs`] overrides onto the concrete
//! [`ModeKnobs`] the transport wires into `client-transport`'s `FlowConfig`
//! (`bitrate_kbps` / `width` / `height` / `fps` / `supported_codecs`, see
//! `client-transport/src/flow.rs`) and the audio-exclusive-mode default.
//! No clocks, no I/O — mirrors the `resolution.rs` / `fec_ratio.rs` pattern:
//! pure controller + `#[derive(Default)]`-style tunables.
//!
//! `transport-core` has zero dependencies, so this module does not import
//! `common::api_bindings::StreamSupportedVideoCodecs` or the app-native
//! `VideoFormats` bitflags type directly. [`CodecPref`] is a small ordinal
//! preference enum that a later wiring goal maps onto that `u32` bitmask
//! (`FlowConfig::supported_codecs`) — e.g. `CodecPref::H264` →
//! `StreamSupportedVideoCodecs::H264`, `CodecPref::Hevc` → `H265`,
//! `CodecPref::Av1` → `AV1_MAIN8` (falling back down the list as the peer's
//! advertised support requires). This module never touches the bitmask
//! itself, only the preference ordinal.
//!
//! `ModeKnobs` only carries fields with a real consumer today. Two levers
//! discussed in the mode-tuning design (`pacing`, `fec_bias`) are RESERVED:
//! the pacing controller and adaptive-FEC-bias wiring are deferred to a
//! later goal, so they are deliberately *not* emitted as knob fields — no
//! dead outputs. When they grow a consumer, add fields then.
//!
//! Free-lunch quality levers (spatial adaptive-quantization on, encoder
//! preset, weighted-prediction intent) are reserved shipping *defaults*,
//! never toggles: they live in the private [`free_lunch`] constants below,
//! are never varied by mode or user, are not exposed on `ModeKnobs`, and
//! are consumed by a later encoder-wiring goal (no encoder reads them yet).

/// Coarse user-facing streaming mode. `Fast` biases every default toward
/// low latency; `Quality` biases toward fidelity; `Medium` sits between.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamMode {
    Fast,
    #[default]
    Medium,
    Quality,
}

/// Ordinal codec preference. Maps onto the `FlowConfig::supported_codecs`
/// bitmask (`common::api_bindings::StreamSupportedVideoCodecs`) in a later
/// wiring goal; kept as a plain enum here since `transport-core` has zero
/// dependencies and must not import `common` or app-native's `VideoFormats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecPref {
    /// Widest hardware/decoder support, lowest encode latency headroom.
    H264,
    /// Better quality-per-bit than H264; moderate latency headroom.
    Hevc,
    /// Best quality-per-bit; highest encode complexity/latency headroom.
    Av1,
}

/// Genuine user-picked tradeoffs. A `0` field means "use the mode default";
/// a non-zero field overrides that single mode default without affecting
/// any other knob (free-lunch constants never move).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UserTradeoffs {
    pub bitrate_kbps: u32,
    pub width: u16,
    pub height: u16,
    pub fps: u16,
}

/// Concrete knobs for one streaming session. Only fields with a real
/// consumer today: `codec_pref` / `width` / `height` / `bitrate_kbps` /
/// `fps` map onto `client-transport::flow::FlowConfig`;
/// `audio_exclusive_default` maps onto the host's audio-exclusive-mode
/// default. `pacing` and `fec_bias` are intentionally absent — see the
/// module doc comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeKnobs {
    pub codec_pref: CodecPref,
    pub width: u16,
    pub height: u16,
    pub bitrate_kbps: u32,
    pub fps: u16,
    pub audio_exclusive_default: bool,
}

/// Free-lunch quality levers: constants only, never surfaced as knobs and
/// never varied by mode or user tradeoffs. "Free lunch" because they cost
/// no latency/bitrate budget the user can feel, so there is nothing to
/// tune — just a default to ship.
#[allow(dead_code)]
mod free_lunch {
    /// Spatial adaptive-quantization: always on.
    pub const SPATIAL_AQ_ON: bool = true;
    /// Encoder preset intent (opaque to this module; consumed by the
    /// encoder wiring as a named preset, not a numeric knob here).
    pub const PRESET: &str = "p4-quality-latency-balanced";
    /// Weighted-prediction intent: always on.
    pub const WEIGHTED_PRED_ON: bool = true;
}

/// Mode-default knobs before any [`UserTradeoffs`] override is applied.
fn mode_defaults(mode: StreamMode) -> ModeKnobs {
    match mode {
        StreamMode::Fast => ModeKnobs {
            codec_pref: CodecPref::H264,
            width: 1280,
            height: 720,
            bitrate_kbps: 8_000,
            fps: 60,
            audio_exclusive_default: true,
        },
        StreamMode::Medium => ModeKnobs {
            codec_pref: CodecPref::Hevc,
            width: 1920,
            height: 1080,
            bitrate_kbps: 20_000,
            fps: 60,
            audio_exclusive_default: true,
        },
        StreamMode::Quality => ModeKnobs {
            codec_pref: CodecPref::Av1,
            width: 3840,
            height: 2160,
            bitrate_kbps: 50_000,
            fps: 60,
            audio_exclusive_default: false,
        },
    }
}

/// Map a [`StreamMode`] plus optional user overrides onto concrete
/// [`ModeKnobs`]. The mode picks defaults for every field; a non-zero
/// `user` field overrides only that field. Free-lunch constants
/// (`free_lunch` module) never change — they are not part of `ModeKnobs`.
pub fn knobs_for(mode: StreamMode, user: UserTradeoffs) -> ModeKnobs {
    let mut knobs = mode_defaults(mode);

    if user.bitrate_kbps != 0 {
        knobs.bitrate_kbps = user.bitrate_kbps;
    }
    if user.width != 0 {
        knobs.width = user.width;
    }
    if user.height != 0 {
        knobs.height = user.height;
    }
    if user.fps != 0 {
        knobs.fps = user.fps;
    }

    knobs
}

/// Canonical mode-default vectors (zeroed [`UserTradeoffs`], i.e. no
/// overrides) for every [`StreamMode`]. This is the single shared parity
/// source the web mirror (`tests/mode.test.mjs`) reuses so the Rust and TS
/// defaults cannot silently drift apart.
pub fn canonical_vectors() -> Vec<(StreamMode, ModeKnobs)> {
    [StreamMode::Fast, StreamMode::Medium, StreamMode::Quality]
        .into_iter()
        .map(|mode| (mode, knobs_for(mode, UserTradeoffs::default())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_minimizes_latency_knobs() {
        let knobs = knobs_for(StreamMode::Fast, UserTradeoffs::default());
        assert_eq!(knobs.codec_pref, CodecPref::H264);
        assert!(knobs.bitrate_kbps < mode_defaults(StreamMode::Medium).bitrate_kbps);
        assert!(knobs.width <= mode_defaults(StreamMode::Medium).width);
        assert!(knobs.height <= mode_defaults(StreamMode::Medium).height);
        assert!(knobs.audio_exclusive_default);
    }

    #[test]
    fn quality_prefers_hevc_av1_codec() {
        let knobs = knobs_for(StreamMode::Quality, UserTradeoffs::default());
        assert_eq!(knobs.codec_pref, CodecPref::Av1);
        assert!(knobs.bitrate_kbps > mode_defaults(StreamMode::Medium).bitrate_kbps);
        assert!(knobs.width >= mode_defaults(StreamMode::Medium).width);
        assert!(knobs.height >= mode_defaults(StreamMode::Medium).height);
    }

    #[test]
    fn medium_is_between() {
        let fast = mode_defaults(StreamMode::Fast);
        let medium = mode_defaults(StreamMode::Medium);
        let quality = mode_defaults(StreamMode::Quality);

        assert!(fast.bitrate_kbps < medium.bitrate_kbps);
        assert!(medium.bitrate_kbps < quality.bitrate_kbps);
        assert_eq!(medium.codec_pref, CodecPref::Hevc);
        assert!(fast.width <= medium.width && medium.width <= quality.width);
        assert!(fast.height <= medium.height && medium.height <= quality.height);
    }

    #[test]
    fn user_tradeoffs_override_but_freelunch_constant() {
        let user = UserTradeoffs {
            bitrate_kbps: 12_345,
            width: 0,
            height: 0,
            fps: 0,
        };
        let knobs = knobs_for(StreamMode::Fast, user);

        // The overridden field wins...
        assert_eq!(knobs.bitrate_kbps, 12_345);
        // ...but every other mode-default field is untouched...
        let base = mode_defaults(StreamMode::Fast);
        assert_eq!(knobs.width, base.width);
        assert_eq!(knobs.height, base.height);
        assert_eq!(knobs.fps, base.fps);
        assert_eq!(knobs.codec_pref, base.codec_pref);
        // ...and the free-lunch constants never move regardless of mode or
        // user tradeoffs (they are not part of ModeKnobs at all).
        assert!(free_lunch::SPATIAL_AQ_ON);
        assert!(free_lunch::WEIGHTED_PRED_ON);
        assert_eq!(free_lunch::PRESET, "p4-quality-latency-balanced");
    }

    #[test]
    fn reserved_knobs_not_emitted() {
        // Contract check: ModeKnobs carries exactly the fields with a real
        // consumer. Constructing with this exact field set is itself the
        // assertion — adding/removing a field breaks this at compile time,
        // and there is no `pacing` / `fec_bias` field to reference.
        let knobs = ModeKnobs {
            codec_pref: CodecPref::H264,
            width: 0,
            height: 0,
            bitrate_kbps: 0,
            fps: 0,
            audio_exclusive_default: false,
        };
        assert_eq!(knobs.width, 0);
    }

    #[test]
    fn canonical_vectors_cover_all_modes_and_match_knobs_for() {
        let vectors = canonical_vectors();
        assert_eq!(vectors.len(), 3);
        for (mode, knobs) in vectors {
            assert_eq!(knobs, knobs_for(mode, UserTradeoffs::default()));
        }
    }
}
