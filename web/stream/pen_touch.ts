// Pen and touch input (G028) — TS mirror of common/src/pen_touch.rs. The web
// client's PointerEvent handler builds on this contact-lifetime tracker; keep
// the invariants and vectors in lockstep with the Rust `tests`.

export type TouchPhase = "down" | "move" | "up" | "cancel"
export type ContactOutcome = "started" | "updated" | "ended" | "rejected"

export const MAX_CONTACTS = 10
export const PEN_PRESSURE_MAX = 1024

/** Tracks active touch contacts and enforces their lifetime invariants. */
export class ContactTracker {
    private active = new Map<number, { x: number; y: number }>()

    activeCount(): number {
        return this.active.size
    }

    isDown(id: number): boolean {
        return this.active.has(id)
    }

    apply(id: number, phase: TouchPhase, x: number, y: number): ContactOutcome {
        switch (phase) {
            case "down":
                if (this.active.has(id) || this.active.size >= MAX_CONTACTS) return "rejected"
                this.active.set(id, { x, y })
                return "started"
            case "move": {
                const c = this.active.get(id)
                if (c === undefined) return "rejected"
                c.x = x
                c.y = y
                return "updated"
            }
            case "up":
            case "cancel":
                return this.active.delete(id) ? "ended" : "rejected"
        }
    }

    /** Cancel every active contact (reconnect / focus loss); returns the count. */
    cancelAll(): number {
        const n = this.active.size
        this.active.clear()
        return n
    }
}

/** Clamp a pen sample: pressure to 0..=PEN_PRESSURE_MAX, tilt to -90..=90. */
export function normalizePen(pressure: number, tiltX: number, tiltY: number): [number, number, number] {
    const clampTilt = (t: number) => Math.max(-90, Math.min(90, t))
    return [Math.max(0, Math.min(PEN_PRESSURE_MAX, pressure)), clampTilt(tiltX), clampTilt(tiltY)]
}

/** Transform a contact coordinate from the client rect into the stream space;
 * null when the client rect is degenerate. */
export function transformContact(
    x: number,
    y: number,
    clientW: number,
    clientH: number,
    streamW: number,
    streamH: number,
): [number, number] | null {
    if (clientW <= 0 || clientH <= 0) return null
    return [Math.trunc((x * streamW) / clientW), Math.trunc((y * streamH) / clientH)]
}
