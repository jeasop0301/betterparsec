// QU overlay manager (U4 P1).
// Split into pure state (QuOverlayState, node-testable) and thin DOM layer (QuOverlayDom).
// See docs/design/qu-protocol.md §4 for the composite strategy.

import type { QuMessage, QuTileMsg } from "./qu_wire.js"
import { crc32 } from "./crc32.js"

// ── Action type ───────────────────────────────────────────────────────────

export type Action =
    | { type: 'drawTile'; col: number; row: number; x: number; y: number; w: number; h: number; payload: Uint8Array; crc32Bgra: number }
    | { type: 'clearRect'; x: number; y: number; w: number; h: number }
    | { type: 'clearAll' }
    | { type: 'resize'; pixelW: number; pixelH: number }

// ── Pure state ────────────────────────────────────────────────────────────

function tileKey(col: number, row: number): string {
    return `${col},${row}`
}

/**
 * QuOverlayState: pure, DOM-free, node-testable.
 *
 * Holds current epoch, tile grid config, and the set of applied tiles.
 * apply(msg) returns Actions to execute on the canvas; no side effects.
 *
 * Stale-epoch tiles (TILE or INVALIDATE with epoch != current) produce no actions.
 * Tile replacement (TILE for an already-applied position) emits a plain drawTile —
 * no pre-clear needed because drawImage fully overwrites the pixel region.
 * CRC verification is intentionally NOT done here (requires pixel decode); the DOM
 * layer calls revoke(col, row) to clear a tile whose CRC fails after decode.
 *
 * Edge tiles are clipped to [framePixelW × framePixelH] supplied at construction.
 */
export class QuOverlayState {
    private epoch: number | null = null
    private tileW = 0
    private tileH = 0
    // Set of tile keys that have been successfully drawn (not yet invalidated/revoked).
    private applied = new Map<string, true>()

    /**
     * @param framePixelW  Actual stream pixel width (from StreamSettings / ConnectionComplete).
     * @param framePixelH  Actual stream pixel height.
     */
    constructor(
        readonly framePixelW: number,
        readonly framePixelH: number,
    ) {}

    /**
     * Process one incoming QU message and return the resulting Action list.
     * The caller (DOM layer) executes these actions on the canvas.
     */
    apply(msg: QuMessage): Action[] {
        switch (msg.kind) {
            case 0x01: { // QU_CONFIG — reset grid + epoch
                this.epoch = msg.epoch
                this.tileW = msg.tileW
                this.tileH = msg.tileH
                this.applied.clear()
                return [
                    { type: 'resize', pixelW: this.framePixelW, pixelH: this.framePixelH },
                    { type: 'clearAll' },
                ]
            }
            case 0x04: { // QU_EPOCH — clear overlay, advance epoch
                this.epoch = msg.epoch
                this.applied.clear()
                return [{ type: 'clearAll' }]
            }
            case 0x02: { // QU_TILE
                if (this.epoch === null || msg.epoch !== this.epoch) return []
                // Guard: out-of-bounds columns/rows produce a zero or negative
                // clipped dimension.  Skip them entirely — do not mark as applied
                // and do not return a drawTile action (which would throw in DOM).
                const { w, h } = this.tileRect(msg.col, msg.row)
                if (w <= 0 || h <= 0) return []
                const key = tileKey(msg.col, msg.row)
                this.applied.set(key, true)
                return [this.tileAction(msg)]
            }
            case 0x03: { // QU_INVALIDATE
                if (this.epoch === null || msg.epoch !== this.epoch) return []
                const actions: Action[] = []
                for (const { col, row } of msg.tiles) {
                    const key = tileKey(col, row)
                    if (this.applied.has(key)) {
                        this.applied.delete(key)
                        actions.push(this.clearRectFor(col, row))
                    }
                }
                return actions
            }
            default:
                return []
        }
    }

    /**
     * Called by the DOM layer after a decode failure or CRC mismatch.
     * Removes the tile from the applied set so future INVALIDATEs skip it.
     * The DOM layer is responsible for clearing the canvas rect.
     */
    revoke(col: number, row: number): void {
        this.applied.delete(tileKey(col, row))
    }

    /**
     * Returns true if the tile is still in the applied set.
     *
     * Used by the DOM layer to guard the final drawImage call after an async
     * createImageBitmap yield: a QU_INVALIDATE may have cleared the tile from
     * the applied set while the PNG was decoding, in which case the completed
     * bitmap must be discarded rather than painted over the cleared region.
     */
    isApplied(col: number, row: number): boolean {
        return this.applied.has(tileKey(col, row))
    }

    get currentEpoch(): number | null { return this.epoch }
    get appliedCount(): number { return this.applied.size }

    // ── Private helpers ───────────────────────────────────────────────────

    private tileAction(msg: QuTileMsg): Action {
        const { x, y, w, h } = this.tileRect(msg.col, msg.row)
        return { type: 'drawTile', col: msg.col, row: msg.row, x, y, w, h, payload: msg.payload, crc32Bgra: msg.crc32Bgra }
    }

    private clearRectFor(col: number, row: number): Action {
        const { x, y, w, h } = this.tileRect(col, row)
        return { type: 'clearRect', x, y, w, h }
    }

    private tileRect(col: number, row: number): { x: number; y: number; w: number; h: number } {
        const x = col * this.tileW
        const y = row * this.tileH
        // Edge tiles are clipped to the actual stream frame bounds.
        // Math.max(0, …) prevents negative dimensions for out-of-bounds col/row
        // values (which would otherwise cause OffscreenCanvas to throw and could
        // clear unintended regions via ctx.clearRect with a negative width).
        const w = Math.max(0, Math.min(this.tileW, this.framePixelW - x))
        const h = Math.max(0, Math.min(this.tileH, this.framePixelH - y))
        return { x, y, w, h }
    }
}

// ── DOM layer ─────────────────────────────────────────────────────────────

/**
 * QuOverlayDom: thin canvas manager.
 *
 * Owns a <canvas> inserted as a sibling above the renderer element
 * (position:absolute, pointer-events:none, z-index:1).
 * Backing store = stream pixel size; CSS size synced to the renderer element
 * via ResizeObserver.
 *
 * SDR/color caveat: the overlay is an SDR 8-bit path. For HDR streams
 * (QU_CONFIG is not sent by the host in HDR sessions), this class should
 * not be used. The createImageBitmap → getImageData path decodes to sRGB
 * regardless of the video element's color space, so color-managed displays
 * may show a slight shift at tile boundaries on HDR-capable panels even in
 * SDR mode. This is acceptable for v1 (text/UI sharpness improvement
 * dominates; color fringing on 4:2:0 video is the baseline being replaced).
 */
export class QuOverlayDom {
    private readonly canvas: HTMLCanvasElement
    private readonly ctx: CanvasRenderingContext2D
    private readonly state: QuOverlayState
    private readonly observer: ResizeObserver
    private _quCrcMismatch = 0

    /**
     * @param rendererEl   The <video> or <canvas> element to observe for size.
     *                     The overlay canvas is inserted as its next sibling.
     * @param parentEl     Parent element (Stream.divElement); must already contain rendererEl.
     * @param framePixelW  Backing-store width (stream pixel width).
     * @param framePixelH  Backing-store height (stream pixel height).
     */
    constructor(
        private readonly rendererEl: HTMLElement,
        parentEl: HTMLElement,
        framePixelW: number,
        framePixelH: number,
    ) {
        this.state = new QuOverlayState(framePixelW, framePixelH)

        this.canvas = document.createElement('canvas')
        this.canvas.width = framePixelW
        this.canvas.height = framePixelH
        this.canvas.style.cssText =
            'position:absolute;top:0;left:0;pointer-events:none;z-index:1;'

        // Insert the canvas after rendererEl so it stacks above it.
        const next = rendererEl.nextSibling
        if (next) {
            parentEl.insertBefore(this.canvas, next)
        } else {
            parentEl.appendChild(this.canvas)
        }

        const ctx = this.canvas.getContext('2d')
        if (!ctx) throw new Error('QuOverlayDom: could not get 2d context')
        this.ctx = ctx

        // Sync CSS size to the renderer element via ResizeObserver.
        this.observer = new ResizeObserver((entries) => {
            for (const entry of entries) {
                const { width, height } = entry.contentRect
                this.canvas.style.width = `${width}px`
                this.canvas.style.height = `${height}px`
                // Also sync top/left in case the renderer element is offset.
                const rRect = this.rendererEl.getBoundingClientRect()
                const pRect = (this.canvas.parentElement ?? parentEl).getBoundingClientRect()
                this.canvas.style.left = `${rRect.left - pRect.left}px`
                this.canvas.style.top = `${rRect.top - pRect.top}px`
            }
        })
        this.observer.observe(rendererEl)
    }

    /** Feed one raw QU message into the state machine and execute resulting actions. */
    apply(msg: QuMessage): void {
        const actions = this.state.apply(msg)
        for (const action of actions) {
            this.executeAction(action, msg)
        }
    }

    /** Number of tiles discarded due to CRC mismatch or PNG decode failure. */
    get quCrcMismatch(): number { return this._quCrcMismatch }

    /** Tear down the canvas and ResizeObserver. */
    dispose(): void {
        this.observer.disconnect()
        this.canvas.remove()
    }

    // ── Action executor ───────────────────────────────────────────────────

    private executeAction(action: Action, _srcMsg: QuMessage): void {
        switch (action.type) {
            case 'clearAll':
                this.ctx.clearRect(0, 0, this.canvas.width, this.canvas.height)
                break
            case 'clearRect':
                this.ctx.clearRect(action.x, action.y, action.w, action.h)
                break
            case 'resize':
                // Backing store size is fixed at construction (stream pixel dims).
                // The resize action confirms the epoch reset; no canvas resize needed
                // because we already sized the canvas at construction. A future
                // resolution change would create a new QuOverlayDom instance.
                this.canvas.width = action.pixelW
                this.canvas.height = action.pixelH
                break
            case 'drawTile':
                // Async decode + CRC check; we fire-and-forget but capture errors.
                this.drawTileAsync(action).catch(() => {
                    // Decode failure: clear the optimistically-applied tile.
                    this.state.revoke(action.col, action.row)
                    this.ctx.clearRect(action.x, action.y, action.w, action.h)
                    this._quCrcMismatch++
                })
                break
        }
    }

    private async drawTileAsync(action: Extract<Action, { type: 'drawTile' }>): Promise<void> {
        // 1. Decode the PNG payload using createImageBitmap (native browser decode).
        const blob = new Blob([action.payload], { type: 'image/png' })
        let bitmap: ImageBitmap
        try {
            bitmap = await createImageBitmap(blob)
        } catch {
            // Decode failure: revoke + clear.
            this.state.revoke(action.col, action.row)
            this.ctx.clearRect(action.x, action.y, action.w, action.h)
            this._quCrcMismatch++
            return
        }

        // 2. Extract BGRA pixel data via an OffscreenCanvas.
        //    canvas getImageData returns RGBA; CRC is over BGRA so we swap R/B.
        //    SDR caveat: sRGB decode — see class JSDoc for HDR notes.
        const off = new OffscreenCanvas(action.w, action.h)
        const ctx2 = off.getContext('2d')!
        ctx2.drawImage(bitmap, 0, 0)
        const imageData = ctx2.getImageData(0, 0, action.w, action.h)
        const rgba = imageData.data // Uint8ClampedArray, RGBA
        const bgra = new Uint8Array(rgba.length)
        for (let i = 0; i < rgba.length; i += 4) {
            bgra[i]   = rgba[i + 2] // B
            bgra[i+1] = rgba[i + 1] // G
            bgra[i+2] = rgba[i]     // R
            bgra[i+3] = rgba[i + 3] // A
        }
        const actualCrc = crc32(bgra)

        // 3. Validate CRC.
        if (actualCrc !== action.crc32Bgra) {
            this.state.revoke(action.col, action.row)
            this.ctx.clearRect(action.x, action.y, action.w, action.h)
            this._quCrcMismatch++
            // Spec §2: log on CRC mismatch so systematic divergence is visible.
            const pad8 = (n: number) => ('00000000' + (n >>> 0).toString(16)).slice(-8)
            console.warn(
                `[QuOverlay] CRC mismatch at (${action.col},${action.row}): ` +
                `expected 0x${pad8(action.crc32Bgra)}, ` +
                `actual 0x${pad8(actualCrc)}`
            )
            bitmap.close()
            return
        }

        // 4. Guard: the tile may have been invalidated while createImageBitmap
        // was pending (async yield at step 1).  If so, the QU_INVALIDATE handler
        // already cleared the canvas region and removed the key from applied.
        // Painting the now-stale bitmap would repaint that cleared area, and
        // future QU_INVALIDATEs for the same position would miss the applied
        // guard and skip the clearRect — leaving a permanently stuck tile.
        if (!this.state.isApplied(action.col, action.row)) {
            bitmap.close()
            return
        }

        // 5. CRC OK + tile still applied — draw the decoded tile onto the overlay canvas.
        this.ctx.drawImage(bitmap, action.x, action.y)
        bitmap.close()
    }
}
