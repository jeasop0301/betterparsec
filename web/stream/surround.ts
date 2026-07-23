// Surround channel-layout negotiation and Opus channel identity (G027) — TS
// mirror of common/src/surround.rs. Keep the channel-identity vectors and the
// negotiation truth table in lockstep with the Rust `tests`.

export type Speaker =
    | "front_left"
    | "front_right"
    | "front_center"
    | "lfe"
    | "back_left"
    | "back_right"
    | "side_left"
    | "side_right"

export type ChannelLayout = "mono" | "stereo" | "surround51" | "surround71"

/** Number of channels in a layout. */
export function channelCount(layout: ChannelLayout): number {
    switch (layout) {
        case "mono":
            return 1
        case "stereo":
            return 2
        case "surround51":
            return 6
        case "surround71":
            return 8
    }
}

/** Per-channel speaker identity in Opus/Vorbis channel order (RFC 7845). */
export function opusOrder(layout: ChannelLayout): Speaker[] {
    switch (layout) {
        case "mono":
            return ["front_center"]
        case "stereo":
            return ["front_left", "front_right"]
        case "surround51":
            return ["front_left", "front_center", "front_right", "back_left", "back_right", "lfe"]
        case "surround71":
            return ["front_left", "front_center", "front_right", "side_left", "side_right", "back_left", "back_right", "lfe"]
    }
}

const LAYOUTS_DESC: ChannelLayout[] = ["surround71", "surround51", "stereo", "mono"]

/** Select the largest layout supported by both the source and the sink's max
 * channel count; `downgraded` is true when it is a downgrade from `source`. */
export function negotiate(source: ChannelLayout, sinkMaxChannels: number): { selected: ChannelLayout; downgraded: boolean } {
    const cap = Math.min(channelCount(source), sinkMaxChannels)
    const selected = LAYOUTS_DESC.find((l) => channelCount(l) <= cap) ?? "mono"
    return { selected, downgraded: channelCount(selected) < channelCount(source) }
}
