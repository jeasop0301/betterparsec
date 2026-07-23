//! Surround channel-layout negotiation and Opus channel identity (G026).
//! Negotiates the audio channel layout against what the client can render and
//! pins the per-channel speaker identity in Opus/Vorbis channel order (RFC 7845
//! §5.1.1.4), so a 5.1/7.1 stream's channels always land on the right speakers
//! and an unsupported layout downgrades visibly.
//!
//! Pure and headless. The actual Opus encode/decode, the ITU-R BS.775 downmix,
//! and WASAPI multichannel render (streamer/native/web audio) build on this.

/// A physical speaker position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    FrontLeft,
    FrontRight,
    FrontCenter,
    Lfe,
    BackLeft,
    BackRight,
    SideLeft,
    SideRight,
}

/// A supported channel layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelLayout {
    Mono,
    Stereo,
    /// 5.1 (6 channels).
    Surround51,
    /// 7.1 (8 channels).
    Surround71,
}

impl ChannelLayout {
    /// Number of channels in this layout.
    pub const fn channel_count(self) -> u8 {
        match self {
            ChannelLayout::Mono => 1,
            ChannelLayout::Stereo => 2,
            ChannelLayout::Surround51 => 6,
            ChannelLayout::Surround71 => 8,
        }
    }

    /// The per-channel speaker identity in Opus/Vorbis channel order
    /// (RFC 7845 §5.1.1.4). The slice length equals [`Self::channel_count`].
    pub fn opus_order(self) -> &'static [Speaker] {
        use Speaker::{
            BackLeft, BackRight, FrontCenter, FrontLeft, FrontRight, Lfe, SideLeft, SideRight,
        };
        match self {
            ChannelLayout::Mono => &[FrontCenter],
            ChannelLayout::Stereo => &[FrontLeft, FrontRight],
            ChannelLayout::Surround51 => {
                &[FrontLeft, FrontCenter, FrontRight, BackLeft, BackRight, Lfe]
            }
            ChannelLayout::Surround71 => &[
                FrontLeft,
                FrontCenter,
                FrontRight,
                SideLeft,
                SideRight,
                BackLeft,
                BackRight,
                Lfe,
            ],
        }
    }
}

/// Layouts largest-to-smallest, for negotiation.
const LAYOUTS_DESC: [ChannelLayout; 4] = [
    ChannelLayout::Surround71,
    ChannelLayout::Surround51,
    ChannelLayout::Stereo,
    ChannelLayout::Mono,
];

/// Negotiate the layout to send: the largest layout whose channel count is at
/// most both the `source` layout and the sink's `sink_max_channels`. Returns the
/// selected layout and whether it is a downgrade from `source` (a visible
/// downgrade the caller reports/logs).
pub fn negotiate(source: ChannelLayout, sink_max_channels: u8) -> (ChannelLayout, bool) {
    let cap = source.channel_count().min(sink_max_channels);
    let selected = LAYOUTS_DESC
        .into_iter()
        .find(|l| l.channel_count() <= cap)
        .unwrap_or(ChannelLayout::Mono);
    let downgraded = selected.channel_count() < source.channel_count();
    (selected, downgraded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Speaker::{
        BackLeft, BackRight, FrontCenter, FrontLeft, FrontRight, Lfe, SideLeft, SideRight,
    };

    #[test]
    fn channel_counts() {
        assert_eq!(ChannelLayout::Mono.channel_count(), 1);
        assert_eq!(ChannelLayout::Stereo.channel_count(), 2);
        assert_eq!(ChannelLayout::Surround51.channel_count(), 6);
        assert_eq!(ChannelLayout::Surround71.channel_count(), 8);
    }

    #[test]
    fn opus_channel_identity_vectors() {
        // Each layout's Opus order length matches its channel count and pins the
        // exact per-channel speaker identity (RFC 7845).
        for layout in LAYOUTS_DESC {
            assert_eq!(layout.opus_order().len(), layout.channel_count() as usize);
        }
        assert_eq!(ChannelLayout::Stereo.opus_order(), &[FrontLeft, FrontRight]);
        assert_eq!(
            ChannelLayout::Surround51.opus_order(),
            &[FrontLeft, FrontCenter, FrontRight, BackLeft, BackRight, Lfe]
        );
        assert_eq!(
            ChannelLayout::Surround71.opus_order(),
            &[
                FrontLeft,
                FrontCenter,
                FrontRight,
                SideLeft,
                SideRight,
                BackLeft,
                BackRight,
                Lfe
            ]
        );
    }

    #[test]
    fn negotiation_selects_the_largest_mutually_supported_layout() {
        // Full mutual 7.1.
        assert_eq!(
            negotiate(ChannelLayout::Surround71, 8),
            (ChannelLayout::Surround71, false)
        );
        // 7.1 source, stereo sink -> stereo, downgraded.
        assert_eq!(
            negotiate(ChannelLayout::Surround71, 2),
            (ChannelLayout::Stereo, true)
        );
        // 7.1 source, 6-channel sink -> 5.1, downgraded.
        assert_eq!(
            negotiate(ChannelLayout::Surround71, 6),
            (ChannelLayout::Surround51, true)
        );
        // 5.1 source, 8-channel sink -> 5.1 (source caps), not a downgrade.
        assert_eq!(
            negotiate(ChannelLayout::Surround51, 8),
            (ChannelLayout::Surround51, false)
        );
        // Stereo source, big sink -> stereo, not a downgrade.
        assert_eq!(
            negotiate(ChannelLayout::Stereo, 8),
            (ChannelLayout::Stereo, false)
        );
        // A sink that can only do 1 channel -> mono, downgraded.
        assert_eq!(
            negotiate(ChannelLayout::Surround51, 1),
            (ChannelLayout::Mono, true)
        );
    }
}
