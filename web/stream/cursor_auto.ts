// Host-authority auto mouse mode — M4 cursor P1
// (docs/design/cursor-channel.md §3).
//
// Pure and timer-free (session_ux/qu_overlay pattern): the wiring feeds
// host cursor visibility samples (from the `cursor` DataChannel) plus a
// wall clock, and executes the returned actions:
//   lock   → enter pointer lock + relative input (FPS aim; 0 cursors)
//   unlock → exit pointer lock + follow (absolute) input + `cursor:none`
//            over the video (the baked host cursor is the visual in P1)
//
// Transitions commit only after the host visibility has been stable for
// `hysteresisMs` (game loading screens flicker the cursor; a 150 ms
// debounce prevents lock/unlock thrash). The host only sends messages on
// change, so the wiring must also drive `tick` to commit a pending
// transition once the hysteresis window elapses.

export type CursorAutoAction = { type: 'lock' } | { type: 'unlock' }

export type CursorAutoConfig = {
    /** Stability window before a visibility change switches modes. */
    hysteresisMs: number
}

export const DEFAULT_CURSOR_AUTO_CONFIG: CursorAutoConfig = {
    hysteresisMs: 150,
}

type Mode = 'locked' | 'unlocked'

export class CursorAutoMode {
    private readonly config: CursorAutoConfig
    /** Desktop default: cursor visible → absolute input, no lock. */
    private mode: Mode = 'unlocked'
    /** Last host-reported visibility (starts visible = unlocked). */
    private visible = true
    /** When the current visibility started disagreeing with `mode`. */
    private pendingSinceMs: number | null = null

    constructor(config?: Partial<CursorAutoConfig>) {
        this.config = { ...DEFAULT_CURSOR_AUTO_CONFIG, ...config }
    }

    /** Current committed mode (UI/wiring readback). */
    get locked(): boolean {
        return this.mode === 'locked'
    }

    /** Host cursor visibility sample from the `cursor` channel. */
    onVisibility(visible: boolean, nowMs: number): CursorAutoAction | null {
        this.visible = visible
        return this.evaluate(nowMs)
    }

    /** Periodic driver: commits a pending transition after hysteresis. */
    tick(nowMs: number): CursorAutoAction | null {
        return this.evaluate(nowMs)
    }

    private evaluate(nowMs: number): CursorAutoAction | null {
        const desired: Mode = this.visible ? 'unlocked' : 'locked'
        if (desired === this.mode) {
            // Agreement (including flicker that returned within the
            // window): nothing pending.
            this.pendingSinceMs = null
            return null
        }
        if (this.pendingSinceMs == null) {
            this.pendingSinceMs = nowMs
        }
        if (nowMs - this.pendingSinceMs < this.config.hysteresisMs) {
            return null
        }
        this.mode = desired
        this.pendingSinceMs = null
        return desired === 'locked' ? { type: 'lock' } : { type: 'unlock' }
    }
}
