// Privacy-safe microphone capture gate (G026) — TS mirror of common/src/mic.rs.
// The web client's getUserMedia flow builds on this gate; keep the invariants in
// lockstep with the Rust `tests`.

export type MicPermission = "prompt" | "granted" | "denied"

/** Privacy-safe microphone capture gate. */
export class MicrophoneGate {
    private permissionState: MicPermission = "prompt"
    private muted = false
    private device: number | null = null
    private sessionActive = false

    /** Audio may be captured only when permission is granted, not muted, a
     * device is present, and the session is active. */
    canCapture(): boolean {
        return this.permissionState === "granted" && !this.muted && this.device !== null && this.sessionActive
    }

    permission(): MicPermission {
        return this.permissionState
    }

    isMuted(): boolean {
        return this.muted
    }

    setPermission(permission: MicPermission): void {
        this.permissionState = permission
    }

    /** Mute — fail-closed: capture stops immediately. */
    mute(): void {
        this.muted = true
    }

    unmute(): void {
        this.muted = false
    }

    setDevice(device: number | null): void {
        this.device = device
    }

    setSessionActive(active: boolean): void {
        this.sessionActive = active
    }

    /** Any uncertainty: fail closed by muting. */
    failClosed(): void {
        this.muted = true
    }

    /** Reconnect: fail closed — the session goes inactive and the device is
     * dropped; permission persists. */
    onReconnect(): void {
        this.sessionActive = false
        this.device = null
    }
}
