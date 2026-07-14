import assert from "node:assert/strict"
import test from "node:test"

import { QuOverlayState } from "../dist/stream/video/qu_overlay.js"

// ── Helpers ───────────────────────────────────────────────────────────────

function makeConfig(tileW, tileH, gridCols, gridRows, epoch) {
    return { kind: 0x01, tileW, tileH, gridCols, gridRows, epoch }
}

function makeTile(epoch, col, row, payload = new Uint8Array([0xAA]), crc32Bgra = 0) {
    return { kind: 0x02, epoch, col, row, format: 0, flags: 0, crc32Bgra, payload }
}

function makeInvalidate(epoch, tiles) {
    return { kind: 0x03, epoch, tiles }
}

function makeEpoch(epoch) {
    return { kind: 0x04, epoch }
}

// ── Config → resize + clearAll ────────────────────────────────────────────

test("QU_CONFIG produces resize then clearAll", () => {
    const state = new QuOverlayState(1920, 1080)
    const actions = state.apply(makeConfig(128, 128, 15, 9, 7))
    assert.equal(actions.length, 2)
    assert.equal(actions[0].type, 'resize')
    assert.equal(actions[0].pixelW, 1920)
    assert.equal(actions[0].pixelH, 1080)
    assert.equal(actions[1].type, 'clearAll')
})

test("QU_CONFIG sets epoch", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 42))
    assert.equal(state.currentEpoch, 42)
})

// ── Tile apply → drawTile action ──────────────────────────────────────────

test("QU_TILE in correct epoch produces drawTile action", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    const actions = state.apply(makeTile(7, 3, 2))
    assert.equal(actions.length, 1)
    const a = actions[0]
    assert.equal(a.type, 'drawTile')
    assert.equal(a.col, 3)
    assert.equal(a.row, 2)
    assert.equal(a.x, 3 * 128)   // 384
    assert.equal(a.y, 2 * 128)   // 256
    assert.equal(a.w, 128)
    assert.equal(a.h, 128)
})

test("QU_TILE records tile in applied set", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    state.apply(makeTile(7, 0, 0))
    assert.equal(state.appliedCount, 1)
})

// ── Stale epoch tile → no action ──────────────────────────────────────────

test("QU_TILE with stale epoch produces no actions", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    const actions = state.apply(makeTile(6, 1, 1))   // epoch 6, current is 7
    assert.equal(actions.length, 0)
    assert.equal(state.appliedCount, 0)
})

test("QU_TILE before any config produces no actions", () => {
    const state = new QuOverlayState(1920, 1080)
    const actions = state.apply(makeTile(7, 0, 0))
    assert.equal(actions.length, 0)
})

// ── QU_INVALIDATE: only clears applied tiles ──────────────────────────────

test("QU_INVALIDATE clears only applied tiles", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    state.apply(makeTile(7, 1, 0))  // apply (1,0)
    state.apply(makeTile(7, 2, 0))  // apply (2,0)
    // Invalidate (1,0) and (3,0) — only (1,0) was applied
    const actions = state.apply(makeInvalidate(7, [{ col: 1, row: 0 }, { col: 3, row: 0 }]))
    assert.equal(actions.length, 1, "only the applied tile should produce clearRect")
    assert.equal(actions[0].type, 'clearRect')
    assert.equal(actions[0].x, 1 * 128)
    assert.equal(actions[0].y, 0)
    assert.equal(actions[0].w, 128)
    assert.equal(actions[0].h, 128)
    assert.equal(state.appliedCount, 1, "(2,0) still applied")
})

test("QU_INVALIDATE with stale epoch produces no actions", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    state.apply(makeTile(7, 0, 0))
    const actions = state.apply(makeInvalidate(6, [{ col: 0, row: 0 }]))
    assert.equal(actions.length, 0)
    assert.equal(state.appliedCount, 1, "tile still present after stale invalidate")
})

test("QU_INVALIDATE all-unapplied produces no actions", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    const actions = state.apply(makeInvalidate(7, [{ col: 5, row: 5 }]))
    assert.equal(actions.length, 0)
})

// ── QU_EPOCH → clearAll ───────────────────────────────────────────────────

test("QU_EPOCH produces clearAll and advances epoch", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    state.apply(makeTile(7, 0, 0))
    const actions = state.apply(makeEpoch(8))
    assert.equal(actions.length, 1)
    assert.equal(actions[0].type, 'clearAll')
    assert.equal(state.currentEpoch, 8)
    assert.equal(state.appliedCount, 0, "applied map cleared by epoch advance")
})

test("after QU_EPOCH, tile with new epoch is accepted", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    state.apply(makeEpoch(8))
    const actions = state.apply(makeTile(8, 0, 0))
    assert.equal(actions.length, 1)
    assert.equal(actions[0].type, 'drawTile')
})

// ── Edge tile clipping ────────────────────────────────────────────────────
// Frame: 1920 × 1080.  Tile: 128×128.  Grid: cols=15, rows=9.
// col 14: x=14*128=1792, w=min(128, 1920-1792)=128  (no horizontal clip: 15*128=1920 exactly)
// row  8: y= 8*128=1024, h=min(128, 1080-1024)= 56  (vertical clip: 1080 mod 128 = 56)

test("edge tile col 14 row 8 clips to w=128, h=56", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 1))
    const actions = state.apply(makeTile(1, 14, 8))
    assert.equal(actions.length, 1)
    const a = actions[0]
    assert.equal(a.type, 'drawTile')
    assert.equal(a.x, 1792)
    assert.equal(a.y, 1024)
    assert.equal(a.w, 128)
    assert.equal(a.h, 56)
})

test("edge tile col 14 row 8 clearRect from invalidate clips identically", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 1))
    state.apply(makeTile(1, 14, 8))
    const actions = state.apply(makeInvalidate(1, [{ col: 14, row: 8 }]))
    assert.equal(actions.length, 1)
    const a = actions[0]
    assert.equal(a.type, 'clearRect')
    assert.equal(a.x, 1792)
    assert.equal(a.y, 1024)
    assert.equal(a.w, 128)
    assert.equal(a.h, 56)
})

// ── Tile replacement = plain drawTile, no pre-clear ───────────────────────

test("replacing an applied tile emits drawTile (no clearRect)", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    state.apply(makeTile(7, 2, 2))
    const actions = state.apply(makeTile(7, 2, 2))  // replace same tile
    assert.equal(actions.length, 1)
    assert.equal(actions[0].type, 'drawTile')
    assert.equal(state.appliedCount, 1, "still only one tile record")
})

// ── revoke ────────────────────────────────────────────────────────────────

test("revoke removes tile from applied set", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    state.apply(makeTile(7, 0, 0))
    assert.equal(state.appliedCount, 1)
    state.revoke(0, 0)
    assert.equal(state.appliedCount, 0)
})

test("after revoke, INVALIDATE for that tile produces no clearRect", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    state.apply(makeTile(7, 5, 3))
    state.revoke(5, 3)
    const actions = state.apply(makeInvalidate(7, [{ col: 5, row: 3 }]))
    assert.equal(actions.length, 0, "revoked tile must not produce clearRect on invalidate")
})

// ── Multiple tiles + partial invalidate ───────────────────────────────────

test("applying 3 tiles then invalidating 2 leaves 1 applied", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    state.apply(makeTile(7, 0, 0))
    state.apply(makeTile(7, 1, 0))
    state.apply(makeTile(7, 2, 0))
    assert.equal(state.appliedCount, 3)
    const actions = state.apply(makeInvalidate(7, [{ col: 0, row: 0 }, { col: 2, row: 0 }]))
    assert.equal(actions.length, 2)
    assert.equal(state.appliedCount, 1)
})

// ── Out-of-bounds tile guard (tileRect clamping) ─────────────────────────

test("tile at col=framePixelW/tileW (x==framePixelW) produces no action and is not applied", () => {
    // Frame 1920 wide, tileW=128.  col=15 → x=1920 == framePixelW → w=0.
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 1))
    const actions = state.apply(makeTile(1, 15, 0))  // col 15 → x=1920
    assert.equal(actions.length, 0, "zero-width tile must produce no action")
    assert.equal(state.appliedCount, 0, "zero-width tile must not enter applied set")
})

test("tile at col beyond grid (col=16) produces no action", () => {
    // col=16 → x=2048 > 1920 → w=max(0, 1920-2048)=0.
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 1))
    const actions = state.apply(makeTile(1, 16, 0))
    assert.equal(actions.length, 0, "out-of-bounds tile must produce no action")
    assert.equal(state.appliedCount, 0)
})

// ── isApplied: invalidate-during-decode guard ─────────────────────────────

test("isApplied returns true after TILE, false after INVALIDATE (pins drawTileAsync guard)", () => {
    // Pins the invariant drawTileAsync's isApplied() check relies on: after
    // QU_INVALIDATE clears the key, a decode that completed after the async
    // yield must discard its bitmap rather than repaint the cleared region.
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 3))
    state.apply(makeTile(3, 2, 1))
    assert.equal(state.isApplied(2, 1), true, "tile must be applied after TILE msg")

    state.apply(makeInvalidate(3, [{ col: 2, row: 1 }]))
    assert.equal(state.isApplied(2, 1), false,
        "tile must no longer be applied after INVALIDATE (drawTileAsync must not draw)")
})

test("isApplied returns false after revoke", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    state.apply(makeTile(7, 0, 0))
    assert.equal(state.isApplied(0, 0), true)
    state.revoke(0, 0)
    assert.equal(state.isApplied(0, 0), false)
})

// ── Re-config resets applied set ─────────────────────────────────────────

test("second QU_CONFIG clears applied tiles", () => {
    const state = new QuOverlayState(1920, 1080)
    state.apply(makeConfig(128, 128, 15, 9, 7))
    state.apply(makeTile(7, 0, 0))
    state.apply(makeTile(7, 1, 0))
    assert.equal(state.appliedCount, 2)
    // New epoch via QU_CONFIG
    state.apply(makeConfig(128, 128, 15, 9, 8))
    assert.equal(state.appliedCount, 0)
    assert.equal(state.currentEpoch, 8)
})
