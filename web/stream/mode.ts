// Pure mode→knobs engine — TS mirror of `transport-core/src/mode.rs`.
//
// Maps a coarse user-facing StreamMode plus optional per-field
// UserTradeoffs overrides onto the concrete ModeKnobs the Rust side wires
// into `client-transport`'s FlowConfig (bitrate_kbps / width / height / fps
// / supported_codecs) and the audio-exclusive-mode default.
//
// This module MUST stay a byte-for-byte behavioral mirror of the Rust
// engine: same mode defaults, same per-field user-override rule
// ("0 means use the mode default"), same free-lunch-levers-are-not-knobs
// contract. `tests/mode.test.mjs` is the shared parity check against the
// Rust `canonical_vectors()` table — if you change a default here, change
// it in `transport-core/src/mode.rs` too (and vice versa).
//
// No clocks, no I/O, no DOM: pure functions/data only (session_ux /
// cursor_auto pattern).

/** Coarse user-facing streaming mode. Mirrors Rust `StreamMode` (Default = 'medium'). */
export type StreamMode = 'fast' | 'medium' | 'quality'

/** Ordinal codec preference. Mirrors Rust `CodecPref`. */
export type CodecPref = 'h264' | 'hevc' | 'av1'

/**
 * Genuine user-picked tradeoffs. A `0` (or omitted) field means "use the
 * mode default"; a non-zero field overrides that single mode default
 * without affecting any other knob (free-lunch constants never move).
 */
export type UserTradeoffs = {
    bitrateKbps: number
    width: number
    height: number
    fps: number
}

/** All-zero tradeoffs: every field defers to the mode default. */
export const DEFAULT_USER_TRADEOFFS: UserTradeoffs = {
    bitrateKbps: 0,
    width: 0,
    height: 0,
    fps: 0,
}

/**
 * Concrete knobs for one streaming session. Only fields with a real
 * consumer today: `codecPref` / `width` / `height` / `bitrateKbps` / `fps`
 * map onto `client-transport`'s FlowConfig; `audioExclusiveDefault` maps
 * onto the host's audio-exclusive-mode default. Free-lunch levers (spatial
 * adaptive-quantization, encoder preset, weighted-prediction intent) are
 * reserved shipping defaults that never vary by mode or user tradeoffs, so
 * they are deliberately NOT fields on this type — mirrors Rust's private
 * `free_lunch` module, which never touches `ModeKnobs`.
 */
export type ModeKnobs = {
    codecPref: CodecPref
    width: number
    height: number
    bitrateKbps: number
    fps: number
    audioExclusiveDefault: boolean
}

/** Mode-default knobs before any UserTradeoffs override is applied. */
function modeDefaults(mode: StreamMode): ModeKnobs {
    switch (mode) {
        case 'fast':
            return {
                codecPref: 'h264',
                width: 1280,
                height: 720,
                bitrateKbps: 8_000,
                fps: 60,
                audioExclusiveDefault: true,
            }
        case 'medium':
            return {
                codecPref: 'hevc',
                width: 1920,
                height: 1080,
                bitrateKbps: 20_000,
                fps: 60,
                audioExclusiveDefault: true,
            }
        case 'quality':
            return {
                codecPref: 'av1',
                width: 3840,
                height: 2160,
                bitrateKbps: 50_000,
                fps: 60,
                audioExclusiveDefault: false,
            }
    }
}

/**
 * Map a StreamMode plus optional user overrides onto concrete ModeKnobs.
 * The mode picks defaults for every field; a non-zero `user` field
 * overrides only that field. Free-lunch constants never change — they are
 * not part of ModeKnobs at all.
 */
export function knobsFor(mode: StreamMode, user: UserTradeoffs): ModeKnobs {
    const knobs = modeDefaults(mode)

    if (user.bitrateKbps !== 0) {
        knobs.bitrateKbps = user.bitrateKbps
    }
    if (user.width !== 0) {
        knobs.width = user.width
    }
    if (user.height !== 0) {
        knobs.height = user.height
    }
    if (user.fps !== 0) {
        knobs.fps = user.fps
    }

    return knobs
}

/**
 * Canonical mode-default vectors (zeroed UserTradeoffs, i.e. no overrides)
 * for every StreamMode. This is the single shared parity source
 * `tests/mode.test.mjs` reuses so the Rust and TS defaults cannot silently
 * drift apart — mirrors Rust `canonical_vectors()`.
 */
export function canonicalVectors(): Array<[StreamMode, ModeKnobs]> {
    const modes: StreamMode[] = ['fast', 'medium', 'quality']
    return modes.map((mode) => [mode, knobsFor(mode, { ...DEFAULT_USER_TRADEOFFS })])
}
