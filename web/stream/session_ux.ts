// Session-UX stall watchdog (M4, field issue #1).
// Split into pure state (StallWatchdog, node-testable) and thin wiring in
// stream/index.ts. Detects "stream silently freezes on the last frame" from
// renderer-delivered frame timestamps alone (no Gate C correlation needed)
// and escalates a recovery ladder: indicator → IDR request(s) → ICE restart
// → full reconnect (ROADMAP 현장 이슈 백로그 #1).

// ── Actions ───────────────────────────────────────────────────────────────

export type WatchdogAction =
    | { type: 'stall' }                        // show the user-visible stall indicator
    | { type: 'recovered'; stalledMs: number } // hide it (frames flowed again)
    | { type: 'requestIdr'; attempt: number }  // 1-based
    | { type: 'restartIce' }
    | { type: 'reconnect' }                    // terminal: wiring tears the session down

export type WatchdogConfig = {
    /** Stall indicator threshold — episode start. */
    stallIndicatorMs: number
    /** First IDR request. */
    idrFirstMs: number
    /** Interval between repeated IDR requests. */
    idrRetryMs: number
    /** Total IDR attempts before moving up the ladder. */
    idrMaxAttempts: number
    /** ICE restart rung. */
    iceRestartMs: number
    /** Full reconnect rung (terminal). */
    reconnectMs: number
}

// Untuned engineering defaults (Gate B/C tuning pending): fast enough that a
// remote user sees the indicator before alt-tabbing away, slow enough that a
// single late frame does not trigger recovery machinery.
export const DEFAULT_WATCHDOG_CONFIG: WatchdogConfig = {
    stallIndicatorMs: 1000,
    idrFirstMs: 2000,
    idrRetryMs: 2000,
    idrMaxAttempts: 3,
    iceRestartMs: 10000,
    reconnectMs: 20000,
}

// Ladder rungs in strict escalation order. At most ONE rung fires per tick:
// background-tab timer throttling can make ticks arrive minutes apart, and a
// blast of stall→idr→ice→reconnect in a single tick after the tab returns
// would tear down a session that recovers on the next delivered frame.
type Rung = 'stall' | 'idr' | 'ice' | 'reconnect'

/**
 * StallWatchdog: pure, DOM/timer-free, node-testable.
 *
 * The caller drives it with wall-clock milliseconds:
 *  - `start(now)` when the stream goes live (arms the clock — a stream that
 *    never delivers a frame escalates through the same ladder),
 *  - `frameReceived(now)` for every renderer-delivered video frame,
 *  - `tick(now)` from a periodic driver (e.g. 250 ms interval),
 *  - `pause()` / `resume(now)` around document-hidden phases (rendering
 *    legitimately stops; a hidden tab must not escalate),
 *  - `stop()` on session end.
 *
 * Returned actions are commands for the wiring layer; the machine never
 * performs I/O. After `reconnect` fires the machine stays silent until the
 * next `start()`.
 */
export class StallWatchdog {
    private readonly config: WatchdogConfig
    private running = false
    private paused = false
    private lastFrameMs = 0
    private stallShown = false
    private idrAttempts = 0
    private iceRequested = false
    private reconnectRequested = false

    constructor(config?: Partial<WatchdogConfig>) {
        this.config = { ...DEFAULT_WATCHDOG_CONFIG, ...config }
    }

    /** Whether the stall indicator is currently up (UI readback). */
    get stalled(): boolean {
        return this.stallShown
    }

    /** Milliseconds without a frame, for indicator text. 0 when not running. */
    stalledMs(nowMs: number): number {
        if (!this.running || this.paused) {
            return 0
        }
        return Math.max(0, nowMs - this.lastFrameMs)
    }

    start(nowMs: number): void {
        this.running = true
        this.paused = false
        this.lastFrameMs = nowMs
        this.resetEpisode()
    }

    stop(): void {
        this.running = false
        this.stallShown = false
    }

    /** Document hidden: hold escalation, keep episode state. */
    pause(): void {
        this.paused = true
    }

    /** Document visible again: restart the clock — the hidden phase proves nothing. */
    resume(nowMs: number): void {
        if (this.paused) {
            this.paused = false
            this.lastFrameMs = nowMs
        }
    }

    frameReceived(nowMs: number): WatchdogAction[] {
        if (!this.running) {
            return []
        }
        const actions: WatchdogAction[] = []
        if (this.stallShown) {
            actions.push({ type: 'recovered', stalledMs: Math.max(0, nowMs - this.lastFrameMs) })
        }
        this.lastFrameMs = nowMs
        this.resetEpisode()
        return actions
    }

    tick(nowMs: number): WatchdogAction[] {
        if (!this.running || this.paused || this.reconnectRequested) {
            return []
        }
        const elapsed = nowMs - this.lastFrameMs
        const rung = this.nextRung(elapsed)
        if (rung == null) {
            return []
        }
        switch (rung) {
            case 'stall':
                this.stallShown = true
                return [{ type: 'stall' }]
            case 'idr':
                this.idrAttempts += 1
                return [{ type: 'requestIdr', attempt: this.idrAttempts }]
            case 'ice':
                this.iceRequested = true
                return [{ type: 'restartIce' }]
            case 'reconnect':
                this.reconnectRequested = true
                return [{ type: 'reconnect' }]
        }
    }

    private nextRung(elapsedMs: number): Rung | null {
        const c = this.config
        if (!this.stallShown) {
            return elapsedMs >= c.stallIndicatorMs ? 'stall' : null
        }
        if (this.idrAttempts < c.idrMaxAttempts) {
            const due = c.idrFirstMs + this.idrAttempts * c.idrRetryMs
            if (elapsedMs >= due) {
                return 'idr'
            }
        }
        if (!this.iceRequested && elapsedMs >= c.iceRestartMs) {
            return 'ice'
        }
        if (elapsedMs >= c.reconnectMs) {
            return 'reconnect'
        }
        return null
    }

    private resetEpisode(): void {
        this.stallShown = false
        this.idrAttempts = 0
        this.iceRequested = false
        this.reconnectRequested = false
    }
}
