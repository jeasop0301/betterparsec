// Mouse-mode vocabulary + the host-authority "auto" resolver — M4 cursor
// P1 (docs/design/cursor-channel.md §3). DOM-free on purpose: node tests
// (tests/cursor_resolve.test.mjs) import this module directly, so it must
// never grow a dependency chain that touches window/document at load time
// (stream/input.ts pulls the notification/resources chain and cannot be
// imported under plain `node --test`).

export type MouseMode = "relative" | "follow" | "localCursor" | "pointAndDrag" | "auto"

// "auto" is never sent on the wire and never reaches the input branches
// directly: it resolves to the effective mode the host cursor visibility
// currently implies (locked → relative, unlocked → follow).
export function resolveMouseMode(mode: MouseMode, autoLocked: boolean): MouseMode {
    if (mode === "auto") {
        return autoLocked ? "relative" : "follow"
    }
    return mode
}
