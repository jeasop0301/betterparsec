//! Native QU (build-to-lossless) tile composition (G022). The client side of the
//! `video_qu` channel: it holds the lossless tiles for the current epoch,
//! composited over a matching base video frame. Tiles from a stale epoch are
//! dropped, every tile is CRC32-self-verified against its declared checksum,
//! invalidations remove tiles, an epoch change clears everything, and the tile
//! cache is byte-bounded so a runaway desktop cannot exhaust memory.
//!
//! Foundation module: the host lossless-tile source, the live `video_qu`
//! decode, and the D3D11 present composition over the base frame are the
//! remaining (partly host-side / physically-gated) part of G022; the streamer
//! relay (qu_relay) and web overlay already exist. This composition state
//! machine is pure and headless unit-tested; `#![allow(dead_code)]` matches the
//! repo's deferred-wire-in pattern. The wire types live in
//! `streamer/src/transport/webrtc/qu_wire.rs`.
#![allow(dead_code)]

use std::collections::HashMap;

/// IEEE (reflected, 0xEDB88320) CRC-32 — the same checksum the QU tile carries
/// over its decoded BGRA. Standard check value: `crc32(b"123456789")` =
/// `0xCBF43926`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// The result of applying one tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileOutcome {
    /// The tile was accepted and stored for the current epoch.
    Applied,
    /// The tile belongs to a different (stale or future) epoch — dropped.
    StaleEpoch,
    /// The tile position is outside the configured grid — dropped.
    OutOfGrid,
    /// The tile's decoded bytes do not match its declared CRC — dropped.
    CrcMismatch,
    /// Storing the tile would exceed the byte budget — dropped.
    OverBudget,
}

/// One stored tile: its decoded pixels (kept only for composition tests /
/// present) — real code hands the texture to D3D11, but the byte length is what
/// bounds memory here.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StoredTile {
    bytes: usize,
    crc: u32,
}

/// Client-side QU composition state for one epoch.
#[derive(Debug, Clone)]
pub struct QuComposition {
    tile_w: u16,
    tile_h: u16,
    grid_cols: u16,
    grid_rows: u16,
    epoch: u32,
    max_bytes: usize,
    used_bytes: usize,
    tiles: HashMap<(u16, u16), StoredTile>,
}

impl QuComposition {
    /// A fresh composition for the given grid/epoch with a byte budget.
    pub fn new(
        tile_w: u16,
        tile_h: u16,
        grid_cols: u16,
        grid_rows: u16,
        epoch: u32,
        max_bytes: usize,
    ) -> Self {
        Self {
            tile_w,
            tile_h,
            grid_cols,
            grid_rows,
            epoch,
            max_bytes,
            used_bytes: 0,
            tiles: HashMap::new(),
        }
    }

    /// The current epoch. A base video frame must match this for the tiles to be
    /// composited over it.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Number of tiles currently held.
    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// Total decoded bytes currently held (never exceeds the budget).
    pub fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    /// Whether a tile is held at the given grid position.
    pub fn has_tile(&self, col: u16, row: u16) -> bool {
        self.tiles.contains_key(&(col, row))
    }

    /// Apply a decoded tile. `decoded` is the tile's BGRA bytes; `crc` is the
    /// declared CRC32. The tile is validated for epoch, grid bounds, CRC, and
    /// byte budget before being stored (replacing any tile at that position).
    pub fn apply_tile(
        &mut self,
        epoch: u32,
        col: u16,
        row: u16,
        decoded: &[u8],
        crc: u32,
    ) -> TileOutcome {
        if epoch != self.epoch {
            return TileOutcome::StaleEpoch;
        }
        if col >= self.grid_cols || row >= self.grid_rows {
            return TileOutcome::OutOfGrid;
        }
        if crc32(decoded) != crc {
            return TileOutcome::CrcMismatch;
        }
        let old = self.tiles.get(&(col, row)).map_or(0, |t| t.bytes);
        let projected = self.used_bytes - old + decoded.len();
        if projected > self.max_bytes {
            return TileOutcome::OverBudget;
        }
        self.used_bytes = projected;
        self.tiles.insert(
            (col, row),
            StoredTile {
                bytes: decoded.len(),
                crc,
            },
        );
        TileOutcome::Applied
    }

    /// Invalidate (remove) tiles for `epoch`. Returns `false` (ignored) when the
    /// epoch does not match the current one.
    pub fn apply_invalidate(&mut self, epoch: u32, tiles: &[(u16, u16)]) -> bool {
        if epoch != self.epoch {
            return false;
        }
        for &pos in tiles {
            if let Some(t) = self.tiles.remove(&pos) {
                self.used_bytes -= t.bytes;
            }
        }
        true
    }

    /// Advance to a new epoch: every tile is cleared (a new base is coming).
    pub fn set_epoch(&mut self, new_epoch: u32) {
        self.epoch = new_epoch;
        self.tiles.clear();
        self.used_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(fill: u8, len: usize) -> Vec<u8> {
        vec![fill; len]
    }

    #[test]
    fn crc32_standard_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn applies_a_valid_tile_and_tracks_bytes() {
        let mut qu = QuComposition::new(64, 64, 4, 4, 7, 4096);
        let px = tile(0xAB, 100);
        assert_eq!(
            qu.apply_tile(7, 1, 2, &px, crc32(&px)),
            TileOutcome::Applied
        );
        assert!(qu.has_tile(1, 2));
        assert_eq!(qu.tile_count(), 1);
        assert_eq!(qu.used_bytes(), 100);
        // Replacing the same slot does not double-count bytes.
        let px2 = tile(0xCD, 60);
        assert_eq!(
            qu.apply_tile(7, 1, 2, &px2, crc32(&px2)),
            TileOutcome::Applied
        );
        assert_eq!(qu.tile_count(), 1);
        assert_eq!(qu.used_bytes(), 60);
    }

    #[test]
    fn rejects_stale_epoch_out_of_grid_and_bad_crc() {
        let mut qu = QuComposition::new(64, 64, 4, 4, 7, 4096);
        let px = tile(0x11, 50);
        assert_eq!(
            qu.apply_tile(6, 0, 0, &px, crc32(&px)),
            TileOutcome::StaleEpoch
        );
        assert_eq!(
            qu.apply_tile(8, 0, 0, &px, crc32(&px)),
            TileOutcome::StaleEpoch
        );
        assert_eq!(
            qu.apply_tile(7, 4, 0, &px, crc32(&px)),
            TileOutcome::OutOfGrid
        );
        assert_eq!(
            qu.apply_tile(7, 0, 4, &px, crc32(&px)),
            TileOutcome::OutOfGrid
        );
        // Wrong declared CRC.
        assert_eq!(
            qu.apply_tile(7, 0, 0, &px, crc32(&px) ^ 1),
            TileOutcome::CrcMismatch
        );
        assert_eq!(qu.tile_count(), 0);
    }

    #[test]
    fn enforces_the_byte_budget() {
        let mut qu = QuComposition::new(64, 64, 4, 4, 1, 150);
        let a = tile(1, 100);
        assert_eq!(qu.apply_tile(1, 0, 0, &a, crc32(&a)), TileOutcome::Applied);
        let b = tile(2, 100);
        // 100 + 100 = 200 > 150 budget -> rejected, state unchanged.
        assert_eq!(
            qu.apply_tile(1, 1, 0, &b, crc32(&b)),
            TileOutcome::OverBudget
        );
        assert_eq!(qu.tile_count(), 1);
        assert_eq!(qu.used_bytes(), 100);
        // A 50-byte tile fits (100 + 50 = 150).
        let c = tile(3, 50);
        assert_eq!(qu.apply_tile(1, 1, 0, &c, crc32(&c)), TileOutcome::Applied);
        assert_eq!(qu.used_bytes(), 150);
    }

    #[test]
    fn invalidate_removes_tiles_only_for_the_current_epoch() {
        let mut qu = QuComposition::new(64, 64, 4, 4, 7, 4096);
        let px = tile(9, 40);
        qu.apply_tile(7, 0, 0, &px, crc32(&px));
        qu.apply_tile(7, 1, 1, &px, crc32(&px));
        assert_eq!(qu.used_bytes(), 80);
        // Wrong epoch is ignored.
        assert!(!qu.apply_invalidate(6, &[(0, 0)]));
        assert_eq!(qu.tile_count(), 2);
        // Current epoch removes the listed tile.
        assert!(qu.apply_invalidate(7, &[(0, 0)]));
        assert!(!qu.has_tile(0, 0));
        assert!(qu.has_tile(1, 1));
        assert_eq!(qu.used_bytes(), 40);
    }

    #[test]
    fn epoch_change_clears_all_tiles() {
        let mut qu = QuComposition::new(64, 64, 4, 4, 7, 4096);
        let px = tile(5, 30);
        qu.apply_tile(7, 0, 0, &px, crc32(&px));
        qu.apply_tile(7, 2, 3, &px, crc32(&px));
        assert_eq!(qu.tile_count(), 2);
        qu.set_epoch(8);
        assert_eq!(qu.epoch(), 8);
        assert_eq!(qu.tile_count(), 0);
        assert_eq!(qu.used_bytes(), 0);
        // Old-epoch tiles are now stale.
        assert_eq!(
            qu.apply_tile(7, 0, 0, &px, crc32(&px)),
            TileOutcome::StaleEpoch
        );
        assert_eq!(
            qu.apply_tile(8, 0, 0, &px, crc32(&px)),
            TileOutcome::Applied
        );
    }
}
