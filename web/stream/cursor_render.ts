// G010 client-render authority decision (pure, timer-free) — TS mirror of
// client-transport/src/cursor.rs `ClientCursorAuthority`. Consumes the G009
// desktop-control cursor-authority domain (a generation-stamped CursorOwner
// admitted through an OwnershipLease) plus cursor shape ids, and decides exactly
// one visual: the host-baked cursor OR a client-rendered cursor, never both
// (no double cursor). Dedups shape application by id and restores the last shape
// across a reconnect once client authority is re-established.

import { OwnershipLease, type CursorOwner } from "./desktop_control.js"

export class ClientCursorAuthority {
    private lease = new OwnershipLease()
    private ownerSide: CursorOwner = "host"
    private latestShapeId = 0
    private appliedShapeId: number | null = null
    private visibleFlag = true

    /** Applies a generation-stamped authority message; false (unchanged) if stale. */
    applyAuthority(generation: number, owner: CursorOwner): boolean {
        if (this.lease.admit(generation)) {
            this.ownerSide = owner
            return true
        }
        return false
    }

    /** The side that currently renders the cursor. */
    owner(): CursorOwner {
        return this.ownerSide
    }

    /** Whether the client should render the cursor from shape messages. */
    renderClient(): boolean {
        return this.ownerSide === "client"
    }

    /** Whether the host-baked cursor is the visual (mutually exclusive with
     * renderClient() — the "no double cursor" invariant). */
    hostBaked(): boolean {
        return this.ownerSide === "host"
    }

    /** Records the latest cursor shape id (retained across reconnect). 0 = none. */
    noteShape(shapeId: number): void {
        this.latestShapeId = shapeId >>> 0
    }

    setVisible(visible: boolean): void {
        this.visibleFlag = visible
    }

    visible(): boolean {
        return this.visibleFlag
    }

    /** The shape id the client should render now, or null when the host bakes,
     * there is no shape, or the current shape was already applied (dedup). */
    takeShapeChange(): number | null {
        if (!this.renderClient() || this.latestShapeId === 0) return null
        if (this.appliedShapeId === this.latestShapeId) return null
        this.appliedShapeId = this.latestShapeId
        return this.latestShapeId
    }

    /** Reconnect: revert to host-baked (no double cursor during the gap), keep
     * the last shape and clear the applied marker so it re-renders on re-claim. */
    onReconnect(): void {
        this.lease.release()
        this.ownerSide = "host"
        this.appliedShapeId = null
    }
}
