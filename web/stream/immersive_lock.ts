// Immersive mode (M4, unified-app-architecture.md D-list): pure
// pointer-lock-wanted resolution shared by ViewerApp.toggleImmersive() and
// the click re-arm logic in onMouseButtonDown. DOM-free on purpose (see
// mouse_mode.ts) so it can be node-tested directly without pulling in
// stream/input.ts's window/document-touching dependency chain.
//
// Mirrors the M4 cursor P1 rule that "auto" never rewrites mouseMode itself
// (see ViewerApp.requestPointerLock): a literal "relative" mouseMode always
// wants the lock; "auto" only wants it while the host-authority
// CursorAutoMode last reported locked (mirrored in autoModeWantsLock).
import { MouseMode } from "./mouse_mode.js"

export function wantsPointerLock(mouseMode: MouseMode, autoModeWantsLock: boolean): boolean {
    return mouseMode === "relative" || (mouseMode === "auto" && autoModeWantsLock)
}
