//! Media color / display capability schema (G017). The contract predecessor for
//! the media train: HDR10 (G018), AV1 & HEVC 4:4:4 (G019), and 4K120/144 + VRR
//! (G020) all negotiate against this. It describes a video mode at full fidelity
//! — codec, chroma, bit depth, primaries/transfer/matrix/range, HDR mastering
//! metadata (maxCLL/maxFALL/luminance), refresh, and VRR — and reconciles a
//! requested mode against both peers' capabilities into a supported mode plus a
//! typed downgrade reason (the "downgrade matrix"), with a deterministic SDR
//! fallback when HDR is unsupported.
//!
//! This is finer-grained than the coarse shipped `StreamColorspace` /
//! `StreamSettings.hdr` and complements them; it does not replace the existing
//! moonlight VideoFormats negotiation. All descriptor codes are stable and
//! forward-compatible (an unknown code decodes to `None`). Keep any TS mirror
//! byte/truth-pinned against [`tests`]. The typed downgrade reason is the shared
//! G009 [`DowngradeReason`].

use crate::desktop_control::DowngradeReason;

/// Video codec, best-to-worst preference order AV1 > HEVC > H264.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
}

impl VideoCodec {
    pub const fn to_u8(self) -> u8 {
        match self {
            VideoCodec::H264 => 1,
            VideoCodec::Hevc => 2,
            VideoCodec::Av1 => 3,
        }
    }
    pub const fn from_u8(code: u8) -> Option<VideoCodec> {
        match code {
            1 => Some(VideoCodec::H264),
            2 => Some(VideoCodec::Hevc),
            3 => Some(VideoCodec::Av1),
            _ => None,
        }
    }
    /// Capability bit for this codec (H264=1, HEVC=2, AV1=4).
    pub const fn bit(self) -> u8 {
        match self {
            VideoCodec::H264 => 0x01,
            VideoCodec::Hevc => 0x02,
            VideoCodec::Av1 => 0x04,
        }
    }
}

/// Chroma subsampling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromaSubsampling {
    Yuv420,
    Yuv444,
}

impl ChromaSubsampling {
    pub const fn to_u8(self) -> u8 {
        match self {
            ChromaSubsampling::Yuv420 => 0,
            ChromaSubsampling::Yuv444 => 1,
        }
    }
    pub const fn from_u8(code: u8) -> Option<ChromaSubsampling> {
        match code {
            0 => Some(ChromaSubsampling::Yuv420),
            1 => Some(ChromaSubsampling::Yuv444),
            _ => None,
        }
    }
}

/// Bit depth per component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitDepth {
    Eight,
    Ten,
}

impl BitDepth {
    pub const fn to_u8(self) -> u8 {
        match self {
            BitDepth::Eight => 8,
            BitDepth::Ten => 10,
        }
    }
    pub const fn from_u8(code: u8) -> Option<BitDepth> {
        match code {
            8 => Some(BitDepth::Eight),
            10 => Some(BitDepth::Ten),
            _ => None,
        }
    }
}

/// Color primaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPrimaries {
    Bt709,
    Bt2020,
}

impl ColorPrimaries {
    pub const fn to_u8(self) -> u8 {
        match self {
            ColorPrimaries::Bt709 => 1,
            ColorPrimaries::Bt2020 => 9,
        }
    }
    pub const fn from_u8(code: u8) -> Option<ColorPrimaries> {
        match code {
            1 => Some(ColorPrimaries::Bt709),
            9 => Some(ColorPrimaries::Bt2020),
            _ => None,
        }
    }
}

/// Transfer function. `Bt709` is SDR; `Pq`/`Hlg` are HDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFunction {
    Bt709,
    Pq,
    Hlg,
}

impl TransferFunction {
    pub const fn to_u8(self) -> u8 {
        match self {
            TransferFunction::Bt709 => 1,
            TransferFunction::Pq => 16,
            TransferFunction::Hlg => 18,
        }
    }
    pub const fn from_u8(code: u8) -> Option<TransferFunction> {
        match code {
            1 => Some(TransferFunction::Bt709),
            16 => Some(TransferFunction::Pq),
            18 => Some(TransferFunction::Hlg),
            _ => None,
        }
    }
    /// Whether this transfer function is an HDR one.
    pub const fn is_hdr(self) -> bool {
        matches!(self, TransferFunction::Pq | TransferFunction::Hlg)
    }
}

/// Matrix coefficients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatrixCoefficients {
    Bt709,
    Bt2020Ncl,
}

impl MatrixCoefficients {
    pub const fn to_u8(self) -> u8 {
        match self {
            MatrixCoefficients::Bt709 => 1,
            MatrixCoefficients::Bt2020Ncl => 9,
        }
    }
    pub const fn from_u8(code: u8) -> Option<MatrixCoefficients> {
        match code {
            1 => Some(MatrixCoefficients::Bt709),
            9 => Some(MatrixCoefficients::Bt2020Ncl),
            _ => None,
        }
    }
}

/// Color range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorRange {
    Limited,
    Full,
}

impl ColorRange {
    pub const fn to_u8(self) -> u8 {
        match self {
            ColorRange::Limited => 0,
            ColorRange::Full => 1,
        }
    }
    pub const fn from_u8(code: u8) -> Option<ColorRange> {
        match code {
            0 => Some(ColorRange::Limited),
            1 => Some(ColorRange::Full),
            _ => None,
        }
    }
}

/// HDR mastering metadata. Luminance is in 0.0001 cd/m² units (CTA-861), maxCLL
/// and maxFALL in cd/m².
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdrMetadata {
    pub max_cll: u16,
    pub max_fall: u16,
    pub max_luminance: u32,
    pub min_luminance: u32,
}

impl HdrMetadata {
    /// Fixed encoded size (`u16 maxCLL | u16 maxFALL | u32 maxLum | u32 minLum`,
    /// little-endian).
    pub const WIRE_LEN: usize = 12;

    /// Encode the HDR mastering metadata for transport alongside the stream.
    pub fn encode(&self) -> [u8; Self::WIRE_LEN] {
        let mut out = [0u8; Self::WIRE_LEN];
        out[0..2].copy_from_slice(&self.max_cll.to_le_bytes());
        out[2..4].copy_from_slice(&self.max_fall.to_le_bytes());
        out[4..8].copy_from_slice(&self.max_luminance.to_le_bytes());
        out[8..12].copy_from_slice(&self.min_luminance.to_le_bytes());
        out
    }

    /// Decode HDR mastering metadata; `None` if shorter than [`Self::WIRE_LEN`].
    /// Trailing bytes are tolerated.
    pub fn decode(bytes: &[u8]) -> Option<HdrMetadata> {
        if bytes.len() < Self::WIRE_LEN {
            return None;
        }
        Some(HdrMetadata {
            max_cll: u16::from_le_bytes([bytes[0], bytes[1]]),
            max_fall: u16::from_le_bytes([bytes[2], bytes[3]]),
            max_luminance: u32::from_le_bytes(bytes[4..8].try_into().ok()?),
            min_luminance: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
        })
    }
}

/// A fully described video mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoMode {
    pub codec: VideoCodec,
    pub chroma: ChromaSubsampling,
    pub bit_depth: BitDepth,
    pub primaries: ColorPrimaries,
    pub transfer: TransferFunction,
    pub matrix: MatrixCoefficients,
    pub range: ColorRange,
    pub hdr: Option<HdrMetadata>,
    pub width: u16,
    pub height: u16,
    pub refresh_mhz: u32,
    pub vrr: bool,
}

impl VideoMode {
    /// A plain 1080p60 SDR H.264 4:2:0 8-bit mode — the always-supported floor.
    pub const fn sdr_baseline() -> VideoMode {
        VideoMode {
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            bit_depth: BitDepth::Eight,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt709,
            matrix: MatrixCoefficients::Bt709,
            range: ColorRange::Limited,
            hdr: None,
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
            vrr: false,
        }
    }
}

/// One peer's media capabilities. `codecs` is the OR of [`VideoCodec::bit`]
/// values; H.264 is always assumed present as the floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub codecs: u8,
    pub chroma444: bool,
    pub bit10: bool,
    pub hdr: bool,
    pub max_refresh_mhz: u32,
    pub vrr: bool,
}

impl Capabilities {
    /// Whether this peer supports the codec. H.264 is the unconditional floor
    /// (see `codecs` doc), so it is always supported even if the bit is unset;
    /// this keeps a plain H.264 request from being reported as a downgrade.
    pub const fn supports_codec(&self, codec: VideoCodec) -> bool {
        matches!(codec, VideoCodec::H264) || self.codecs & codec.bit() != 0
    }
}

/// Reconcile a `requested` mode against both peers' capabilities. Returns the
/// supported mode plus a typed downgrade reason (`None` when the exact request
/// is satisfied). HDR that is not mutually supported falls back deterministically
/// to SDR (BT.709 primaries/transfer/matrix, Limited range, no metadata).
pub fn negotiate(
    client: &Capabilities,
    host: &Capabilities,
    requested: &VideoMode,
) -> (VideoMode, Option<DowngradeReason>) {
    let mut out = *requested;
    let mut reason: Option<DowngradeReason> = None;

    // Codec: keep if both support it, else the best codec both share (H.264 floor).
    if !(client.supports_codec(requested.codec) && host.supports_codec(requested.codec)) {
        out.codec = best_common_codec(client.codecs & host.codecs);
        reason.get_or_insert(DowngradeReason::CapabilityMismatch);
    }

    // Chroma 4:4:4.
    if matches!(requested.chroma, ChromaSubsampling::Yuv444)
        && !(client.chroma444 && host.chroma444)
    {
        out.chroma = ChromaSubsampling::Yuv420;
        reason.get_or_insert(DowngradeReason::CapabilityMismatch);
    }

    // 10-bit.
    if matches!(requested.bit_depth, BitDepth::Ten) && !(client.bit10 && host.bit10) {
        out.bit_depth = BitDepth::Eight;
        reason.get_or_insert(DowngradeReason::CapabilityMismatch);
    }

    // HDR -> deterministic SDR fallback when not mutually supported.
    let wants_hdr = requested.transfer.is_hdr() || requested.hdr.is_some();
    if wants_hdr && !(client.hdr && host.hdr) {
        out.hdr = None;
        out.primaries = ColorPrimaries::Bt709;
        out.transfer = TransferFunction::Bt709;
        out.matrix = MatrixCoefficients::Bt709;
        out.range = ColorRange::Limited;
        reason.get_or_insert(DowngradeReason::Unsupported);
    }

    // Refresh clamp to the mutual maximum.
    let refresh_cap = client.max_refresh_mhz.min(host.max_refresh_mhz);
    if requested.refresh_mhz > refresh_cap {
        out.refresh_mhz = refresh_cap;
        reason.get_or_insert(DowngradeReason::CapabilityMismatch);
    }

    // VRR.
    if requested.vrr && !(client.vrr && host.vrr) {
        out.vrr = false;
        reason.get_or_insert(DowngradeReason::CapabilityMismatch);
    }

    (out, reason)
}

/// The best codec present in a shared capability bitset (AV1 > HEVC > H264);
/// H.264 is the floor when nothing else is shared.
pub fn best_common_codec(shared: u8) -> VideoCodec {
    if shared & VideoCodec::Av1.bit() != 0 {
        VideoCodec::Av1
    } else if shared & VideoCodec::Hevc.bit() != 0 {
        VideoCodec::Hevc
    } else {
        VideoCodec::H264
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(
        codecs: u8,
        chroma444: bool,
        bit10: bool,
        hdr: bool,
        refresh: u32,
        vrr: bool,
    ) -> Capabilities {
        Capabilities {
            codecs,
            chroma444,
            bit10,
            hdr,
            max_refresh_mhz: refresh,
            vrr,
        }
    }

    const AV1: u8 = 0x04;
    const HEVC: u8 = 0x02;
    const H264: u8 = 0x01;

    fn hdr_av1_444_10bit_4k144() -> VideoMode {
        VideoMode {
            codec: VideoCodec::Av1,
            chroma: ChromaSubsampling::Yuv444,
            bit_depth: BitDepth::Ten,
            primaries: ColorPrimaries::Bt2020,
            transfer: TransferFunction::Pq,
            matrix: MatrixCoefficients::Bt2020Ncl,
            range: ColorRange::Full,
            hdr: Some(HdrMetadata {
                max_cll: 1000,
                max_fall: 400,
                max_luminance: 10_000_000,
                min_luminance: 1,
            }),
            width: 3840,
            height: 2160,
            refresh_mhz: 144_000,
            vrr: true,
        }
    }

    #[test]
    fn h264_is_the_unconditional_floor() {
        // A capability set that omits the H.264 bit still "supports" H.264, so a
        // plain H.264 request is never reported as a downgrade.
        let no_bits = caps(0, false, false, false, 60_000, false);
        assert!(no_bits.supports_codec(VideoCodec::H264));
        assert!(!no_bits.supports_codec(VideoCodec::Av1));

        let h264_mode = VideoMode::sdr_baseline();
        let (out, reason) = negotiate(&no_bits, &no_bits, &h264_mode);
        assert_eq!(out.codec, VideoCodec::H264);
        assert_eq!(reason, None);
    }
    #[test]
    fn descriptor_codes_round_trip_and_reject_unknown() {
        assert_eq!(
            VideoCodec::from_u8(VideoCodec::Av1.to_u8()),
            Some(VideoCodec::Av1)
        );
        assert_eq!(VideoCodec::from_u8(0), None);
        assert_eq!(BitDepth::from_u8(10), Some(BitDepth::Ten));
        assert_eq!(BitDepth::from_u8(12), None);
        assert_eq!(TransferFunction::from_u8(16), Some(TransferFunction::Pq));
        assert!(TransferFunction::Pq.is_hdr());
        assert!(!TransferFunction::Bt709.is_hdr());
        assert_eq!(ColorPrimaries::from_u8(9), Some(ColorPrimaries::Bt2020));
        assert_eq!(ColorPrimaries::from_u8(2), None);
        assert_eq!(
            ChromaSubsampling::from_u8(1),
            Some(ChromaSubsampling::Yuv444)
        );
        assert_eq!(
            MatrixCoefficients::from_u8(9),
            Some(MatrixCoefficients::Bt2020Ncl)
        );
        assert_eq!(ColorRange::from_u8(1), Some(ColorRange::Full));
    }

    #[test]
    fn full_match_negotiates_the_exact_mode() {
        let full = caps(AV1 | HEVC | H264, true, true, true, 144_000, true);
        let req = hdr_av1_444_10bit_4k144();
        let (mode, reason) = negotiate(&full, &full, &req);
        assert_eq!(reason, None);
        assert_eq!(mode, req);
    }

    #[test]
    fn hdr_falls_back_deterministically_to_sdr() {
        let hdr_client = caps(AV1 | HEVC | H264, true, true, true, 144_000, true);
        let sdr_host = caps(AV1 | HEVC | H264, true, true, false, 144_000, true);
        let (mode, reason) = negotiate(&hdr_client, &sdr_host, &hdr_av1_444_10bit_4k144());
        assert_eq!(reason, Some(DowngradeReason::Unsupported));
        assert_eq!(mode.hdr, None);
        assert_eq!(mode.transfer, TransferFunction::Bt709);
        assert_eq!(mode.primaries, ColorPrimaries::Bt709);
        assert_eq!(mode.matrix, MatrixCoefficients::Bt709);
        assert_eq!(mode.range, ColorRange::Limited);
        // Codec/chroma/bit-depth stay (both support them).
        assert_eq!(mode.codec, VideoCodec::Av1);
        assert_eq!(mode.chroma, ChromaSubsampling::Yuv444);
    }

    #[test]
    fn codec_chroma_bitdepth_refresh_vrr_downgrade_matrix() {
        // Host only supports HEVC (no AV1), no 4:4:4, no 10-bit, 120Hz max, no VRR.
        let client = caps(AV1 | HEVC | H264, true, true, false, 144_000, true);
        let host = caps(HEVC | H264, false, false, false, 120_000, false);
        let mut req = hdr_av1_444_10bit_4k144();
        req.transfer = TransferFunction::Bt709; // SDR request to isolate the other axes
        req.hdr = None;
        let (mode, reason) = negotiate(&client, &host, &req);
        assert_eq!(reason, Some(DowngradeReason::CapabilityMismatch));
        assert_eq!(mode.codec, VideoCodec::Hevc); // AV1 -> best shared HEVC
        assert_eq!(mode.chroma, ChromaSubsampling::Yuv420); // 444 -> 420
        assert_eq!(mode.bit_depth, BitDepth::Eight); // 10 -> 8
        assert_eq!(mode.refresh_mhz, 120_000); // clamped
        assert!(!mode.vrr); // VRR off
    }

    #[test]
    fn baseline_h264_survives_when_nothing_is_shared_above_it() {
        // Client AV1-only, host H264-only: no overlap above H.264 floor.
        let client = caps(AV1 | H264, false, false, false, 60_000, false);
        let host = caps(H264, false, false, false, 60_000, false);
        let mut req = VideoMode::sdr_baseline();
        req.codec = VideoCodec::Av1;
        let (mode, reason) = negotiate(&client, &host, &req);
        assert_eq!(mode.codec, VideoCodec::H264);
        assert_eq!(reason, Some(DowngradeReason::CapabilityMismatch));
        assert_eq!(best_common_codec(0), VideoCodec::H264);
    }

    #[test]
    fn hdr_metadata_wire_round_trip_and_truncation() {
        let hdr = HdrMetadata {
            max_cll: 1000,
            max_fall: 400,
            max_luminance: 10_000_000,
            min_luminance: 5,
        };
        let bytes = hdr.encode();
        assert_eq!(
            bytes,
            [
                0xE8, 0x03, // maxCLL 1000 LE
                0x90, 0x01, // maxFALL 400 LE
                0x80, 0x96, 0x98, 0x00, // maxLum 10_000_000 LE
                0x05, 0x00, 0x00, 0x00, // minLum 5 LE
            ]
        );
        assert_eq!(HdrMetadata::decode(&bytes), Some(hdr));
        // Trailing tolerated; truncation rejected.
        let mut trailing = bytes.to_vec();
        trailing.push(0xFF);
        assert_eq!(HdrMetadata::decode(&trailing), Some(hdr));
        assert!(HdrMetadata::decode(&bytes[..HdrMetadata::WIRE_LEN - 1]).is_none());
    }
}
