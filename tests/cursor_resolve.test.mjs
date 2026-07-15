import assert from "node:assert/strict"
import test from "node:test"

import { resolveMouseMode } from "../dist/stream/mouse_mode.js"

// ── M4 cursor P1: auto mouse mode resolver (docs/design/cursor-channel.md §3) ──

test("auto + locked resolves to relative", () => {
    assert.equal(resolveMouseMode("auto", true), "relative")
})

test("auto + unlocked resolves to follow", () => {
    assert.equal(resolveMouseMode("auto", false), "follow")
})

test("non-auto modes pass through regardless of autoLocked", () => {
    for (const mode of ["relative", "follow", "localCursor", "pointAndDrag"]) {
        assert.equal(resolveMouseMode(mode, true), mode)
        assert.equal(resolveMouseMode(mode, false), mode)
    }
})
